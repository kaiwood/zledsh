use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, Write};
use std::rc::Rc;

use crate::syntax::{Command, Part, Redirect, Word};

pub(crate) struct Invocation<'a> {
    name: &'a str,
    args: &'a [String],
    variables: HashMap<String, String>,
    pub status: u8,
    pub exited: bool,
}

impl<'a> Invocation<'a> {
    pub fn new(name: &'a str, args: &'a [String]) -> Self {
        Self {
            name,
            args,
            variables: HashMap::new(),
            status: 0,
            exited: false,
        }
    }

    pub fn run(&mut self, commands: &[Command], input: &mut impl BufRead) {
        let original = [Output::Stdout, Output::Stderr];
        for command in commands {
            let mut output = original.clone();
            if let Err(error) = self.redirect(&command.redirects, &mut output) {
                report(&output[1], &error);
                self.status = 1;
                continue;
            }
            let argv: Vec<_> = command
                .words
                .iter()
                .flat_map(|word| self.expand(word))
                .collect();
            let Some((name, args)) = argv.split_first() else {
                self.status = 0;
                continue;
            };
            match name.as_str() {
                "exit" => {
                    self.exited = true;
                    self.status = match args {
                        [] => self.status,
                        [arg] => match arg.parse::<i64>() {
                            Ok(value) => value as u8,
                            Err(_) => {
                                report(&output[1], "exit: expected numeric status");
                                2
                            }
                        },
                        _ => {
                            report(&output[1], "exit: too many arguments");
                            2
                        }
                    };
                    return;
                }
                "read" => {
                    self.status = match self.read(args, input) {
                        Ok(complete) => u8::from(!complete),
                        Err(error) => {
                            report(&output[1], &format!("read: {error}"));
                            1
                        }
                    };
                }
                "printf" => {
                    self.status = match printf(args)
                        .and_then(|text| output[0].write(&text).map_err(|e| e.to_string()))
                    {
                        Ok(()) => 0,
                        Err(error) => {
                            report(&output[1], &format!("printf: {error}"));
                            1
                        }
                    };
                }
                _ => {
                    report(&output[1], &format!("unsupported command: {name}"));
                    self.status = 127;
                    return;
                }
            }
        }
    }

    fn read(&mut self, args: &[String], input: &mut impl BufRead) -> Result<bool, String> {
        let (raw, name) = match args {
            [name] => (false, name),
            [flag, name] if flag == "-r" => (true, name),
            _ => return Err("expected read [-r] name".into()),
        };
        if !valid_name(name) {
            return Err("expected a variable name".into());
        }
        let mut characters = Vec::new();
        let mut line = String::new();
        let complete = 'lines: loop {
            line.clear();
            input
                .read_line(&mut line)
                .map_err(|error| error.to_string())?;
            let mut chars = line.chars();
            while let Some(ch) = chars.next() {
                match ch {
                    '\n' => break 'lines true,
                    '\\' if !raw => match chars.next() {
                        Some('\n') => continue 'lines,
                        Some(escaped) => characters.push((escaped, false)),
                        None => break 'lines false,
                    },
                    ch => characters.push((ch, matches!(ch, ' ' | '\t'))),
                }
            }
            break false;
        };
        let first = characters
            .iter()
            .position(|&(_, separator)| !separator)
            .unwrap_or(characters.len());
        let end = characters
            .iter()
            .rposition(|&(_, separator)| !separator)
            .map_or(first, |index| index + 1);
        self.variables.insert(
            name.clone(),
            characters[first..end].iter().map(|&(ch, _)| ch).collect(),
        );
        Ok(complete)
    }

    fn expand(&self, word: &Word) -> Vec<String> {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut present = false;
        for part in &word.0 {
            match part {
                Part::Literal(text) => field.push_str(text),
                Part::Name => field.push_str(self.name),
                Part::Arguments => {
                    let Some((first, rest)) = self.args.split_first() else {
                        continue;
                    };
                    field.push_str(first);
                    for arg in rest {
                        fields.push(std::mem::take(&mut field));
                        field.push_str(arg);
                    }
                }
                Part::Variable { name, default } => {
                    if let Some(value) = self.variables.get(name) {
                        field.push_str(value);
                    } else if let Some(value) = std::env::var_os(name) {
                        field.push_str(&value.to_string_lossy());
                    } else if let Some(value) = default {
                        field.push_str(value);
                    }
                }
            }
            present = true;
        }
        if present {
            fields.push(field);
        }
        fields
    }

    fn redirect(&self, redirects: &[Redirect], output: &mut [Output; 2]) -> Result<(), String> {
        for redirect in redirects {
            let targets = self.expand(&redirect.target);
            let [target] = targets.as_slice() else {
                return Err("ambiguous redirection".into());
            };
            let destination = if redirect.duplicate {
                match target.as_str() {
                    "1" => output[0].clone(),
                    "2" => output[1].clone(),
                    _ => return Err("only output descriptors 1 and 2 are supported".into()),
                }
            } else {
                let file = File::create(target).map_err(|error| format!("{target}: {error}"))?;
                Output::File(Rc::new(file))
            };
            output[redirect.fd - 1] = destination;
        }
        Ok(())
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.bytes();
    chars
        .next()
        .is_some_and(|ch| ch == b'_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == b'_' || ch.is_ascii_alphanumeric())
}

#[derive(Clone)]
enum Output {
    Stdout,
    Stderr,
    File(Rc<File>),
}

impl Output {
    fn write(&self, text: &str) -> io::Result<()> {
        match self {
            Self::Stdout => {
                let mut output = io::stdout().lock();
                output.write_all(text.as_bytes())?;
                output.flush()
            }
            Self::Stderr => io::stderr().lock().write_all(text.as_bytes()),
            Self::File(file) => file.as_ref().write_all(text.as_bytes()),
        }
    }
}

fn report(output: &Output, message: &str) {
    let _ = output.write(&format!("kwsh: {message}\n"));
}

fn printf(args: &[String]) -> Result<String, String> {
    let Some((format, args)) = args.split_first() else {
        return Err("expected a format".into());
    };
    let mut output = String::new();
    let mut args = args.iter();
    loop {
        let mut format = format.chars();
        let mut consumes_arguments = false;
        while let Some(ch) = format.next() {
            match ch {
                '%' => match format.next() {
                    Some('%') => output.push('%'),
                    Some('s') => {
                        consumes_arguments = true;
                        if let Some(arg) = args.next() {
                            output.push_str(arg);
                        }
                    }
                    _ => return Err("only %s and %% are supported".into()),
                },
                '\\' => match format.next() {
                    Some('n') => output.push('\n'),
                    Some('r') => output.push('\r'),
                    Some('t') => output.push('\t'),
                    Some('\\') => output.push('\\'),
                    _ => return Err("unsupported format escape".into()),
                },
                ch => output.push(ch),
            }
        }
        if args.len() == 0 || !consumes_arguments {
            return Ok(output);
        }
    }
}
