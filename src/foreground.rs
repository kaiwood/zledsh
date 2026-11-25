use std::io;
use std::process::Child;
use std::time::Instant;

use crate::process::{Outcome, Pipeline, Stage};

use crate::terminal_modes::{TTY, attributes, give_terminal, set_attributes};

pub(crate) struct Foreground {
    shell_group: Option<libc::pid_t>,
    jobs: Vec<Job>,
    next: usize,
    pub last_background: Option<libc::pid_t>,
}

impl Foreground {
    pub fn new() -> io::Result<Self> {
        let shell_group = unsafe { libc::getpgrp() };
        if cvt(unsafe { libc::tcgetpgrp(TTY) })? != shell_group {
            return Err(io::Error::other("shell must be in the foreground"));
        }
        Ok(Self {
            shell_group: Some(shell_group),
            ..Self::noninteractive()
        })
    }

    pub fn noninteractive() -> Self {
        Self {
            shell_group: None,
            jobs: Vec::new(),
            next: 1,
            last_background: None,
        }
    }

    pub fn has_terminal(&self) -> bool {
        self.shell_group.is_some()
    }

    pub fn has_live_jobs(&self) -> bool {
        self.jobs.iter().any(|job| job.done.is_none())
    }

    pub fn refresh(&mut self) -> io::Result<()> {
        for job in &mut self.jobs {
            if job.done.is_none() {
                match job.pipeline.poll()? {
                    Some(Outcome::Exited { code, .. }) => {
                        job.done = Some(code);
                        job.stopped = false;
                    }
                    Some(Outcome::Stopped(_)) => job.stopped = true,
                    _ => {}
                }
            }
        }
        // Preserve a bounded set of completed statuses for a later wait.
        while self.jobs.iter().filter(|job| job.done.is_some()).count() > 128 {
            let first_done = self.jobs.iter().position(|job| job.done.is_some()).unwrap();
            self.jobs.remove(first_done);
        }
        Ok(())
    }

    pub fn run_background(&mut self, stages: Vec<Stage>) -> io::Result<u8> {
        self.refresh()?;
        let (pipeline, gate) = Pipeline::start(stages, true)?;
        let pid = pipeline.last_pid();
        let id = self.add(pipeline, None);
        self.last_background = Some(pid);
        if self.has_terminal() {
            eprintln!("[{id}] {pid}");
        }
        drop(gate);
        Ok(0)
    }

    fn add(&mut self, pipeline: Pipeline, modes: Option<libc::termios>) -> usize {
        let id = self.next;
        self.next += 1;
        self.jobs.push(Job {
            id,
            pipeline,
            stopped: modes.is_some(),
            done: None,
            modes,
        });
        id
    }

    pub fn listing(&mut self) -> io::Result<String> {
        self.refresh()?;
        let mut output = String::new();
        for job in &self.jobs {
            let state = if job.done.is_some() {
                "Done"
            } else if job.stopped {
                "Stopped"
            } else {
                "Running"
            };
            output.push_str(&format!(
                "[{}] {state} {}\n",
                job.id,
                job.pipeline.last_pid()
            ));
        }
        Ok(output)
    }

    fn select(&self, selector: Option<&str>, include_done: bool) -> io::Result<usize> {
        self.jobs
            .iter()
            .rposition(|job| {
                (include_done || job.done.is_none())
                    && match selector {
                        None | Some("%+") => true,
                        Some(value) if value.starts_with('%') => {
                            value[1..].parse::<usize>() == Ok(job.id)
                        }
                        Some(value) => value.parse::<libc::pid_t>() == Ok(job.pipeline.last_pid()),
                    }
            })
            .ok_or_else(|| io::Error::other("no matching job"))
    }

    pub fn background(&mut self, selector: Option<&str>) -> io::Result<u8> {
        self.refresh()?;
        let index = self.select(selector, false)?;
        let job = &mut self.jobs[index];
        if job.stopped {
            job.resume()?;
        }
        Ok(0)
    }

    pub fn wait_background(&mut self, selector: Option<&str>) -> io::Result<u8> {
        self.refresh()?;
        if selector.is_some() {
            let index = self.select(selector, true)?;
            return self.wait_job(index);
        }
        for index in (0..self.jobs.len()).rev() {
            if !self.jobs[index].stopped {
                self.wait_job(index)?;
            }
        }
        Ok(0)
    }

    fn wait_job(&mut self, index: usize) -> io::Result<u8> {
        let job = &mut self.jobs[index];
        if let Some(code) = job.done {
            self.jobs.remove(index);
            return Ok(code);
        }
        match job.pipeline.wait(None)? {
            Outcome::Exited { code, .. } => {
                self.jobs.remove(index);
                Ok(code)
            }
            Outcome::Stopped(code) => {
                job.stopped = true;
                Ok(code)
            }
            Outcome::TimedOut => unreachable!(),
        }
    }

    fn terminal(&self) -> io::Result<ShellTerminal> {
        ShellTerminal::save(
            self.shell_group
                .ok_or_else(|| io::Error::other("no controlling terminal"))?,
        )
    }

    pub fn run(
        &mut self,
        deadline: Option<Instant>,
        spawn: impl FnOnce() -> io::Result<Child>,
    ) -> io::Result<u8> {
        let terminal = self.terminal()?;
        let child = spawn()?;
        self.wait(Pipeline::from_foreground_child(child), terminal, deadline)
    }

    pub fn run_pipeline(
        &mut self,
        stages: Vec<Stage>,
        deadline: Option<Instant>,
    ) -> io::Result<u8> {
        let terminal = self.terminal()?;
        let (job, gate) = Pipeline::start(stages, true)?;
        if let Err(error) = give_terminal(job.group.unwrap()) {
            drop(job);
            return Err(error);
        }
        drop(gate);
        self.wait(job, terminal, deadline)
    }

    pub fn resume(&mut self, selector: Option<&str>) -> io::Result<u8> {
        self.refresh()?;
        let terminal = self.terminal()?;
        let index = self.select(selector, false)?;
        let mut job = self.jobs.remove(index);
        if let Some(modes) = job.modes {
            set_attributes(&modes)?;
        }
        give_terminal(job.pipeline.group.unwrap())?;
        if job.stopped {
            job.resume()?;
        }
        self.wait(job.pipeline, terminal, None)
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
                self.add(job, Some(modes));
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

struct Job {
    id: usize,
    pipeline: Pipeline,
    modes: Option<libc::termios>,
    stopped: bool,
    done: Option<u8>,
}

impl Job {
    fn resume(&mut self) -> io::Result<()> {
        cvt(unsafe { libc::kill(-self.pipeline.group.unwrap(), libc::SIGCONT) })?;
        self.pipeline.resumed();
        self.stopped = false;
        Ok(())
    }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
