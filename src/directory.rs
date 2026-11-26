use std::{
    io,
    path::{Component, Path, PathBuf},
};

pub(crate) fn normalize(path: &Path) -> io::Result<PathBuf> {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir if result.parent().is_some() => {
                if !result.metadata()?.is_dir() {
                    return Err(io::Error::from_raw_os_error(libc::ENOTDIR));
                }
                result.pop();
            }
            Component::ParentDir | Component::CurDir => {}
            _ => result.push(component),
        }
    }
    Ok(result)
}

pub(crate) fn logical(pwd: Option<&Path>) -> io::Result<PathBuf> {
    use std::os::unix::{ffi::OsStrExt, fs::MetadataExt};

    if let Some(path) = pwd.filter(|path| {
        path.is_absolute()
            && !path
                .as_os_str()
                .as_bytes()
                .split(|&b| b == b'/')
                .any(|part| matches!(part, b"." | b".."))
    }) {
        if let (Ok(known), Ok(current)) = (path.metadata(), Path::new(".").metadata()) {
            if known.dev() == current.dev() && known.ino() == current.ino() {
                return Ok(path.into());
            }
        }
    }
    std::env::current_dir()
}
