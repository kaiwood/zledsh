use std::io;
use std::time::Instant;

use crate::process::{Outcome, Pipeline, Stage};

use crate::terminal_modes::{TTY, attributes, give_terminal, set_attributes};

pub(crate) struct Foreground {
    shell_group: Option<libc::pid_t>,
    jobs: Vec<Job>,
    next: usize,
    retained_statuses: usize,
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
            retained_statuses: usize::try_from(unsafe { libc::sysconf(libc::_SC_CHILD_MAX) })
                .ok()
                .filter(|&limit| limit > 0)
                .unwrap_or(128),
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
                    None => job.stopped = false,
                    Some(Outcome::TimedOut) => unreachable!(),
                }
            }
        }
        // Preserve a bounded set of completed statuses for a later wait.
        while self.jobs.iter().filter(|job| job.done.is_some()).count() > self.retained_statuses {
            let first_done = self.jobs.iter().position(|job| job.done.is_some()).unwrap();
            self.jobs.remove(first_done);
        }
        Ok(())
    }

    pub fn run_background(&mut self, stages: Vec<Stage>, pipefail: bool) -> io::Result<u8> {
        self.refresh()?;
        let (pipeline, gate) = Pipeline::start(stages, true, pipefail)?;
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
            notified: false,
            modes,
        });
        id
    }

    pub fn notify(&mut self) -> io::Result<()> {
        self.refresh()?;
        if self.has_terminal() {
            for job in &mut self.jobs {
                if let Some(code) = job.done.filter(|_| !job.notified) {
                    eprintln!(
                        "[{}] {} {}",
                        job.id,
                        done_state(code),
                        job.pipeline.last_pid()
                    );
                    job.notified = true;
                }
            }
        }
        Ok(())
    }

    pub fn listing(&mut self, selectors: &[String]) -> io::Result<String> {
        self.refresh()?;
        let mut indices = if selectors.is_empty() {
            (0..self.jobs.len())
                .filter(|&index| !self.jobs[index].notified)
                .collect()
        } else {
            selectors
                .iter()
                .map(|selector| self.select(Some(selector), true))
                .collect::<io::Result<Vec<_>>>()?
        };
        let mut output = String::new();
        for &index in &indices {
            let job = &self.jobs[index];
            let state = if let Some(code) = job.done {
                done_state(code)
            } else if job.stopped {
                "Stopped".into()
            } else {
                "Running".into()
            };
            output.push_str(&format!(
                "[{}] {state} {}\n",
                job.id,
                job.pipeline.last_pid()
            ));
        }
        indices.sort_unstable();
        indices.dedup();
        for index in indices.into_iter().rev() {
            if self.jobs[index].done.is_some() {
                self.jobs.remove(index);
            }
        }
        Ok(output)
    }

    fn select(&self, selector: Option<&str>, include_done: bool) -> io::Result<usize> {
        let eligible = |job: &Job| include_done || job.done.is_none();
        let index = match selector {
            None | Some("%" | "%+" | "%%" | "%-") => {
                // A suspended job takes precedence over a running job.
                let matching = |stopped| {
                    self.jobs
                        .iter()
                        .enumerate()
                        .rev()
                        .filter(move |(_, job)| job.done.is_none() && job.stopped == stopped)
                        .map(|(index, _)| index)
                };
                matching(true)
                    .chain(matching(false))
                    .nth(usize::from(selector == Some("%-")))
            }
            Some(value) => self.jobs.iter().rposition(|job| {
                eligible(job)
                    && if let Some(number) = value.strip_prefix('%') {
                        number.parse::<usize>() == Ok(job.id)
                    } else {
                        value.parse::<libc::pid_t>() == Ok(job.pipeline.last_pid())
                    }
            }),
        };
        index.ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no matching job"))
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

    pub fn wait_background(&mut self, selectors: &[String]) -> io::Result<u8> {
        self.refresh()?;
        if let Some(selectors) = selectors.strip_prefix(&[String::from("-n")]) {
            let candidates = if selectors.is_empty() {
                (0..self.jobs.len()).collect::<Vec<_>>()
            } else {
                selectors
                    .iter()
                    .filter_map(|selector| self.select(Some(selector), true).ok())
                    .collect()
            };
            return self.wait_next(&candidates);
        }
        let mut status = 0;
        if selectors.is_empty() {
            while !self.jobs.is_empty() {
                if let Err(signal) = self.wait_job(self.jobs.len() - 1)? {
                    return Ok(128 + signal);
                }
            }
        } else {
            for selector in selectors {
                match self.select(Some(selector), true) {
                    Ok(index) => match self.wait_job(index)? {
                        Ok(code) => status = code,
                        Err(signal) => return Ok(128 + signal),
                    },
                    Err(error) if error.kind() == io::ErrorKind::NotFound => status = 127,
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(status)
    }

    fn wait_next(&mut self, candidates: &[usize]) -> io::Result<u8> {
        if candidates.is_empty() {
            return Ok(127);
        }
        loop {
            if let Some(signal) = crate::traps::pending() {
                return Ok(128 + signal as u8);
            }
            for &index in candidates {
                let job = &mut self.jobs[index];
                let code = match job.done {
                    Some(code) => code,
                    None => match job.pipeline.poll()? {
                        Some(Outcome::Exited { code, .. }) => code,
                        _ => continue,
                    },
                };
                self.jobs.remove(index);
                return Ok(code);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    // Distinguish a trap interrupt from a child's signal-derived exit status.
    fn wait_job(&mut self, index: usize) -> io::Result<Result<u8, u8>> {
        let job = &mut self.jobs[index];
        let code = loop {
            if let Some(signal) = crate::traps::pending() {
                return Ok(Err(signal as u8));
            }
            if let Some(code) = job.done {
                break code;
            }
            if let Some(Outcome::Exited { code, .. }) = job.pipeline.poll()? {
                break code;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        self.jobs.remove(index);
        Ok(Ok(code))
    }

    fn terminal(&self) -> io::Result<ShellTerminal> {
        ShellTerminal::save(
            self.shell_group
                .ok_or_else(|| io::Error::other("no controlling terminal"))?,
        )
    }

    pub fn capture(
        &self,
        mut command: std::process::Command,
        limit: u64,
    ) -> io::Result<(Vec<u8>, u8)> {
        use std::{io::Read, os::fd::AsRawFd, os::unix::process::CommandExt};

        let terminal = self.terminal()?;
        let fd = crate::process::duplicate(TTY)?;
        command.process_group(0);
        unsafe {
            command.pre_exec(move || crate::terminal_modes::enter_child(fd.as_raw_fd()));
        }
        let mut child = command.spawn()?;
        let stdout = child.stdout.take().unwrap();
        let group = child.id() as libc::pid_t;
        let mut job = Pipeline::from_foreground_child(child);
        let (bytes, status) = std::thread::scope(|scope| -> io::Result<_> {
            let reader = scope.spawn(move || {
                let mut bytes = Vec::new();
                let result = stdout.take(limit + 1).read_to_end(&mut bytes);
                if result.is_err() || bytes.len() as u64 > limit {
                    unsafe { libc::kill(-group, libc::SIGKILL) };
                }
                result.map(|_| bytes)
            });
            let result = job.wait(None);
            let code = match result {
                Ok(Outcome::Exited { code, signaled }) => {
                    if signaled {
                        unsafe { libc::kill(-group, libc::SIGKILL) };
                    }
                    code
                }
                Ok(Outcome::Stopped(code)) => code,
                Ok(Outcome::TimedOut) => unreachable!(),
                Err(_) => 2,
            };
            drop(job);
            result?;
            Ok((
                reader
                    .join()
                    .map_err(|_| io::Error::other("capture worker failed"))??,
                code,
            ))
        })?;
        terminal.restore()?;
        Ok((bytes, status))
    }

    pub fn run_pipeline(
        &mut self,
        stages: Vec<Stage>,
        pipefail: bool,
        deadline: Option<Instant>,
    ) -> io::Result<u8> {
        let terminal = self.terminal()?;
        let (job, gate) = Pipeline::start(stages, true, pipefail)?;
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
                eprintln!("zledsh: shell agent command timed out");
                Ok(124)
            }
            Outcome::Stopped(code) if deadline.is_some() => {
                drop(job);
                terminal.restore()?;
                eprintln!("zledsh: shell agent command stopped and was cancelled");
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
    notified: bool,
}

impl Job {
    fn resume(&mut self) -> io::Result<()> {
        cvt(unsafe { libc::kill(-self.pipeline.group.unwrap(), libc::SIGCONT) })?;
        self.pipeline.resumed();
        self.stopped = false;
        Ok(())
    }
}

fn done_state(code: u8) -> String {
    if code == 0 {
        "Done".into()
    } else {
        format!("Done({code})")
    }
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}
