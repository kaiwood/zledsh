use std::io;
use std::process::Child;

use crate::terminal_modes::{TTY, attributes, give_terminal, set_attributes};

pub(crate) struct Foreground {
    shell_group: libc::pid_t,
    stopped: Vec<StoppedJob>,
}

impl Foreground {
    pub fn new() -> io::Result<Self> {
        let shell_group = unsafe { libc::getpgrp() };
        if cvt(unsafe { libc::tcgetpgrp(TTY) })? != shell_group {
            return Err(io::Error::other("shell must be in the foreground"));
        }
        Ok(Self {
            shell_group,
            stopped: Vec::new(),
        })
    }

    pub fn run(&mut self, spawn: impl FnOnce() -> io::Result<Child>) -> io::Result<u8> {
        let terminal = ShellTerminal::save(self.shell_group)?;
        let child = spawn()?;
        self.wait(Job(Some(child.id() as libc::pid_t)), terminal)
    }

    pub fn resume(&mut self) -> io::Result<u8> {
        let terminal = ShellTerminal::save(self.shell_group)?;
        let StoppedJob { job, modes } = self
            .stopped
            .pop()
            .ok_or_else(|| io::Error::other("no stopped job"))?;
        set_attributes(&modes)?;
        give_terminal(job.pid())?;
        cvt(unsafe { libc::kill(-job.pid(), libc::SIGCONT) })?;
        self.wait(job, terminal)
    }

    fn wait(&mut self, mut job: Job, terminal: ShellTerminal) -> io::Result<u8> {
        let status = job.wait()?;
        let stopped_modes = if libc::WIFSTOPPED(status) {
            Some(attributes()?)
        } else {
            None
        };
        terminal.restore()?;
        if let Some(modes) = stopped_modes {
            self.stopped.push(StoppedJob { job, modes });
            eprintln!("\nStopped");
            Ok(128 + libc::WSTOPSIG(status) as u8)
        } else if libc::WIFSIGNALED(status) {
            eprintln!();
            Ok(128 + libc::WTERMSIG(status) as u8)
        } else {
            Ok(libc::WEXITSTATUS(status) as u8)
        }
    }
}

struct ShellTerminal {
    group: libc::pid_t,
    modes: libc::termios,
    restored: bool,
}

impl ShellTerminal {
    fn save(group: libc::pid_t) -> io::Result<Self> {
        Ok(Self {
            group,
            modes: attributes()?,
            restored: false,
        })
    }

    fn restore(mut self) -> io::Result<()> {
        give_terminal(self.group)?;
        set_attributes(&self.modes)?;
        self.restored = true;
        Ok(())
    }
}

impl Drop for ShellTerminal {
    fn drop(&mut self) {
        if !self.restored && give_terminal(self.group).is_ok() {
            let _ = set_attributes(&self.modes);
        }
    }
}

struct StoppedJob {
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
        waitpid(self.pid(), &mut status, libc::WUNTRACED)?;
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
            let _ = waitpid(pid, std::ptr::null_mut(), 0);
        }
    }
}

fn waitpid(pid: libc::pid_t, status: *mut libc::c_int, flags: libc::c_int) -> io::Result<()> {
    loop {
        match cvt(unsafe { libc::waitpid(pid, status, flags) }) {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => return result.map(|_| ()),
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
