use std::ffi::{CString, OsStr, OsString, c_char};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::process::{Child, ExitStatus};

pub(crate) struct Stdin;

impl Read for Stdin {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count =
            unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
}

pub(crate) fn null_stdin() -> io::Result<()> {
    let null = std::fs::File::open("/dev/null")?;
    if unsafe { libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(crate) fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, crate::SHELL_FDS as RawFd) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(copy) })
}

pub(crate) fn inherited_descriptor(fd: RawFd) -> bool {
    // Private descriptors can temporarily occupy unused public numbers, but
    // must never be made available as redirection sources.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    flags >= 0 && flags & libc::FD_CLOEXEC == 0
}

fn candidates(name: &str, path: &OsStr) -> impl Iterator<Item = PathBuf> {
    let explicit = name.contains('/');
    let name = name.to_owned();
    explicit.then(|| PathBuf::from(&name)).into_iter().chain(
        std::env::split_paths(path)
            .filter(move |_| !explicit)
            .map(move |dir| dir.join(&name)),
    )
}

pub(crate) fn find_executable(name: &str, path: &OsStr) -> Option<PathBuf> {
    candidates(name, path).find(|path| {
        path.is_file()
            && cstring(path.as_os_str()).is_ok_and(|path| unsafe {
                libc::faccessat(libc::AT_FDCWD, path.as_ptr(), libc::X_OK, libc::AT_EACCESS) == 0
            })
    })
}

pub(crate) struct Exec {
    candidates: Vec<CString>,
    args: ExecVector,
    env: ExecVector,
}

impl Exec {
    pub fn new(
        argv: &[String],
        path: &OsStr,
        environment: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> io::Result<Self> {
        let candidates = candidates(&argv[0], path)
            .map(|path| cstring(path.as_os_str()))
            .collect::<io::Result<Vec<_>>>()?;
        let args = ExecVector::new(argv.iter().map(|arg| cstring(arg.as_ref())))?;
        let env = ExecVector::new(environment.into_iter().map(|(mut key, value)| {
            key.push("=");
            key.push(value);
            cstring(&key)
        }))?;

        Ok(Self {
            candidates,
            args,
            env,
        })
    }

    // Called after fork; this must not allocate or lock.
    fn invoke(&self) -> io::Error {
        let mut error = libc::ENOENT;
        for candidate in &self.candidates {
            unsafe { libc::execve(candidate.as_ptr(), self.args.as_ptr(), self.env.as_ptr()) };
            match io::Error::last_os_error().raw_os_error() {
                Some(libc::ENOENT) => {
                    if unsafe { libc::access(candidate.as_ptr(), libc::F_OK) } == 0 {
                        error = libc::ENOEXEC;
                    }
                }
                Some(libc::ENOTDIR) => {}
                Some(libc::EACCES | libc::EPERM | libc::EISDIR) => error = libc::EACCES,
                Some(other) => return io::Error::from_raw_os_error(other),
                None => return io::Error::from_raw_os_error(libc::EIO),
            }
        }
        io::Error::from_raw_os_error(error)
    }
}

pub(crate) fn pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    if unsafe { libc::pipe(fds.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let [read, write] = fds.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) });
    // Keep pipe and launch-gate ends above the public range. Only explicitly
    // mapped ends survive exec, so readers see EOF when the last writer exits.
    Ok((duplicate(read.as_raw_fd())?, duplicate(write.as_raw_fd())?))
}

pub(crate) enum FdAction {
    Keep,
    Set(OwnedFd),
    Close,
}

pub(crate) struct Stage {
    pub exec: Exec,
    pub streams: [FdAction; crate::SHELL_FDS],
    pub worker_state: Option<OwnedFd>,
    pub missing: Vec<u8>,
    pub failed: Vec<u8>,
}

const CHILD_SIGNALS: [libc::c_int; 6] = [
    libc::SIGPIPE,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
];

pub(crate) fn replace(stage: &Stage) -> io::Error {
    let mut descriptors = Vec::new();
    for (fd, action) in stage.streams.iter().enumerate() {
        if matches!(action, FdAction::Keep) {
            continue;
        }
        let fd = fd as RawFd;
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        let saved = if flags >= 0 {
            match duplicate(fd) {
                Ok(copy) => Some(copy),
                Err(error) => return error,
            }
        } else if io::Error::last_os_error().raw_os_error() == Some(libc::EBADF) {
            None
        } else {
            return io::Error::last_os_error();
        };
        descriptors.push((fd, flags, saved, action));
    }
    let mut signals = Vec::new();
    let result = (|| -> io::Result<()> {
        for &(fd, _, _, action) in &descriptors {
            match action {
                FdAction::Set(source) => {
                    if unsafe { libc::dup2(source.as_raw_fd(), fd) } < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                FdAction::Close => {
                    unsafe { libc::close(fd) };
                }
                FdAction::Keep => unreachable!(),
            }
        }
        for signal in CHILD_SIGNALS {
            let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
            action.sa_sigaction = libc::SIG_DFL;
            let mut old = std::mem::MaybeUninit::uninit();
            if unsafe { libc::sigaction(signal, &action, old.as_mut_ptr()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            signals.push((signal, unsafe { old.assume_init() }));
        }
        Err(stage.exec.invoke())
    })();
    for (signal, old) in signals.into_iter().rev() {
        unsafe { libc::sigaction(signal, &old, std::ptr::null_mut()) };
    }
    for (fd, flags, saved, _) in descriptors {
        if let Some(saved) = saved {
            // None of these descriptors are handed to other shell code until
            // rollback finishes. Retry dup2 if a signal interrupted it.
            while unsafe { libc::dup2(saved.as_raw_fd(), fd) } < 0 {
                if io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                    break;
                }
            }
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags) };
        } else {
            unsafe { libc::close(fd) };
        }
    }
    result.unwrap_err()
}

pub(crate) fn fork_stage(
    stage: &Stage,
    gate: &(OwnedFd, OwnedFd),
    group: Option<libc::pid_t>,
) -> io::Result<libc::pid_t> {
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid > 0 {
        return Ok(pid);
    }
    // Only async-signal-safe functions in the child until exec/_exit. The
    // parent releases the gate after every process has joined the group.
    unsafe {
        libc::close(gate.1.as_raw_fd());
        if group.is_some_and(|group| libc::setpgid(0, group) < 0) {
            libc::_exit(126);
        }
        for signal in CHILD_SIGNALS {
            libc::signal(signal, libc::SIG_DFL);
        }
        for (fd, action) in stage.streams.iter().enumerate() {
            let fd = fd as RawFd;
            match action {
                FdAction::Set(source) => {
                    if libc::dup2(source.as_raw_fd(), fd) < 0 {
                        libc::_exit(126);
                    }
                }
                FdAction::Close => {
                    libc::close(fd);
                }
                FdAction::Keep => {}
            }
        }
        let mut byte = 0u8;
        loop {
            let count = libc::read(gate.0.as_raw_fd(), (&mut byte as *mut u8).cast(), 1);
            if count == 0 {
                break;
            }
            if count < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            libc::_exit(126);
        }
        // Only this worker inherits its state file; all other copies stay close-on-exec.
        if let Some(state) = &stage.worker_state {
            if libc::fcntl(state.as_raw_fd(), libc::F_SETFD, 0) < 0 {
                libc::_exit(126);
            }
        }
        let missing = stage.exec.invoke().raw_os_error() == Some(libc::ENOENT);
        let message = if missing {
            &stage.missing
        } else {
            &stage.failed
        };
        libc::write(libc::STDERR_FILENO, message.as_ptr().cast(), message.len());
        libc::_exit(if missing { 127 } else { 126 });
    }
}

pub(crate) fn wait_pid(
    pid: libc::pid_t,
    flags: libc::c_int,
) -> io::Result<(libc::pid_t, libc::c_int)> {
    let mut status = 0;
    loop {
        let waited = unsafe { libc::waitpid(pid, &mut status, flags) };
        if waited >= 0 {
            return Ok((waited, status));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

pub(crate) fn status_code(status: ExitStatus) -> u8 {
    status.code().map_or_else(
        || 128 + status.signal().unwrap_or(0) as u8,
        |code| code as u8,
    )
}

fn cstring(value: &OsStr) -> io::Result<CString> {
    CString::new(value.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "NUL in command or environment"))
}

struct ExecVector {
    _strings: Vec<CString>,
    pointers: Vec<*const c_char>,
}

// Pointers refer to owned, immutable CString allocations. Moving the vector
// does not move those allocations, and neither vector is exposed for mutation.
unsafe impl Send for ExecVector {}
unsafe impl Sync for ExecVector {}

impl ExecVector {
    fn new(strings: impl IntoIterator<Item = io::Result<CString>>) -> io::Result<Self> {
        let strings: Vec<_> = strings.into_iter().collect::<io::Result<_>>()?;
        let pointers = strings
            .iter()
            .map(|s| s.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Ok(Self {
            _strings: strings,
            pointers,
        })
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.pointers.as_ptr()
    }
}

pub(crate) struct Pipeline {
    pub group: Option<libc::pid_t>,
    children: Vec<(libc::pid_t, Member)>,
    pipefail: bool,
}

enum Member {
    Running,
    Stopped(u8),
    Done(libc::c_int),
}

pub(crate) enum Outcome {
    Stopped(u8),
    TimedOut,
    Exited { code: u8, signaled: bool },
}

impl Pipeline {
    pub fn from_foreground_child(child: Child) -> Self {
        let pid = child.id() as libc::pid_t;
        Self {
            group: Some(pid),
            children: vec![(pid, Member::Running)],
            pipefail: false,
        }
    }

    pub fn start(
        stages: Vec<Stage>,
        foreground: bool,
        pipefail: bool,
    ) -> io::Result<(Self, OwnedFd)> {
        let gate = pipe()?;
        let mut job = Self {
            group: None,
            children: Vec::new(),
            pipefail,
        };
        for stage in &stages {
            let group = foreground.then(|| job.group.unwrap_or(0));
            let pid = match fork_stage(stage, &gate, group) {
                Ok(pid) => pid,
                Err(error) => {
                    drop(job);
                    return Err(error);
                }
            };
            job.children.push((pid, Member::Running));
            if foreground {
                let group = *job.group.get_or_insert(pid);
                // Parent and child both set it so either can win the race.
                if unsafe { libc::setpgid(pid, group) } < 0 {
                    let error = io::Error::last_os_error();
                    drop(job);
                    return Err(error);
                }
            }
        }
        Ok((job, gate.1))
    }

    pub fn resumed(&mut self) {
        for (_, member) in &mut self.children {
            if matches!(member, Member::Stopped(_)) {
                *member = Member::Running;
            }
        }
    }

    pub fn wait(&mut self, deadline: Option<std::time::Instant>) -> io::Result<Outcome> {
        loop {
            if let Some(outcome) = self.collect(deadline.is_some())? {
                return Ok(outcome);
            }
            if deadline.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Ok(Outcome::TimedOut);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn poll(&mut self) -> io::Result<Option<Outcome>> {
        self.collect(true)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn last_pid(&self) -> libc::pid_t {
        self.children.last().unwrap().0
    }

    fn collect(&mut self, nonblocking: bool) -> io::Result<Option<Outcome>> {
        loop {
            let Some(&(pid, _)) = self
                .children
                .iter()
                .find(|(_, state)| matches!(state, Member::Running))
            else {
                if let Some(stop_signal) = self.children.iter().find_map(|(_, state)| match state {
                    Member::Stopped(signal) => Some(*signal),
                    _ => None,
                }) {
                    return Ok(Some(Outcome::Stopped(128 + stop_signal)));
                }
                // The option is fixed at launch, including for jobs collected later.
                let last = self.children.last();
                let status = if self.pipefail {
                    self.children
                        .iter()
                        .rev()
                        .find(|(_, member)| matches!(member, Member::Done(status) if *status != 0))
                        .or(last)
                } else {
                    last
                };
                let status = match status {
                    Some((_, Member::Done(status))) => ExitStatus::from_raw(*status),
                    _ => ExitStatus::from_raw(0),
                };
                return Ok(Some(Outcome::Exited {
                    code: status_code(status),
                    signaled: status.signal().is_some(),
                }));
            };
            let polling = if nonblocking { libc::WNOHANG } else { 0 };
            let (pid, status) = if let Some(group) = self.group {
                wait_pid(-group, libc::WUNTRACED | polling)?
            } else {
                wait_pid(pid, polling)?
            };
            if pid == 0 {
                return Ok(None);
            }
            if let Some((_, member)) = self.children.iter_mut().find(|(child, _)| *child == pid) {
                if libc::WIFSTOPPED(status) {
                    *member = Member::Stopped(libc::WSTOPSIG(status) as u8);
                } else {
                    *member = Member::Done(status);
                }
            }
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        let live = self
            .children
            .iter()
            .filter(|(_, state)| !matches!(state, Member::Done(_)));
        if let Some(group) = self.group.filter(|_| live.clone().next().is_some()) {
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        for &(pid, _) in live {
            unsafe { libc::kill(pid, libc::SIGKILL) };
            let _ = wait_pid(pid, 0);
        }
    }
}
