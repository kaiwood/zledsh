use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

// Writers take a shared lock while marking and emitting diagnostics. A
// nonblocking exclusive sample fails closed if any writer is still active,
// including a stopped worker. No diagnostic text is stored here.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn write(path: Option<&Path>, emit: impl FnOnce() -> io::Result<()>) -> io::Result<()> {
    let Some(path) = path else {
        return emit();
    };
    let mut marker = OpenOptions::new().append(true).open(path)?;
    lock(&marker, libc::LOCK_SH)?;
    marker.write_all(b"+")?;
    emit()
}

pub(crate) fn generation(path: &Path) -> Option<u64> {
    let marker = File::open(path).ok()?;
    lock(&marker, libc::LOCK_EX | libc::LOCK_NB).ok()?;
    Some(marker.metadata().ok()?.len())
}

fn lock(file: &File, mode: libc::c_int) -> io::Result<()> {
    loop {
        // The lock is released when this independently opened file is closed.
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
        assert_eq!(generation(path), Some(0));
        write(Some(path), || {
            assert_eq!(generation(path), None);
            Ok(())
        })
        .unwrap();
        assert_eq!(generation(path), Some(1));
        assert_eq!(generation(path), Some(1));
    }
}
