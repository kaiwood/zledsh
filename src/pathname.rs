use std::ffi::{CString, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

pub(crate) struct Pattern(Vec<Component>);

enum Component {
    Literal(String),
    Wildcard { pattern: CString, leading_dot: bool },
}

// POSIX 2.14.3: separate components before interpreting brackets, and keep
// shell quoting independent of backslashes that come from expansion results.
pub(crate) fn pattern(
    mut chars: impl Iterator<Item = (char, bool)>,
) -> Result<Option<Pattern>, String> {
    let mut components = Vec::new();
    let mut component = Vec::new();
    while let Some((ch, quoted)) = chars.next() {
        let (ch, quoted) = if ch == '\\' && !quoted {
            chars.next().map_or(('\\', true), |(ch, _)| (ch, true))
        } else {
            (ch, quoted)
        };
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
    let closing_bracket = chars.iter().rposition(|&(ch, quoted)| ch == ']' && !quoted);
    let mut active = false;
    let mut encoded = String::new();
    for (i, &(ch, quoted)) in chars.iter().enumerate() {
        let unmatched_bracket = ch == '[' && closing_bracket.is_none_or(|end| i >= end);
        active |= !quoted && !unmatched_bracket && matches!(ch, '*' | '?' | '[');
        if unmatched_bracket
            || (quoted
                && matches!(
                    ch,
                    '*' | '?' | '[' | ']' | '\\' | '-' | '!' | '^' | ':' | '=' | '.'
                ))
        {
            encoded.push('\\');
        }
        encoded.push(ch);
    }
    if active {
        Ok(Component::Wildcard {
            pattern: CString::new(encoded).map_err(|_| "pathname pattern contains a NUL byte")?,
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
                        let Ok(name_c) = CString::new(bytes) else {
                            continue;
                        };
                        // SAFETY: both arguments are live NUL-terminated strings.
                        if unsafe { libc::fnmatch(pattern.as_ptr(), name_c.as_ptr(), 0) } == 0 {
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
    paths.sort_unstable();
    paths
        .into_iter()
        .map(|path| {
            path.into_string()
                .map_err(|_| "pathname match is not UTF-8".into())
        })
        .collect()
}
