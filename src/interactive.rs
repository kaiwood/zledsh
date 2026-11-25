use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::{env, fs};

use crate::foreground::Foreground;
use crate::runtime::Invocation;
use crate::syntax::{self, ParseError};
use crate::terminal_modes::{
    TTY, attributes, editor_attributes, restore_attributes, set_attributes,
};

pub(crate) fn run(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    kward: bool,
) -> io::Result<u8> {
    invocation.foreground = Some(Foreground::new()?);
    if kward {
        load_startup(invocation, input);
    }
    if invocation.exited {
        return Ok(invocation.status);
    }
    let editor = if kward && invocation.variable("TERM").as_deref() != Some("dumb") {
        Some(EditorTerminal::new()?)
    } else {
        None
    };
    let mut source = String::new();
    while !invocation.exited {
        let prompt = if !source.is_empty() {
            invocation.variable("PS2").unwrap_or_else(|| "> ".into())
        } else {
            let rich = kward.then(|| invocation.variable("PROMPT")).flatten();
            rich.or_else(|| invocation.variable("PS1"))
                .unwrap_or_else(|| "$ ".into())
        };
        if let Some(editor) = &editor {
            match editor.read_line(input, &prompt)? {
                Line::Submitted(line) => source.push_str(&line),
                Line::Cancelled => {
                    source.clear();
                    invocation.status = 130;
                    continue;
                }
                Line::End => break,
            }
        } else {
            write_prompt(&mut io::stderr().lock(), &prompt)?;
            if input.read_line(&mut source)? == 0 {
                break;
            }
        }
        match syntax::parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(ParseError::Incomplete(_)) => continue,
            Err(error) => {
                eprintln!("kwsh: {}", error.message());
                invocation.status = 2;
            }
        }
        source.clear();
    }
    if !source.is_empty() {
        eprintln!("kwsh: incomplete command at end of input");
        return Ok(2);
    }
    Ok(invocation.status)
}

fn load_startup(invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
    // Do not run user startup files if real and effective IDs differ.
    if unsafe { libc::getuid() != libc::geteuid() || libc::getgid() != libc::getegid() } {
        return;
    }
    let home = env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let primary = env::var_os("KWARD_CONFIG_PATH")
        .filter(|path| !path.is_empty())
        .and_then(|path| Path::new(&path).parent().map(|dir| dir.join("kwshrc")))
        .or_else(|| home.as_ref().map(|path| path.join(".kward/kwshrc")));
    let personal = home.map(|path| path.join(".kwshrc"));
    if let Some(path) = &primary {
        load_rc(path, invocation, input);
    }
    if invocation.exited || personal == primary {
        return;
    }
    if let Some(path) = &personal {
        load_rc(path, invocation, input);
    }
}

fn load_rc(path: &Path, invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
    match fs::read_to_string(path) {
        Ok(source) => match syntax::parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(_) => eprintln!(
                "kwsh: {}: invalid or unsupported startup syntax",
                path.display()
            ),
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("kwsh: {}: {error}", path.display()),
    }
}

struct EditorTerminal {
    original: libc::termios,
    editor: libc::termios,
}

impl EditorTerminal {
    fn new() -> io::Result<Self> {
        if unsafe { libc::tcgetpgrp(TTY) != libc::getpgrp() } {
            return Err(io::Error::other("shell must be in the foreground"));
        }
        let original = attributes()?;
        Ok(Self {
            original,
            editor: editor_attributes(original),
        })
    }

    fn read_line(&self, input: &mut impl BufRead, prompt: &str) -> io::Result<Line> {
        set_attributes(&self.editor)?;
        let mut stderr = io::stderr().lock();
        let line = write_prompt(&mut stderr, prompt).and_then(|()| read_keys(input, &mut stderr));
        // Builtins such as `read` run with the user's cooked settings.
        set_attributes(&self.original)?;
        line
    }
}

impl Drop for EditorTerminal {
    fn drop(&mut self) {
        restore_attributes(&self.original);
    }
}

enum Line {
    Submitted(String),
    Cancelled,
    End,
}

fn write_prompt(output: &mut impl Write, prompt: &str) -> io::Result<()> {
    output.write_all(prompt.as_bytes())?;
    output.flush()
}

fn read_keys(input: &mut impl BufRead, echo: &mut impl Write) -> io::Result<Line> {
    let mut line = Vec::new();
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(Line::End),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        match byte[0] {
            b'\n' | b'\r' => {
                echo.write_all(b"\n")?;
                echo.flush()?;
                line.push(b'\n');
                return String::from_utf8(line)
                    .map(Line::Submitted)
                    .map_err(io::Error::other);
            }
            3 => {
                echo.write_all(b"^C\n")?;
                echo.flush()?;
                return Ok(Line::Cancelled);
            }
            4 if line.is_empty() => return Ok(Line::End),
            8 | 127 if !line.is_empty() => {
                while line.pop().is_some_and(|ch| ch & 0b1100_0000 == 0b1000_0000) {}
                echo.write_all(b"\x08 \x08")?;
            }
            b' '..=b'~' | 128..=255 | b'\t' => {
                line.push(byte[0]);
                echo.write_all(&byte)?;
            }
            _ => {}
        }
        echo.flush()?;
    }
}
