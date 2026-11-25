use std::io::{self, BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;

use crate::terminal_modes::{
    TTY, attributes, editor_attributes, enter_child, give_terminal, restore_attributes,
    set_attributes,
};

pub struct Terminal {
    shell: libc::pid_t,
    original: libc::termios,
    editor: libc::termios,
    old_ttou: libc::sighandler_t,
}

impl Terminal {
    pub fn enter() -> io::Result<Self> {
        let shell = unsafe { libc::getpgrp() };
        if cvt(unsafe { libc::tcgetpgrp(TTY) })? != shell {
            return Err(io::Error::other("spike must be in the foreground"));
        }
        let original = attributes()?;
        let editor = editor_attributes(original);
        // tcsetpgrp must also work while the shell is in the background.
        let old_ttou = unsafe { libc::signal(libc::SIGTTOU, libc::SIG_IGN) };
        if old_ttou == libc::SIG_ERR {
            return Err(io::Error::last_os_error());
        }
        let tty = Self {
            shell,
            original,
            editor,
            old_ttou,
        };
        set_attributes(&tty.editor)?;
        Ok(tty)
    }

    pub fn start(&self, executable: &Path, mode: &str) -> io::Result<Option<StoppedJob>> {
        set_attributes(&self.original)?;
        let mut command = Command::new(executable);
        command.arg(mode).process_group(0);
        // The child claims the tty before exec, so it cannot read it too early.
        // The closure runs after fork and uses only async-signal-safe calls.
        unsafe {
            command.pre_exec(|| enter_child(TTY));
        }
        let child = command.spawn()?;
        self.finish(Job(Some(child.id() as libc::pid_t)))
    }

    pub fn resume(&self, stopped: StoppedJob) -> io::Result<Option<StoppedJob>> {
        set_attributes(&stopped.modes)?;
        let group = stopped.job.pid();
        give_terminal(group)?;
        cvt(unsafe { libc::kill(-group, libc::SIGCONT) })?;
        self.finish(stopped.job)
    }

    fn finish(&self, mut job: Job) -> io::Result<Option<StoppedJob>> {
        let status = job.wait()?;
        give_terminal(self.shell)?;
        let stopped = if libc::WIFSTOPPED(status) {
            Some(StoppedJob {
                job,
                modes: attributes()?,
            })
        } else {
            None
        };
        set_attributes(&self.editor)?;
        if stopped.is_some() {
            println!("foreground job stopped");
        } else if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
            return Err(io::Error::other(format!(
                "foreground probe failed: wait status {status}"
            )));
        }
        Ok(stopped)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = give_terminal(self.shell);
        restore_attributes(&self.original);
        unsafe { libc::signal(libc::SIGTTOU, self.old_ttou) };
    }
}

pub struct StoppedJob {
    job: Job,
    modes: libc::termios,
}

struct Job(Option<libc::pid_t>);

impl Job {
    fn pid(&self) -> libc::pid_t {
        self.0.expect("job is still live")
    }

    fn wait(&mut self) -> io::Result<libc::c_int> {
        let mut status = 0;
        retry(|| unsafe { libc::waitpid(self.pid(), &mut status, libc::WUNTRACED) })?;
        if !libc::WIFSTOPPED(status) {
            self.0 = None;
        }
        Ok(status)
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        if let Some(pid) = self.0 {
            unsafe { libc::kill(-pid, libc::SIGKILL) };
            let _ = retry(|| unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) });
        }
    }
}

pub fn read_key() -> io::Result<Option<u8>> {
    let mut byte = 0u8;
    loop {
        let result = unsafe { libc::read(TTY, (&mut byte as *mut u8).cast(), 1) };
        match result {
            1 => return Ok(Some(byte)),
            0 => return Ok(None),
            _ => {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

pub fn foreground_probe() -> io::Result<()> {
    if cvt(unsafe { libc::tcgetpgrp(TTY) })? != unsafe { libc::getpgrp() } {
        return Err(io::Error::other("probe did not receive the foreground tty"));
    }
    let mut modes = attributes()?;
    if modes.c_lflag & (libc::ICANON | libc::ISIG) != libc::ICANON | libc::ISIG {
        return Err(io::Error::other("probe did not receive cooked tty modes"));
    }
    modes.c_lflag &= !libc::ECHO;
    set_attributes(&modes)?;
    println!("foreground ready");
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().lock().read_line(&mut input)?;
    if attributes()?.c_lflag & libc::ECHO != 0 {
        return Err(io::Error::other("stopped job tty modes were not restored"));
    }
    println!("foreground read: {}", input.trim_end());
    Ok(())
}

pub fn fd_probe() -> io::Result<()> {
    let marker = b"from foreground fd 9\n";
    let count = unsafe { libc::write(9, marker.as_ptr().cast(), marker.len()) };
    if count == marker.len() as libc::ssize_t {
        Ok(())
    } else if count == -1 {
        Err(io::Error::last_os_error())
    } else {
        Err(io::Error::new(
            io::ErrorKind::WriteZero,
            "short fd probe write",
        ))
    }
}

pub fn retry(mut call: impl FnMut() -> libc::c_int) -> io::Result<libc::c_int> {
    loop {
        match cvt(call()) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
