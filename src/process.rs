use std::ffi::{CString, OsStr, OsString, c_char};
use std::io::{self, Read};
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};

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

pub(crate) fn duplicate(fd: RawFd) -> io::Result<Stdio> {
    let copy = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copy < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Stdio::from(unsafe { OwnedFd::from_raw_fd(copy) }))
}

pub(crate) fn spawn(
    argv: &[String],
    path: &OsStr,
    environment: impl IntoIterator<Item = (OsString, OsString)>,
    stdout: Stdio,
    stderr: Stdio,
    foreground: Option<fn() -> io::Result<()>>,
) -> io::Result<Child> {
    let name = &argv[0];
    let candidates: Vec<_> = if name.contains('/') {
        vec![PathBuf::from(name)]
    } else {
        std::env::split_paths(path)
            .map(|dir| dir.join(name))
            .collect()
    };
    let candidates = candidates
        .iter()
        .map(|path| cstring(path.as_os_str()))
        .collect::<io::Result<Vec<_>>>()?;
    let args = ExecVector::new(argv.iter().map(|arg| cstring(arg.as_ref())))?;
    let env = ExecVector::new(environment.into_iter().map(|(mut key, value)| {
        key.push("=");
        key.push(value);
        cstring(&key)
    }))?;

    // Let std wire the descriptors and reap spawn failures. The hook always
    // execs or returns an error, so std's own execvp path is never reached.
    let mut command = Command::new("/");
    command.stdout(stdout).stderr(stderr);
    if foreground.is_some() {
        command.process_group(0);
    }
    unsafe {
        command.pre_exec(move || {
            if let Some(enter) = foreground {
                enter()?;
            }
            let mut error = libc::ENOENT;
            for candidate in &candidates {
                libc::execve(candidate.as_ptr(), args.as_ptr(), env.as_ptr());
                match io::Error::last_os_error().raw_os_error() {
                    Some(libc::ENOENT) => {
                        if libc::access(candidate.as_ptr(), libc::F_OK) == 0 {
                            error = libc::ENOEXEC;
                        }
                    }
                    Some(libc::ENOTDIR) => {}
                    Some(libc::EACCES | libc::EPERM | libc::EISDIR) => error = libc::EACCES,
                    Some(other) => return Err(io::Error::from_raw_os_error(other)),
                    None => return Err(io::Error::from_raw_os_error(libc::EIO)),
                }
            }
            Err(io::Error::from_raw_os_error(error))
        });
    }
    command.spawn()
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
