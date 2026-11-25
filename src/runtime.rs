use std::fs::File;
use std::io::{self, Write};
use std::rc::Rc;

use crate::syntax::{Command, Part, Redirect, Word};

pub(crate) struct Invocation<'a> {
    pub name: &'a str,
    pub args: &'a [String],
}

impl Invocation<'_> {
    pub fn run(&self, commands: &[Command]) -> u8 {
        let mut status = 0;
        let original = [Output::Stdout, Output::Stderr];
        for command in commands {
            let mut output = original.clone();
            if let Err(error) = self.redirect(&command.redirects, &mut output) {
                report(&output[1], &error);
                status = 1;
                continue;
            }
            let argv: Vec<_> = command
                .words
                .iter()
                .flat_map(|word| self.expand(word))
                .collect();
            let Some((name, args)) = argv.split_first() else {
                status = 0;
                continue;
            };
            match name.as_str() {
                "exit" => {
                    return match args {
                        [] => status,
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
                }
                "printf" => {
                    status = match printf(args)
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
                    return 127;
                }
            }
        }
        status
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
                    if let Some(value) = std::env::var_os(name) {
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
