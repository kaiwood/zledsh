use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

// Writers take a shared lock while marking and emitting diagnostics. A
// nonblocking exclusive sample fails closed if any writer is still active,
// including a stopped worker. No diagnostic text is stored here.
pub(crate) fn write(path: Option<&Path>, emit: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let Some(path) = path else {
        return emit();
    };
    let marker = OpenOptions::new().append(true).open(path)?;
    with_lock(&marker, libc::LOCK_SH, || {
        (&marker).write_all(b"+")?;
        emit()
    })
}

pub(crate) fn generation(path: &Path) -> io::Result<u64> {
    let marker = File::open(path)?;
    with_lock(&marker, libc::LOCK_EX | libc::LOCK_NB, || {
        Ok(marker.metadata()?.len())
    })
}

fn with_lock<T>(
    file: &File,
    mode: libc::c_int,
    operation: impl FnOnce() -> io::Result<T>,
) -> io::Result<T> {
    lock(file, mode)?;
    let result = operation();
    // A concurrent fork can keep the open file description alive after close.
    let unlocked = lock(file, libc::LOCK_UN);
    result.and_then(|value| unlocked.map(|()| value))
}

fn lock(file: &File, mode: libc::c_int) -> io::Result<()> {
    loop {
        if unsafe { libc::flock(file.as_raw_fd(), mode) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_in_progress_and_completed_writes_and_keeps_later_captures_eligible() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let path = marker.path();
        assert_eq!(generation(path).unwrap(), 0);
        write(Some(path), || {
            assert_eq!(
                generation(path).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            Ok(())
        })
        .unwrap();
        assert_eq!(generation(path).unwrap(), 1);
        assert_eq!(generation(path).unwrap(), 1);
    }

    #[test]
    fn another_reference_does_not_keep_a_completed_operation_locked() {
        let marker = tempfile::NamedTempFile::new().unwrap();
        let file = File::open(marker.path()).unwrap();
        let inherited = with_lock(&file, libc::LOCK_SH, || file.try_clone()).unwrap();
        drop(file);
        assert_eq!(generation(marker.path()).unwrap(), 0);
        drop(inherited);
    }
}
