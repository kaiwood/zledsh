use std::{
    io::{self, Read, Seek},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::runtime::{BUILTINS, Invocation};

pub fn suffix(line: &str, shell: &Invocation<'_>) -> Option<String> {
    // Leave quoted and structured command lines to a later, parser-aware completer.
    if line.contains(['\'', '"', '\\', '$', ';', '|', '&', '<', '>', '\n']) {
        return None;
    }
    let (before, token) = line.rsplit_once(char::is_whitespace).unwrap_or(("", line));
    let command = before.trim().is_empty();
    let programmed = if command {
        None
    } else {
        let mut words: Vec<_> = before.split_whitespace().map(str::to_owned).collect();
        words.push(token.into());
        shell.programmed_completion(words)
    };
    let mut matches = programmed.unwrap_or_default();
    matches.retain(|candidate| {
        !candidate.is_empty()
            && candidate.starts_with(token)
            && !candidate.chars().any(char::is_control)
    });
    if token == "~" && matches.is_empty() {
        return shell
            .variable("HOME")
            .filter(|home| Path::new(home).is_dir())
            .map(|_| "/".into());
    }
    if command && !token.contains('/') {
        if token.is_empty() {
            return None;
        }
        matches.extend(
            BUILTINS
                .iter()
                .filter(|word| word.starts_with(token))
                .map(ToString::to_string),
        );
        matches.extend(
            shell
                .aliases()
                .filter(|name| name.starts_with(token))
                .map(str::to_owned),
        );
        let path = shell
            .variable("PATH")
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        for directory in std::env::split_paths(&path) {
            let directory = if directory.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &directory
            };
            collect(directory, "", token, true, &mut matches);
        }
    } else if matches.is_empty() {
        let (parent, prefix) = token
            .rsplit_once('/')
            .map_or(("", token), |(parent, prefix)| (parent, prefix));
        let original = &token[..token.len() - prefix.len()];
        let directory = if original.is_empty() {
            "."
        } else if parent.is_empty() {
            "/"
        } else {
            parent
        };
        // Expand only the lookup path; candidates must keep the typed `~/`.
        let home_directory = original
            .strip_prefix("~/")
            .and_then(|rest| shell.variable("HOME").map(|home| format!("{home}/{rest}")));
        collect(
            Path::new(home_directory.as_deref().unwrap_or(directory)),
            original,
            prefix,
            command,
            &mut matches,
        );
    }
    matches.sort_unstable();
    matches.dedup();
    let first = matches.first()?;
    let mut end = first.len();
    for candidate in &matches[1..] {
        end = first[..end]
            .char_indices()
            .zip(candidate.chars())
            .take_while(|((_, a), b)| a == b)
            .map(|((i, ch), _)| i + ch.len_utf8())
            .last()
            .unwrap_or(0);
    }
    let mut suffix = String::new();
    for ch in first.get(token.len()..end)?.chars() {
        if ch.is_whitespace() || "\\'\"$`;|&<>()*?[]#!{}~".contains(ch) {
            suffix.push('\\');
        }
        suffix.push(ch);
    }
    if matches.len() == 1 && !first.ends_with('/') {
        suffix.push(' ');
    }
    (!suffix.is_empty()).then_some(suffix)
}

pub(crate) fn run_callback(mut command: Command) -> io::Result<Vec<String>> {
    const LIMIT: u64 = 128 * 1024;
    let mut reply = tempfile::tempfile()?;
    // Use a fresh session so even an explicit open of /dev/tty cannot
    // interfere with the line editor. The worker leads its own process group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .stdout(reply.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut guard = CallbackChild {
        child,
        reaped: false,
    };
    if !guard.wait(deadline)? {
        return Ok(Vec::new());
    }
    reply.rewind()?;
    let mut bytes = Vec::new();
    reply.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

struct CallbackChild {
    child: std::process::Child,
    reaped: bool,
}

impl CallbackChild {
    fn wait(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(status.success());
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for CallbackChild {
    fn drop(&mut self) {
        // Kill children of a timed-out callback along with the worker.
        if !self.reaped {
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn collect(directory: &Path, base: &str, prefix: &str, command: bool, matches: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = directory.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(prefix)
            || (name.starts_with('.') && !prefix.starts_with('.'))
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(metadata) = entry.path().metadata() else {
            continue;
        };
        if metadata.is_dir() && (!command || !base.is_empty()) {
            matches.push(format!("{base}{name}/"));
        } else if metadata.is_file() && (!command || metadata.permissions().mode() & 0o111 != 0) {
            matches.push(format!("{base}{name}"));
        }
    }
}
