use std::io;
use std::process::Child;
use std::time::Instant;

use crate::process::{Outcome, Pipeline, Stage};

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

    pub fn run(
        &mut self,
        deadline: Option<Instant>,
        spawn: impl FnOnce() -> io::Result<Child>,
    ) -> io::Result<u8> {
        let terminal = ShellTerminal::save(self.shell_group)?;
        let child = spawn()?;
        self.wait(Pipeline::from_foreground_child(child), terminal, deadline)
    }

    pub fn run_pipeline(
        &mut self,
        stages: Vec<Stage>,
        deadline: Option<Instant>,
    ) -> io::Result<u8> {
        let terminal = ShellTerminal::save(self.shell_group)?;
        let (job, gate) = Pipeline::start(stages, true)?;
        if let Err(error) = give_terminal(job.group.unwrap()) {
            drop(job);
            return Err(error);
        }
        drop(gate);
        self.wait(job, terminal, deadline)
    }

    pub fn resume(&mut self) -> io::Result<u8> {
        let terminal = ShellTerminal::save(self.shell_group)?;
        let StoppedJob { mut job, modes } = self
            .stopped
            .pop()
            .ok_or_else(|| io::Error::other("no stopped job"))?;
        set_attributes(&modes)?;
        let group = job.group.unwrap();
        give_terminal(group)?;
        cvt(unsafe { libc::kill(-group, libc::SIGCONT) })?;
        job.resumed();
        self.wait(job, terminal, None)
    }

    fn wait(
        &mut self,
        mut job: Pipeline,
        terminal: ShellTerminal,
        deadline: Option<Instant>,
    ) -> io::Result<u8> {
        match job.wait(deadline)? {
            Outcome::TimedOut => {
                drop(job);
                terminal.restore()?;
                eprintln!("kwsh: shell agent command timed out");
                Ok(124)
            }
            Outcome::Stopped(code) if deadline.is_some() => {
                drop(job);
                terminal.restore()?;
                eprintln!("kwsh: shell agent command stopped and was cancelled");
                Ok(code)
            }
            Outcome::Stopped(code) => {
                let modes = attributes()?;
                terminal.restore()?;
                self.stopped.push(StoppedJob { job, modes });
                eprintln!("\nStopped");
                Ok(code)
            }
            Outcome::Exited { code, signaled } => {
                terminal.restore()?;
                if signaled {
                    eprintln!();
                }
                Ok(code)
            }
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
    job: Pipeline,
    modes: libc::termios,
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
