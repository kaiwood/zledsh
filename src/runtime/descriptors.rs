use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::rc::Rc;

use crate::process;

pub(super) type Descriptors = [Descriptor; crate::SHELL_FDS];

pub(super) fn inherited_descriptors() -> Descriptors {
    std::array::from_fn(|fd| Descriptor::Inherited(fd as RawFd))
}

pub(super) fn worker_descriptors(closed: u8) -> Descriptors {
    let mut descriptors = inherited_descriptors();
    for (fd, stream) in descriptors[..3].iter_mut().enumerate() {
        if closed & (1 << fd) != 0 {
            *stream = Descriptor::Closed;
        }
    }
    descriptors
}

pub(super) fn closed_stdio(streams: &Descriptors) -> u8 {
    streams[..3]
        .iter()
        .enumerate()
        .fold(0, |mask, (fd, stream)| {
            mask | if matches!(stream, Descriptor::Closed) {
                1 << fd
            } else {
                0
            }
        })
}

pub(super) fn stage(
    exec: process::Exec,
    name: &str,
    descriptors: &Descriptors,
) -> io::Result<process::Stage> {
    let mut streams = std::array::from_fn(|_| process::FdAction::Keep);
    for (fd, descriptor) in descriptors.iter().enumerate() {
        streams[fd] = match descriptor {
            Descriptor::Inherited(source) if *source == fd as RawFd => process::FdAction::Keep,
            Descriptor::Closed => process::FdAction::Close,
            _ => process::FdAction::Set(descriptor.duplicate()?),
        };
    }
    Ok(process::Stage {
        exec,
        streams,
        worker_state: None,
        missing: format!("zledsh: {name}: command not found\n").into_bytes(),
        failed: format!("zledsh: {name}: cannot execute\n").into_bytes(),
    })
}

#[derive(Clone)]
pub(super) enum Descriptor {
    Inherited(RawFd),
    File(Rc<File>),
    Closed,
}

impl Descriptor {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub(super) fn is_terminal(&self) -> bool {
        match self {
            Self::Inherited(fd) => unsafe { libc::isatty(*fd) == 1 },
            Self::File(file) => unsafe { libc::isatty(file.as_raw_fd()) == 1 },
            Self::Closed => false,
        }
    }

    pub(super) fn is_open(&self) -> bool {
        match self {
            Self::Inherited(fd) => process::inherited_descriptor(*fd),
            Self::File(_) => true,
            Self::Closed => false,
        }
    }

    pub(super) fn duplicate(&self) -> io::Result<OwnedFd> {
        match self {
            Self::Inherited(fd) => process::duplicate(*fd),
            Self::File(file) => process::duplicate(file.as_raw_fd()),
            Self::Closed => Err(io::Error::from_raw_os_error(libc::EBADF)),
        }
    }

    pub(super) fn write(&self, text: &str) -> io::Result<()> {
        self.write_bytes(text.as_bytes())
    }

    pub(super) fn write_bytes(&self, bytes: &[u8]) -> io::Result<()> {
        match self {
            Self::Inherited(1) => {
                let mut output = io::stdout().lock();
                output.write_all(bytes)?;
                output.flush()
            }
            Self::Inherited(2) => io::stderr().lock().write_all(bytes),
            Self::File(file) => file.as_ref().write_all(bytes),
            _ => File::from(self.duplicate()?).write_all(bytes),
        }
    }
}

pub(super) fn write_path(output: &Descriptor, path: &std::path::Path) -> io::Result<()> {
    let mut line = path.as_os_str().as_bytes().to_vec();
    line.push(b'\n');
    output.write_bytes(&line)
}

pub(super) fn read_from<T>(
    stream: &Descriptor,
    stdin: &mut dyn BufRead,
    read: impl FnOnce(&mut dyn BufRead) -> Result<T, String>,
) -> Result<T, String> {
    if matches!(stream, Descriptor::Inherited(0)) {
        return read(stdin);
    }
    let fd = stream.duplicate().map_err(|error| error.to_string())?;
    // Preserve unread bytes for the next builtin or external command.
    read(&mut BufReader::with_capacity(1, File::from(fd)))
}

pub(super) fn open_noclobber(target: &OsStr) -> io::Result<File> {
    let exists = match OpenOptions::new().write(true).create_new(true).open(target) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => error,
        result => return result,
    };
    if std::fs::metadata(target)?.is_file() {
        return Err(exists);
    }
    // Devices and FIFOs may be opened, but never truncate here. If the path
    // was replaced after metadata(), refuse to write to a regular file.
    let file = OpenOptions::new().write(true).open(target)?;
    if file.metadata()?.is_file() {
        return Err(exists);
    }
    Ok(file)
}

pub(super) fn here_input(text: &[u8], newline: bool) -> Result<Descriptor, String> {
    // An anonymous file avoids blocking before the reader starts.
    let mut file = tempfile::tempfile().map_err(here_document_error)?;
    file.write_all(text).map_err(here_document_error)?;
    if newline {
        file.write_all(b"\n").map_err(here_document_error)?;
    }
    file.rewind().map_err(here_document_error)?;
    Ok(Descriptor::File(Rc::new(file)))
}

fn here_document_error(error: io::Error) -> String {
    format!("here-document: {error}")
}
