use serde::Serialize;

#[derive(Serialize)]
pub(crate) struct ShellContext {
    cwd: Option<String>,
    last_command: Option<String>,
    last_output: Option<String>,
    exit_status: Option<u8>,
}

pub(crate) struct LastCommand {
    pub source: String,
    pub output: Option<String>,
    pub status: u8,
}

impl ShellContext {
    pub fn current(last: Option<&LastCommand>) -> Self {
        Self {
            cwd: std::env::current_dir()
                .ok()
                .map(|path| path.to_string_lossy().into_owned()),
            last_command: last
                .filter(|last| last.source.len() <= 32_000)
                .map(|last| last.source.clone()),
            last_output: last.and_then(|last| last.output.clone()),
            exit_status: last.map(|last| last.status),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct VisibleDirectories {
    count: usize,
    names: Vec<String>,
}

pub(crate) fn visible_directories(path: &std::path::Path) -> Option<VisibleDirectories> {
    use std::os::unix::ffi::OsStrExt;

    let mut result = VisibleDirectories {
        count: 0,
        names: Vec::new(),
    };
    for (index, entry) in std::fs::read_dir(path).ok()?.enumerate() {
        // A partial scan cannot establish the count.
        if index >= 16_384 {
            return None;
        }
        let entry = entry.ok()?;
        let name = entry.file_name();
        if !name.as_bytes().starts_with(b".") && entry.file_type().ok()?.is_dir() {
            result.count += 1;
            if result.names.len() < 128 {
                result.names.push(name.to_string_lossy().into_owned());
            }
        }
    }
    result.names.sort_unstable();
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_directories_from_filesystem_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path();
        for name in ["examples", "src", "target", "tests", ".hidden"] {
            std::fs::create_dir(path.join(name)).unwrap();
        }
        for name in ["README.md", "Cargo.toml", "Cargo.lock"] {
            std::fs::File::create(path.join(name)).unwrap();
        }
        std::os::unix::fs::symlink(path.join("tests"), path.join("link")).unwrap();
        let result = visible_directories(path).unwrap();
        assert_eq!(result.count, 4);
        assert_eq!(result.names, ["examples", "src", "target", "tests"]);
    }
}
