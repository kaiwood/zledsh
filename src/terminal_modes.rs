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
