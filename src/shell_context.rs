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
