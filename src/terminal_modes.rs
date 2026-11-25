use std::{io, mem::MaybeUninit};

pub(crate) const TTY: libc::c_int = libc::STDIN_FILENO;

pub(crate) fn attributes() -> io::Result<libc::termios> {
    let mut modes = MaybeUninit::uninit();
    retry(|| unsafe { libc::tcgetattr(TTY, modes.as_mut_ptr()) })?;
    Ok(unsafe { modes.assume_init() })
}

pub(crate) fn editor_attributes(original: libc::termios) -> libc::termios {
    let mut editor = original;
    unsafe { libc::cfmakeraw(&mut editor) };
    editor.c_oflag = original.c_oflag;
    editor
}

pub(crate) fn set_attributes(modes: &libc::termios) -> io::Result<()> {
    retry(|| unsafe { libc::tcsetattr(TTY, libc::TCSANOW, modes) }).map(|_| ())
}

pub(crate) fn restore_attributes(original: &libc::termios) {
    // Discard editor typeahead instead of marking it pending in cooked mode.
    let _ = retry(|| unsafe { libc::tcsetattr(TTY, libc::TCSAFLUSH, original) });
}

fn retry(mut call: impl FnMut() -> libc::c_int) -> io::Result<()> {
    loop {
        if call() != -1 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

// The caller is the newly created process group leader, in pre_exec. All
// operations here and in give_terminal are async-signal-safe.
pub(crate) fn enter_child() -> io::Result<()> {
    for signal in [
        libc::SIGTTOU,
        libc::SIGTTIN,
        libc::SIGTSTP,
        libc::SIGINT,
        libc::SIGQUIT,
    ] {
        if unsafe { libc::signal(signal, libc::SIG_DFL) } == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
    }
    give_terminal(unsafe { libc::getpgrp() })
}

pub(crate) fn give_terminal(group: libc::pid_t) -> io::Result<()> {
    // A background group can reclaim or hand off the tty without stopping
    // itself. Keep SIGTTOU blocked only for this operation.
    let mut block = MaybeUninit::uninit();
    let mut old = MaybeUninit::uninit();
    unsafe {
        cvt(libc::sigemptyset(block.as_mut_ptr()))?;
        cvt(libc::sigaddset(block.as_mut_ptr(), libc::SIGTTOU))?;
        cvt(libc::sigprocmask(
            libc::SIG_BLOCK,
            block.as_ptr(),
            old.as_mut_ptr(),
        ))?;
        let result = cvt(libc::tcsetpgrp(TTY, group));
        let restore = cvt(libc::sigprocmask(
            libc::SIG_SETMASK,
            old.as_ptr(),
            std::ptr::null_mut(),
        ));
        result.and(restore).map(|_| ())
    }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
