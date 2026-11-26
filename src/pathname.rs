use std::ffi::OsString;

use crate::pattern::{self, TextPattern};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) struct Pattern(Vec<Component>);

enum Component {
    Literal(String),
    Recursive,
    Wildcard {
        pattern: TextPattern,
        leading_dot: bool,
    },
}

// POSIX 2.14.3: separate components before interpreting brackets, and keep
// shell quoting independent of backslashes that come from expansion results.
pub(crate) fn pattern(
    chars: impl Iterator<Item = (char, bool)>,
    globstar: bool,
) -> Result<Option<Pattern>, String> {
    let mut components = Vec::new();
    let mut component = Vec::new();
    for (ch, quoted) in pattern::unescape(chars) {
        if ch == '/' {
            components.push(compile_component(&component, globstar)?);
            component.clear();
        } else {
            component.push((ch, quoted));
        }
    }
    components.push(compile_component(&component, globstar)?);
    Ok(components
        .iter()
        .any(|part| !matches!(part, Component::Literal(_)))
        .then_some(Pattern(components)))
}

fn compile_component(chars: &[(char, bool)], globstar: bool) -> Result<Component, String> {
    if globstar && chars == [('*', false), ('*', false)] {
        return Ok(Component::Recursive);
    }
    let (pattern, active) = TextPattern::encode(chars)?;
    if active {
        Ok(Component::Wildcard {
            pattern,
            leading_dot: chars.first().is_some_and(|&(ch, _)| ch == '.'),
        })
    } else {
        Ok(Component::Literal(
            chars.iter().map(|&(ch, _)| ch).collect(),
        ))
    }
}

pub(crate) fn expand(Pattern(components): Pattern) -> Result<Vec<String>, String> {
    let mut paths = vec![OsString::new()];
    let check_literal_tail = matches!(components.last(), Some(Component::Literal(_)));
    let absolute = matches!(components.first(), Some(Component::Literal(text)) if text.is_empty());
    let mut recursive = false;
    let mut used_recursion = false;
    let count = components.len();
    for (i, component) in components.into_iter().enumerate() {
        if i != 0 && !matches!(component, Component::Recursive) {
            for path in &mut paths {
                if !(path.is_empty() && used_recursion
                    || recursive && path.as_bytes().ends_with(b"/"))
                {
                    path.push("/");
                }
            }
        }
        recursive = matches!(component, Component::Recursive);
        used_recursion |= recursive;
        match component {
            Component::Recursive => {
                let terminal = i + 1 == count;
                let mut matches = Vec::new();
                for mut prefix in paths {
                    if absolute && i == 1 {
                        prefix.push("/");
                    }
                    if !terminal && (prefix.is_empty() || Path::new(&prefix).is_dir()) {
                        matches.push(prefix.clone());
                    }
                    recursive_matches(prefix, terminal, &mut matches);
                }
                paths = matches;
            }
            Component::Literal(text) => {
                for path in &mut paths {
                    path.push(&text);
                }
            }
            Component::Wildcard {
                pattern,
                leading_dot,
            } => {
                let mut matches = Vec::new();
                for prefix in paths {
                    let dir = if prefix.is_empty() {
                        Path::new(".")
                    } else {
                        Path::new(&prefix)
                    };
                    // Missing or unreadable directories contribute no matches.
                    for entry in fs::read_dir(dir)
                        .into_iter()
                        .flatten()
                        .filter_map(Result::ok)
                    {
                        let name = entry.file_name();
                        let bytes = name.as_bytes();
                        if !leading_dot && bytes.starts_with(b".") {
                            continue;
                        }
                        if pattern.matches(bytes) {
                            let mut path = prefix.clone();
                            path.push(name);
                            matches.push(path);
                        }
                    }
                }
                paths = matches;
            }
        }
    }
    if check_literal_tail {
        paths.retain(|path| fs::symlink_metadata(path).is_ok());
    }
    // POSIX/C collation order. Per-shell locale selection is not implemented.
    paths.sort_unstable_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    if used_recursion {
        paths.dedup();
    }
    paths
        .into_iter()
        .map(|path| {
            path.into_string()
                .map_err(|_| "pathname match is not UTF-8".into())
        })
        .collect()
}

fn recursive_matches(prefix: OsString, files: bool, output: &mut Vec<OsString>) {
    let mut pending = vec![prefix];
    while let Some(prefix) = pending.pop() {
        let dir = if prefix.is_empty() {
            Path::new(".")
        } else {
            Path::new(&prefix)
        };
        for entry in fs::read_dir(dir)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
        {
            let name = entry.file_name();
            if name.as_bytes().starts_with(b".") {
                continue;
            }
            let directory = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !directory && !files {
                continue;
            }
            let mut path = prefix.clone();
            if !path.is_empty() && !path.as_bytes().ends_with(b"/") {
                path.push("/");
            }
            path.push(name);
            if directory {
                pending.push(path.clone());
            }
            output.push(path);
        }
    }
}
