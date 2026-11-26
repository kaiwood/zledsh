use std::{env, ffi::CStr, fmt::Write, io::IsTerminal, path::Path};

use crate::runtime::Invocation;

pub fn render(shell: &mut Invocation<'_>, continuation: bool, number: usize) -> String {
    if let Some(template) = shell.variable("PROMPT").filter(|_| !continuation) {
        return formatted(shell, &template);
    }
    let (name, default) = if continuation {
        ("PS2", "> ")
    } else {
        ("PS1", "$ ")
    };
    let value = shell.variable(name).unwrap_or_else(|| default.into());
    let value = shell.parameter_text(&value).unwrap_or_else(|error| {
        eprintln!("zledsh: {name}: {error}");
        default.into()
    });
    if continuation {
        return value;
    }
    let mut result = String::new();
    let mut chars = value.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '!' || chars.next_if_eq(&'!').is_some() {
            result.push(ch);
        } else {
            let _ = write!(result, "{number}");
        }
    }
    result
}

fn formatted(shell: &Invocation<'_>, mut text: &str) -> String {
    let cwd = env::current_dir().unwrap_or_default();
    let home = shell.variable("HOME").unwrap_or_default();
    let home_path = Path::new(&home);
    let canonical_home = home_path.canonicalize();
    let relative = cwd.strip_prefix(home_path).or_else(|error| {
        canonical_home
            .as_ref()
            .map_err(|_| error)
            .and_then(|home| cwd.strip_prefix(home))
    });
    let abbreviated = match relative {
        Ok(path) if home_path.is_absolute() => {
            if path.as_os_str().is_empty() {
                "~".into()
            } else {
                format!("~/{}", path.display())
            }
        }
        _ => cwd.to_string_lossy().into_owned(),
    };
    let color = std::io::stderr().is_terminal()
        && shell.variable("TERM").as_deref() != Some("dumb")
        && shell
            .variable("NO_COLOR")
            .is_none_or(|value| value.is_empty())
        && shell.variable("KWARD_COLOR").as_deref() != Some("never");
    let mut result = String::new();
    let mut styled = false;
    while let Some((prefix, rest)) = text.split_once('%') {
        result.push_str(prefix);
        text = rest;
        if let Some(rest) = text.strip_prefix("1~") {
            result.push_str(
                Path::new(&abbreviated)
                    .file_name()
                    .and_then(|p| p.to_str())
                    .unwrap_or(&abbreviated),
            );
            text = rest;
            continue;
        }
        let Some(code) = text.chars().next() else {
            result.push('%');
            break;
        };
        text = &text[code.len_utf8()..];
        let style = match code {
            '%' => {
                result.push('%');
                None
            }
            '~' => {
                result.push_str(&abbreviated);
                None
            }
            'd' | '/' => {
                result.push_str(&cwd.to_string_lossy());
                None
            }
            '?' => {
                let _ = write!(result, "{}", shell.status);
                None
            }
            '#' => {
                result.push(if unsafe { libc::geteuid() } == 0 {
                    '#'
                } else {
                    '%'
                });
                None
            }
            'n' => {
                result.push_str(&username());
                None
            }
            'm' => {
                result.push_str(&hostname());
                None
            }
            'f' => Some("39".into()),
            'k' => Some("49".into()),
            'B' => Some("1".into()),
            'b' => Some("22".into()),
            'F' | 'K' => {
                let spec = text.strip_prefix('{').and_then(|rest| rest.split_once('}'));
                match spec.and_then(|(spec, tail)| {
                    color_code(code == 'F', spec).map(|style| (style, tail))
                }) {
                    Some((style, tail)) => {
                        text = tail;
                        Some(style)
                    }
                    None => {
                        result.push('%');
                        result.push(code);
                        None
                    }
                }
            }
            _ => {
                result.push('%');
                result.push(code);
                None
            }
        };
        if let Some(style) = style.filter(|_| color) {
            styled = true;
            let _ = write!(result, "\x1b[{style}m");
        }
    }
    result.push_str(text);
    if styled {
        result.push_str("\x1b[0m");
    }
    result
}

fn color_code(foreground: bool, spec: &str) -> Option<String> {
    let base = if foreground { 30 } else { 40 };
    let names = [
        "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
    ];
    if let Some(index) = names.iter().position(|name| *name == spec) {
        return Some((base + index).to_string());
    }
    if !spec.is_empty() && spec.len() <= 3 && spec.bytes().all(|b| b.is_ascii_digit()) {
        return spec
            .parse::<u8>()
            .ok()
            .map(|index| format!("{};5;{index}", base + 8));
    }
    let hex = spec
        .strip_prefix('#')
        .filter(|hex| hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit()))?;
    let value = u32::from_str_radix(hex, 16).ok()?;
    Some(format!(
        "{};2;{};{};{}",
        base + 8,
        value >> 16,
        (value >> 8) & 255,
        value & 255
    ))
}

fn username() -> String {
    // The shell is single-threaded; copy libc's static result before another lookup.
    unsafe {
        let uid = libc::geteuid();
        let entry = libc::getpwuid(uid);
        if entry.is_null() {
            uid.to_string()
        } else {
            CStr::from_ptr((*entry).pw_name)
                .to_string_lossy()
                .into_owned()
        }
    }
}

fn hostname() -> String {
    let mut buffer = [0u8; 256];
    if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
        return String::new();
    }
    let end = buffer
        .iter()
        .position(|b| matches!(b, 0 | b'.'))
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}
