use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{self, BufRead, Read, Seek, Write};
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::rc::Rc;

use crate::process;

use crate::syntax::{self, Command, ParseError, Part, Redirect, Script, Statement, Word};

pub(crate) struct Invocation<'a> {
    name: &'a str,
    args: &'a [String],
    variables: HashMap<String, String>,
    aliases: HashMap<String, String>,
    exported: HashSet<String>,
    unset: HashSet<String>,
    substitution_depth: u8,
    eval_depth: u8,
    loop_depth: usize,
    loop_control: Option<LoopControl>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub foreground: Option<crate::foreground::Foreground>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    agent_deadline: Option<std::time::Instant>,
    pub status: u8,
    pub exited: bool,
}

#[derive(Clone, Copy)]
enum LoopControl {
    Break(usize),
    Continue(usize),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Substitution {
    source: String,
    name: String,
    args: Vec<String>,
    variables: HashMap<String, String>,
    aliases: HashMap<String, String>,
    exported: HashSet<String>,
    unset: HashSet<String>,
    depth: u8,
    status: u8,
}

impl<'a> Invocation<'a> {
    pub fn new(name: &'a str, args: &'a [String]) -> Self {
        Self {
            name,
            args,
            variables: HashMap::new(),
            aliases: HashMap::new(),
            exported: HashSet::new(),
            unset: HashSet::new(),
            substitution_depth: 0,
            eval_depth: 0,
            loop_depth: 0,
            loop_control: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            foreground: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            agent_deadline: None,
            status: 0,
            exited: false,
        }
    }

    pub fn parse(&self, source: &str) -> Result<Script, ParseError> {
        let mut commands = syntax::parse(source)?;
        self.expand_aliases(&mut commands)?;
        Ok(commands)
    }

    fn expand_aliases(&self, script: &mut [Statement]) -> Result<(), ParseError> {
        for statement in script {
            match statement {
                Statement::Pipeline(stages) => {
                    for command in stages {
                        self.expand_alias(command)?;
                    }
                }
                Statement::Negated(inner) => {
                    self.expand_aliases(std::slice::from_mut(inner.as_mut()))?
                }
                Statement::AndOr { first, rest } => {
                    self.expand_aliases(std::slice::from_mut(first.as_mut()))?;
                    for (_, statement) in rest {
                        self.expand_aliases(std::slice::from_mut(statement))?;
                    }
                }
                Statement::For { body, .. } => self.expand_aliases(body)?,
                Statement::While {
                    condition, body, ..
                } => {
                    self.expand_aliases(condition)?;
                    self.expand_aliases(body)?;
                }
                Statement::If {
                    branches,
                    otherwise,
                } => {
                    for (condition, body) in branches {
                        self.expand_aliases(condition)?;
                        self.expand_aliases(body)?;
                    }
                    self.expand_aliases(otherwise)?;
                }
            }
        }
        Ok(())
    }

    fn expand_alias(&self, command: &mut Command) -> Result<(), ParseError> {
        let mut seen = HashSet::new();
        while let Some(Word {
            parts,
            bare: true,
            assignment: false,
        }) = command.words.first()
        {
            let [Part::Literal(name)] = parts.as_slice() else {
                break;
            };
            let Some((name, value)) = self.aliases.get_key_value(name) else {
                break;
            };
            if !seen.insert(name) {
                break;
            }
            command.words.splice(..1, alias_words(value)?);
        }
        Ok(())
    }

    pub fn run_source(&mut self, source: &str, input: &mut impl BufRead) -> Result<(), String> {
        self.run_source_with_output(source, input, &[Output::Stdout, Output::Stderr])
    }

    fn run_source_with_output(
        &mut self,
        source: &str,
        input: &mut impl BufRead,
        output: &[Output; 2],
    ) -> Result<(), String> {
        let mut pending = String::new();
        for line in source.split_inclusive('\n') {
            pending.push_str(line);
            match self.parse(&pending) {
                Ok(commands) => self.run_with_output(&commands, input, output),
                Err(ParseError::Incomplete(_)) => continue,
                Err(error) => return Err(error.message().to_owned()),
            }
            if self.interrupted() {
                return Ok(());
            }
            pending.clear();
        }
        if let Err(error) = self.parse(&pending) {
            return Err(error.message().to_owned());
        }
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn aliases(&self) -> impl Iterator<Item = &str> {
        self.aliases.keys().map(String::as_str)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn run_agent_command(&mut self, pipeline: Vec<Command>) {
        self.agent_deadline = Some(std::time::Instant::now() + std::time::Duration::from_secs(30));
        self.run_pipeline(&pipeline, &mut io::empty());
        self.agent_deadline = None;
    }

    fn agent_command(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.agent_deadline.is_some()
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }

    pub fn run(&mut self, script: &[Statement], input: &mut impl BufRead) {
        self.run_with_output(script, input, &[Output::Stdout, Output::Stderr]);
    }

    fn run_with_output(
        &mut self,
        script: &[Statement],
        input: &mut impl BufRead,
        output: &[Output; 2],
    ) {
        for statement in script {
            if self.interrupted() {
                break;
            }
            match statement {
                Statement::Pipeline(pipeline) => {
                    self.run_pipeline_with_output(pipeline, input, output)
                }
                Statement::Negated(inner) => {
                    self.run_with_output(std::slice::from_ref(inner.as_ref()), input, output);
                    if !self.interrupted() {
                        self.status = u8::from(self.status == 0);
                    }
                }
                Statement::AndOr { first, rest } => {
                    self.run_with_output(std::slice::from_ref(first.as_ref()), input, output);
                    for (on_success, statement) in rest {
                        if self.interrupted() {
                            break;
                        }
                        if (self.status == 0) == *on_success {
                            self.run_with_output(std::slice::from_ref(statement), input, output);
                        }
                    }
                }
                Statement::For { name, words, body } => {
                    let items = match words {
                        Some(words) => match self.expand_words(words, &mut None) {
                            Ok(items) => items,
                            Err(error) => {
                                report(&output[1], &error);
                                self.status = 2;
                                continue;
                            }
                        },
                        None => self.args.to_vec(),
                    };
                    self.status = 0;
                    self.loop_depth += 1;
                    for item in items {
                        self.assign(&format!("{name}={item}"));
                        self.run_with_output(body, input, output);
                        if !self.next_iteration() {
                            break;
                        }
                    }
                    self.loop_depth -= 1;
                }
                Statement::While {
                    until,
                    condition,
                    body,
                } => {
                    let mut last = 0;
                    self.loop_depth += 1;
                    loop {
                        self.run_with_output(condition, input, output);
                        if self.interrupted() {
                            last = self.status;
                            if self.next_iteration() {
                                continue;
                            }
                            break;
                        }
                        if (self.status != 0) != *until {
                            break;
                        }
                        self.run_with_output(body, input, output);
                        last = self.status;
                        if !self.next_iteration() {
                            break;
                        }
                    }
                    self.loop_depth -= 1;
                    self.status = last;
                }
                Statement::If {
                    branches,
                    otherwise,
                } => {
                    let mut selected = otherwise;
                    for (condition, body) in branches {
                        self.run_with_output(condition, input, output);
                        if self.interrupted() {
                            return;
                        }
                        if self.status == 0 {
                            selected = body;
                            break;
                        }
                    }
                    if selected.is_empty() {
                        self.status = 0;
                    } else {
                        self.run_with_output(selected, input, output);
                    }
                }
            }
        }
    }

    fn interrupted(&self) -> bool {
        self.exited || self.loop_control.is_some()
    }

    fn next_iteration(&mut self) -> bool {
        if self.exited {
            return false;
        }
        match self.loop_control.take() {
            None | Some(LoopControl::Continue(1)) => true,
            Some(LoopControl::Break(1)) => false,
            Some(LoopControl::Break(levels)) => {
                self.loop_control = Some(LoopControl::Break(levels - 1));
                false
            }
            Some(LoopControl::Continue(levels)) => {
                self.loop_control = Some(LoopControl::Continue(levels - 1));
                false
            }
        }
    }

    pub fn run_pipeline(&mut self, pipeline: &[Command], input: &mut impl BufRead) {
        self.run_pipeline_with_output(pipeline, input, &[Output::Stdout, Output::Stderr]);
    }

    fn run_pipeline_with_output(
        &mut self,
        pipeline: &[Command],
        input: &mut impl BufRead,
        original: &[Output; 2],
    ) {
        let background = pipeline.last().is_some_and(|command| command.background);
        if background || pipeline.len() != 1 {
            self.status = match self.pipeline(pipeline, background, original) {
                Ok(status) => status,
                Err(error) => {
                    report(&original[1], &format!("pipeline: {error}"));
                    1
                }
            };
            return;
        }
        let command = &pipeline[0];
        let mut output = original.clone();
        let mut substitution_status = None;
        if let Err(error) = self.redirect(&command.redirects, &mut output, &mut substitution_status)
        {
            report(&output[1], &error);
            self.status = 1;
            return;
        }
        if !command.words.is_empty() && command.words.iter().all(|word| word.assignment) {
            for word in &command.words {
                match self.expand(word, &mut substitution_status) {
                    Ok(fields) if fields.len() == 1 => self.assign(&fields[0]),
                    result => {
                        report(
                            &output[1],
                            &result
                                .err()
                                .unwrap_or_else(|| "assignment must expand to one value".into()),
                        );
                        self.status = 2;
                        return;
                    }
                }
            }
            self.status = substitution_status.unwrap_or(0);
            return;
        }
        let argv = match self.expand_words(&command.words, &mut substitution_status) {
            Ok(argv) => argv,
            Err(error) => {
                report(&output[1], &error);
                self.status = 2;
                return;
            }
        };
        let mut argv = argv.as_slice();
        while let [prefix, rest @ ..] = argv {
            if prefix != "command" {
                break;
            }
            match rest {
                [flag, names @ ..] if flag == "-v" => {
                    self.status = self.describe_commands(names, &output);
                    return;
                }
                [flag, ..] if flag.starts_with('-') && flag != "--" => {
                    report(&output[1], "command: expected -v or a command name");
                    self.status = 2;
                    return;
                }
                [flag, rest @ ..] if flag == "--" => argv = rest,
                _ => argv = rest,
            }
        }
        let Some((name, args)) = argv.split_first() else {
            self.status = substitution_status.unwrap_or(0);
            return;
        };
        if self.agent_command() && matches!(name.as_str(), "exit" | "fg" | "bg" | "wait" | "eval") {
            report(&output[1], &format!("{name} must be submitted by the user"));
            self.status = 2;
            return;
        }
        match name.as_str() {
            ":" | "true" => self.status = 0,
            "false" => self.status = 1,
            "break" | "continue" => {
                let count = match args {
                    [] => Some(1),
                    [arg] => arg.parse::<usize>().ok().filter(|&n| n > 0),
                    _ => None,
                };
                if let Some(levels) = count.filter(|&n| n <= self.loop_depth) {
                    self.loop_control = Some(if name == "break" {
                        LoopControl::Break(levels)
                    } else {
                        LoopControl::Continue(levels)
                    });
                    self.status = 0;
                } else {
                    report(
                        &output[1],
                        &format!("{name}: expected a valid enclosing loop level"),
                    );
                    self.status = 2;
                }
            }
            "eval" => {
                if self.eval_depth >= 32 {
                    report(&output[1], "eval: nesting too deep");
                    self.status = 2;
                    return;
                }
                let source = args.join(" ");
                self.eval_depth += 1;
                self.status = 0;
                if let Err(error) = self.run_source_with_output(&source, input, &output) {
                    report(&output[1], &format!("eval: {error}"));
                    self.status = 2;
                }
                self.eval_depth -= 1;
            }
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
            }
            "cd" | "pwd" => {
                let result = if name == "cd" {
                    self.change_directory(args, &output[0])
                } else if args.is_empty() {
                    std::env::current_dir()
                        .and_then(|path| output[0].write(&format!("{}\n", path.display())))
                        .map_err(|error| error.to_string())
                } else {
                    Err("arguments are not supported".into())
                };
                self.status = match result {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&output[1], &format!("{name}: {error}"));
                        1
                    }
                };
            }
            "export" => {
                self.status = 0;
                for arg in args {
                    let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
                    if !valid_name(name) {
                        report(&output[1], "export: expected a variable name or assignment");
                        self.status = 2;
                        break;
                    }
                    self.exported.insert(name.to_owned());
                    self.assign(arg);
                }
            }
            "unset" => {
                self.status = 0;
                for name in args.strip_prefix(&[String::from("-v")]).unwrap_or(args) {
                    if !valid_name(name) {
                        report(&output[1], "unset: expected a variable name");
                        self.status = 2;
                        break;
                    }
                    self.variables.remove(name);
                    self.exported.remove(name);
                    self.unset.insert(name.clone());
                }
            }
            "alias" | "unalias" => {
                if let Err(error) = self.update_aliases(name, args, &output[0]) {
                    report(&output[1], &format!("{name}: {error}"));
                    self.status = 1;
                } else {
                    self.status = 0;
                }
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
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            "fg" | "bg" | "jobs" | "wait" => {
                let jobs = self
                    .foreground
                    .get_or_insert_with(crate::foreground::Foreground::noninteractive);
                let selector = args.first().map(String::as_str);
                let result = if args.len() > 1 || (name == "jobs" && selector.is_some()) {
                    Err(io::Error::other("unexpected job arguments"))
                } else {
                    match name.as_str() {
                        "fg" => jobs.resume(selector),
                        "bg" => jobs.background(selector),
                        "wait" => jobs.wait_background(selector),
                        _ => jobs
                            .listing()
                            .and_then(|text| output[0].write(&text))
                            .map(|()| 0),
                    }
                };
                self.status = result.unwrap_or_else(|error| {
                    report(&output[1], &format!("{name}: {error}"));
                    if name == "wait" { 127 } else { 1 }
                });
            }
            _ => self.status = self.external(argv, &output),
        }
    }

    fn update_aliases(
        &mut self,
        builtin: &str,
        args: &[String],
        output: &Output,
    ) -> Result<(), String> {
        if builtin == "unalias" {
            if args == ["-a"] {
                self.aliases.clear();
            } else if args.is_empty() {
                return Err("expected an alias name".into());
            } else {
                for name in args {
                    self.aliases
                        .remove(name)
                        .ok_or_else(|| format!("{name}: alias not found"))?;
                }
            }
            return Ok(());
        }
        let print = |name: &str, value: &str| {
            output
                .write(&format!("{name}='{}'\n", value.replace('\'', "'\\''")))
                .map_err(|error| error.to_string())
        };
        if args.is_empty() {
            let mut aliases: Vec<_> = self.aliases.iter().collect();
            aliases.sort_unstable();
            for (name, value) in aliases {
                print(name, value)?;
            }
        }
        for arg in args {
            if let Some((name, value)) = arg.split_once('=') {
                if name.is_empty()
                    || !name
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || b"_-.!%,@".contains(&c))
                {
                    return Err("invalid alias name".into());
                }
                alias_words(value).map_err(|error| error.message().to_owned())?;
                self.aliases.insert(name.to_owned(), value.to_owned());
            } else {
                print(
                    arg,
                    self.aliases
                        .get(arg)
                        .ok_or_else(|| format!("{arg}: alias not found"))?,
                )?;
            }
        }
        Ok(())
    }

    fn change_directory(&mut self, args: &[String], output: &Output) -> Result<(), String> {
        let destination = match args {
            [] => self.variable("HOME").ok_or("HOME is not set")?,
            [path] if path == "-" => self.variable("OLDPWD").ok_or("OLDPWD is not set")?,
            [path] => path.clone(),
            _ => return Err("expected at most one directory".into()),
        };
        let previous = std::env::current_dir().map_err(|error| error.to_string())?;
        std::env::set_current_dir(&destination)
            .map_err(|error| format!("{destination}: {error}"))?;
        let current = std::env::current_dir().map_err(|error| error.to_string())?;
        for (name, path) in [("OLDPWD", previous), ("PWD", current.clone())] {
            self.variables
                .insert(name.into(), path.to_string_lossy().into_owned());
            self.exported.insert(name.into());
        }
        if args.first().is_some_and(|arg| arg == "-") {
            output
                .write(&format!("{}\n", current.display()))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn describe_commands(&self, names: &[String], output: &[Output; 2]) -> u8 {
        let path = self.command_path();
        let mut status = u8::from(names.is_empty());
        for name in names {
            let description = if let Some(alias) = self.aliases.get(name) {
                format!("alias {name}='{}'", alias.replace("'", "'\\''"))
            } else if is_builtin(name) {
                name.clone()
            } else if let Some(path) = process::find_executable(name, &path) {
                path.to_string_lossy().into_owned()
            } else {
                status = 1;
                continue;
            };
            if let Err(error) = output[0].write(&format!("{description}\n")) {
                report(&output[1], &format!("command: {error}"));
                return 1;
            }
        }
        status
    }

    fn command_path(&self) -> OsString {
        self.variable_os("PATH")
            .unwrap_or_else(|| OsString::from("/usr/bin:/bin"))
    }

    fn execution_environment(&self) -> (OsString, HashMap<OsString, OsString>) {
        let path = self.command_path();
        let mut env: HashMap<OsString, OsString> = std::env::vars_os()
            .filter(|(name, _)| !name.to_str().is_some_and(|name| self.unset.contains(name)))
            .collect();
        for (name, value) in &self.variables {
            if self.exported.contains(name) || env.contains_key(OsStr::new(name)) {
                env.insert(name.into(), value.into());
            }
        }
        (path, env)
    }

    fn pipeline(
        &mut self,
        commands: &[Command],
        background: bool,
        original: &[Output; 2],
    ) -> io::Result<u8> {
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        if background {
            return Err(io::Error::other(
                "background jobs are not supported on this platform",
            ));
        }
        if background && self.substitution_depth != 0 {
            return Err(io::Error::other(
                "background jobs are not supported in command substitution",
            ));
        }
        if background && self.agent_command() {
            return Err(io::Error::other("agent cannot start background jobs"));
        }
        let (path, env) = self.execution_environment();
        let mut stdin = if background || self.agent_command() {
            File::open("/dev/null")?.into()
        } else {
            process::duplicate(libc::STDIN_FILENO)?
        };
        let mut stages = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let next = if index + 1 == commands.len() {
                None
            } else {
                Some(process::pipe()?)
            };
            let (stdout, next_input) = match next {
                Some((read, write)) => (Output::File(Rc::new(File::from(write))), Some(read)),
                None => (original[0].clone(), None),
            };
            let mut output = [stdout, original[1].clone()];
            let mut status = None;
            let argv = match self.expand_words(&command.words, &mut status) {
                Err(error) => {
                    report(&output[1], &error);
                    vec!["exit".into(), "2".into()]
                }
                Ok(argv) => {
                    if let Err(error) = self.redirect(&command.redirects, &mut output, &mut status)
                    {
                        report(&output[1], &error);
                        vec!["exit".into(), "1".into()]
                    } else if argv.is_empty() || command.words.iter().all(|word| word.assignment) {
                        vec!["exit".into(), status.unwrap_or(0).to_string()]
                    } else {
                        argv
                    }
                }
            };
            if self.agent_command() && direct_name(&argv) == Some("eval") {
                return Err(io::Error::other("eval must be submitted by the user"));
            }
            let name = argv[0].clone();
            let exec = if is_builtin(&name) {
                let executable = std::env::current_exe()?;
                let mut args = vec![
                    executable
                        .to_str()
                        .ok_or_else(|| io::Error::other("shell path must be UTF-8"))?
                        .to_owned(),
                    crate::PIPELINE_BUILTIN.into(),
                ];
                args.extend(argv);
                let mut local_env = env.clone();
                local_env.extend(
                    self.variables
                        .iter()
                        .map(|(key, value)| (key.into(), value.into())),
                );
                process::Exec::new(&args, &path, local_env)?
            } else {
                process::Exec::new(&argv, &path, env.clone())?
            };
            stages.push(process::Stage {
                exec,
                streams: [stdin, output[0].descriptor()?, output[1].descriptor()?],
                missing: format!("kwsh: {name}: command not found\n").into_bytes(),
                failed: format!("kwsh: {name}: cannot execute\n").into_bytes(),
            });
            match next_input {
                Some(read) => stdin = read,
                None => break,
            }
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            if background {
                return self
                    .foreground
                    .get_or_insert_with(crate::foreground::Foreground::noninteractive)
                    .run_background(stages);
            }
            if let Some(tty) = self.foreground.as_mut().filter(|tty| tty.has_terminal()) {
                return tty.run_pipeline(stages, self.agent_deadline);
            }
        }
        let (mut job, gate) = process::Pipeline::start(stages, false)?;
        drop(gate);
        match job.wait(None)? {
            process::Outcome::Exited { code, .. } | process::Outcome::Stopped(code) => Ok(code),
            process::Outcome::TimedOut => Ok(124),
        }
    }

    fn external(&mut self, argv: &[String], output: &[Output; 2]) -> u8 {
        let (path, env) = self.execution_environment();
        let streams = output[0]
            .descriptor()
            .and_then(|stdout| Ok((Stdio::from(stdout), Stdio::from(output[1].descriptor()?))));
        let (stdout, stderr) = match streams {
            Ok(streams) => streams,
            Err(error) => {
                report(&output[1], &format!("{}: {error}", argv[0]));
                return 1;
            }
        };
        let stdin = if self.agent_command() {
            Stdio::null()
        } else {
            Stdio::inherit()
        };
        let launch = |enter| process::spawn(argv, &path, env, [stdin, stdout, stderr], enter);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let result = match self.foreground.as_mut().filter(|tty| tty.has_terminal()) {
            Some(tty) => tty.run(self.agent_deadline, || {
                use std::os::fd::AsRawFd;

                // pre_exec runs after stdio is installed, so fd 0 may be
                // /dev/null. Keep the original tty open just until exec.
                let terminal = process::duplicate(crate::terminal_modes::TTY)?;
                launch(Some(move || {
                    crate::terminal_modes::enter_child(terminal.as_raw_fd())
                }))
            }),
            None => launch(None).and_then(|mut child| child.wait().map(process::status_code)),
        };
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        let result = launch(None::<fn() -> io::Result<()>>)
            .and_then(|mut child| child.wait().map(process::status_code));
        match result {
            Ok(status) => status,
            Err(error) => {
                let missing = error.raw_os_error() == Some(libc::ENOENT);
                let message = if missing {
                    "command not found".to_owned()
                } else {
                    error.to_string()
                };
                report(&output[1], &format!("{}: {message}", argv[0]));
                if missing { 127 } else { 126 }
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

    fn assign(&mut self, assignment: &str) {
        if let Some((name, value)) = assignment.split_once('=') {
            self.variables.insert(name.to_owned(), value.to_owned());
        }
    }

    fn variable_os(&self, name: &str) -> Option<OsString> {
        self.variables.get(name).map(OsString::from).or_else(|| {
            if self.unset.contains(name) {
                None
            } else {
                std::env::var_os(name)
            }
        })
    }

    pub fn variable(&self, name: &str) -> Option<String> {
        self.variable_os(name)
            .and_then(|value| value.into_string().ok())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn parameter_text(&self, value: &str) -> Result<String, String> {
        self.expand(&crate::syntax::parameter_text(value)?, &mut None)
            .map(|fields| fields.join(" "))
    }

    fn expand_words(&self, words: &[Word], status: &mut Option<u8>) -> Result<Vec<String>, String> {
        let mut fields = Vec::new();
        for word in words {
            fields.extend(self.expand(word, status)?);
        }
        Ok(fields)
    }

    pub fn run_substitution() -> Result<u8, String> {
        let saved: Substitution = serde_json::from_reader(process::Stdin)
            .map_err(|_| "invalid command substitution state")?;
        let mut shell = Invocation::new(&saved.name, &saved.args);
        shell.variables = saved.variables;
        shell.aliases = saved.aliases;
        shell.exported = saved.exported;
        shell.unset = saved.unset;
        shell.substitution_depth = saved.depth;
        shell.status = saved.status;
        process::null_stdin().map_err(|error| error.to_string())?;
        shell.run_source(&saved.source, &mut io::empty())?;
        Ok(shell.status)
    }

    fn substitute(&self, source: &str) -> Result<(String, u8), String> {
        const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
        if self.agent_command() {
            return Err("command substitutions must be submitted by the user".into());
        }
        if self.substitution_depth >= 16 {
            return Err("command substitutions nested too deeply".into());
        }
        let saved = Substitution {
            source: source.into(),
            name: self.name.into(),
            args: self.args.to_vec(),
            variables: self.variables.clone(),
            aliases: self.aliases.clone(),
            exported: self.exported.clone(),
            unset: self.unset.clone(),
            depth: self.substitution_depth + 1,
            status: self.status,
        };
        let capture = || -> io::Result<_> {
            // Send private shell variables via an anonymous fd, not argv or the environment.
            let mut payload = tempfile::tempfile()?;
            serde_json::to_writer(&mut payload, &saved)?;
            payload.rewind()?;
            let mut command = std::process::Command::new(std::env::current_exe()?);
            command
                .arg(crate::SUBSTITUTION)
                .stdin(payload)
                .stdout(Stdio::piped());
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Some(tty) = self.foreground.as_ref().filter(|tty| tty.has_terminal()) {
                return tty.capture(command, MAX_OUTPUT);
            }
            let mut child = command.spawn()?;
            let mut bytes = Vec::new();
            let read = child
                .stdout
                .take()
                .unwrap()
                .take(MAX_OUTPUT + 1)
                .read_to_end(&mut bytes);
            if read.is_err() || bytes.len() as u64 > MAX_OUTPUT {
                let _ = child.kill();
            }
            let status = child.wait().map(process::status_code)?;
            read?;
            Ok((bytes, status))
        };
        let (mut bytes, status) =
            capture().map_err(|error| format!("command substitution: {error}"))?;
        if bytes.len() as u64 > MAX_OUTPUT {
            return Err("command substitution exceeds 16 MiB".into());
        }
        if bytes.contains(&0) {
            return Err("command substitution contains a NUL byte".into());
        }
        while bytes.last() == Some(&b'\n') {
            bytes.pop();
        }
        let text =
            String::from_utf8(bytes).map_err(|_| "command substitution output is not UTF-8")?;
        Ok((text, status))
    }

    fn expand(&self, word: &Word, status: &mut Option<u8>) -> Result<Vec<String>, String> {
        let mut fields = Vec::new();
        let mut field = String::new();
        let mut present = false;
        for part in &word.parts {
            match part {
                Part::Literal(text) => field.push_str(text),
                Part::Name => field.push_str(self.name),
                Part::Status => field.push_str(&self.status.to_string()),
                Part::LastBackground => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    if let Some(pid) = self
                        .foreground
                        .as_ref()
                        .and_then(|jobs| jobs.last_background)
                    {
                        field.push_str(&pid.to_string());
                    }
                }
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
                Part::Substitution { source, split } => {
                    let (text, code) = self.substitute(source)?;
                    *status = Some(code);
                    if *split {
                        for ch in text.chars() {
                            if matches!(ch, ' ' | '\t' | '\n') {
                                if present {
                                    fields.push(std::mem::take(&mut field));
                                }
                                present = false;
                            } else {
                                field.push(ch);
                                present = true;
                            }
                        }
                        continue;
                    }
                    field.push_str(&text);
                }
                Part::Variable {
                    name,
                    default,
                    if_empty,
                } => {
                    if let Some(value) = self
                        .variable_os(name)
                        .filter(|value| !if_empty || !value.is_empty())
                    {
                        field.push_str(&value.to_string_lossy());
                    } else if let Some(value) = default {
                        field.push_str(&self.expand(value, status)?.join(" "));
                    }
                }
            }
            present = true;
        }
        if present {
            fields.push(field);
        }
        Ok(fields)
    }

    fn redirect(
        &self,
        redirects: &[Redirect],
        output: &mut [Output; 2],
        status: &mut Option<u8>,
    ) -> Result<(), String> {
        for redirect in redirects {
            let targets = self.expand(&redirect.target, status)?;
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

pub(crate) const BUILTINS: &[&str] = &[
    "exit", "cd", "pwd", "export", "unset", "read", "printf", "fg", "alias", "unalias", "jobs",
    "bg", "wait", "command", ":", "true", "false", "eval", "break", "continue",
];

pub(crate) fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

fn direct_name(mut argv: &[String]) -> Option<&str> {
    while let [name, rest @ ..] = argv {
        if name != "command" {
            return Some(name);
        }
        match rest {
            [flag, rest @ ..] if flag == "--" => argv = rest,
            [flag, ..] if flag.starts_with('-') => return Some("command"),
            _ => argv = rest,
        }
    }
    None
}

fn alias_words(value: &str) -> Result<Vec<Word>, ParseError> {
    let mut parsed = syntax::parse(value)?;
    if let [Statement::Pipeline(pipeline)] = parsed.as_mut_slice() {
        if let [command] = pipeline.as_mut_slice() {
            if !command.background
                && command.redirects.is_empty()
                && !command.words.is_empty()
                && command.words.iter().all(|word| !word.assignment)
            {
                return Ok(std::mem::take(&mut command.words));
            }
        }
    }
    Err("alias replacement must be a simple command without redirects".into())
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
    fn descriptor(&self) -> io::Result<OwnedFd> {
        match self {
            Self::Stdout => process::duplicate(libc::STDOUT_FILENO),
            Self::Stderr => process::duplicate(libc::STDERR_FILENO),
            Self::File(file) => file.try_clone().map(OwnedFd::from),
        }
    }

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

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod tests {
    use super::*;

    #[test]
    fn agent_external_command_keeps_terminal_handoff_with_null_stdin() {
        const CHILD: &str = "KWSH_TEST_AGENT_FOREGROUND_CHILD";
        const TEST: &str =
            "runtime::tests::agent_external_command_keeps_terminal_handoff_with_null_stdin";
        if std::env::var_os(CHILD).is_some() {
            let mut invocation = Invocation::new("kwsh", &[]);
            invocation.foreground = Some(crate::foreground::Foreground::new().unwrap());
            let mut run = |source| {
                let mut commands = crate::syntax::parse(source).ok().unwrap();
                let Some(Statement::Pipeline(pipeline)) = commands.pop() else {
                    panic!("expected a pipeline")
                };
                invocation.run_agent_command(pipeline);
                assert_eq!(invocation.status, 0, "{source}");
            };
            run("/usr/bin/touch foo.txt");
            run("/bin/test ! -t 0 > output 2>&1");
            assert!(crate::foreground::Foreground::new().is_ok());
            return;
        }

        // Re-exec this test on a controlling PTY, not Cargo's stdin.
        let work = tempfile::tempdir().unwrap();
        let output = crate::test_terminal::test_command(TEST, false)
            .env(CHILD, "1")
            .current_dir(work.path())
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success()
                && String::from_utf8_lossy(&output.stdout).contains("1 passed")
                && std::fs::metadata(work.path().join("foo.txt")).is_ok_and(|file| file.len() == 0),
            "{}{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(work.path().join("output")).unwrap_or_default(),
        );
    }
}
