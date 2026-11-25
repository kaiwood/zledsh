use std::path::Path;

use crate::runtime::{BUILTINS, Invocation};

pub fn suffix(line: &str, shell: &Invocation<'_>) -> Option<String> {
    // Leave quoted and structured command lines to a later, parser-aware completer.
    if line.contains(['\'', '"', '\\', '$', ';', '|', '&', '<', '>', '\n']) {
        return None;
    }
    let (before, token) = line.rsplit_once(char::is_whitespace).unwrap_or(("", line));
    let command = before.trim().is_empty();
    let mut matches = Vec::new();
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
    } else {
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
        collect(
            Path::new(directory),
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
        if ch.is_whitespace() || "\\'\"$`;|&<>()*?[]#!".contains(ch) {
            suffix.push('\\');
        }
        suffix.push(ch);
    }
    if matches.len() == 1 && !first.ends_with('/') {
        suffix.push(' ');
    }
    (!suffix.is_empty()).then_some(suffix)
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
