use std::ffi::OsString;

use crate::pattern::{self, TextPattern};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) struct Pattern(Vec<Component>);

enum Component {
    Literal(String),
    Wildcard {
        pattern: TextPattern,
        leading_dot: bool,
    },
}

// POSIX 2.14.3: separate components before interpreting brackets, and keep
// shell quoting independent of backslashes that come from expansion results.
pub(crate) fn pattern(
    chars: impl Iterator<Item = (char, bool)>,
) -> Result<Option<Pattern>, String> {
    let mut components = Vec::new();
    let mut component = Vec::new();
    for (ch, quoted) in pattern::unescape(chars) {
        if ch == '/' {
            components.push(compile_component(&component)?);
            component.clear();
        } else {
            component.push((ch, quoted));
        }
    }
    components.push(compile_component(&component)?);
    Ok(components
        .iter()
        .any(|part| matches!(part, Component::Wildcard { .. }))
        .then_some(Pattern(components)))
}

fn compile_component(chars: &[(char, bool)]) -> Result<Component, String> {
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
    for (i, component) in components.into_iter().enumerate() {
        if i != 0 {
            for path in &mut paths {
                path.push("/");
            }
        }
        match component {
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
    paths
        .into_iter()
        .map(|path| {
            path.into_string()
                .map_err(|_| "pathname match is not UTF-8".into())
        })
        .collect()
}
