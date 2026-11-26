use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::Stdio;
use std::rc::Rc;

use crate::process;

use crate::syntax::{
    self, ArrayIndex, CaseClause, Command, ParameterOperator, ParseError, Part, Redirect,
    RedirectKind, Script, Statement, Word, WordPart,
};

const DEFAULT_IFS: &str = " \t\n";

#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Literal,
    Quoted,
    Expansion,
}

struct Fragment {
    text: String,
    origin: Origin,
}

type Field = Vec<Fragment>;

fn login_home(login: &str) -> Option<String> {
    let login = CString::new(login).ok()?;
    let mut buffer = vec![0u8; 1024];
    loop {
        let mut entry = std::mem::MaybeUninit::<libc::passwd>::uninit();
        let mut found = std::ptr::null_mut();
        // SAFETY: all pointers reference writable storage or the live C string.
        let status = unsafe {
            libc::getpwnam_r(
                login.as_ptr(),
                entry.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut found,
            )
        };
        if status == libc::ERANGE && buffer.len() < 1024 * 1024 {
            buffer.resize(buffer.len() * 2, 0);
        } else if status != 0 || found.is_null() {
            return None;
        } else {
            // SAFETY: getpwnam_r succeeded and found points to the initialized entry.
            let home = unsafe { (*found).pw_dir };
            if home.is_null() {
                return None;
            }
            // SAFETY: pw_dir points into the live result buffer.
            return unsafe { CStr::from_ptr(home) }
                .to_str()
                .ok()
                .map(str::to_owned);
        }
    }
}

fn field_text(field: Field) -> String {
    field.into_iter().map(|part| part.text).collect()
}

fn split_field(field: Field, ifs: &str) -> Vec<Field> {
    // POSIX.1-2024 2.6.5: IFS whitespace can precede a non-whitespace
    // delimiter as part of the same terminator. Only the latter forces a field.
    let mut output = Vec::new();
    let mut candidate = Field::new();
    let mut whitespace = false;
    for part in field {
        if part.origin != Origin::Expansion {
            if whitespace && !candidate.is_empty() {
                output.push(std::mem::take(&mut candidate));
            }
            whitespace = false;
            candidate.push(part);
            continue;
        }
        for ch in part.text.chars() {
            if !ifs.contains(ch) {
                if whitespace && !candidate.is_empty() {
                    output.push(std::mem::take(&mut candidate));
                }
                whitespace = false;
                if let Some(last) = candidate
                    .last_mut()
                    .filter(|last| last.origin == Origin::Expansion)
                {
                    last.text.push(ch);
                } else {
                    candidate.push(Fragment {
                        text: ch.into(),
                        origin: Origin::Expansion,
                    });
                }
            } else if DEFAULT_IFS.contains(ch) {
                whitespace = true;
            } else {
                output.push(std::mem::take(&mut candidate));
                whitespace = false;
            }
        }
    }
    if !candidate.is_empty() {
        output.push(candidate);
    }
    output
}

pub(crate) struct Invocation<'a> {
    name: &'a str,
    args: Vec<String>,
    variables: HashMap<String, String>,
    arrays: HashMap<String, BTreeMap<usize, String>>,
    aliases: HashMap<String, String>,
    functions: HashMap<String, Script>,
    completions: HashMap<String, String>,
    exported: HashSet<String>,
    unset: HashSet<String>,
    substitution_depth: u8,
    script_depth: u8,
    loop_depth: usize,
    loop_control: Option<LoopControl>,
    locals: Vec<HashMap<String, SavedVariable>>,
    returned: bool,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub foreground: Option<crate::foreground::Foreground>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    agent_deadline: Option<std::time::Instant>,
    pub status: u8,
    pub exited: bool,
    pub interactive: bool,
}

struct SavedVariable {
    value: Option<String>,
    array: Option<BTreeMap<usize, String>>,
    exported: bool,
    unset: bool,
}

#[derive(Clone, Copy)]
enum LoopControl {
    Break(usize),
    Continue(usize),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PipelineVariables {
    variables: HashMap<String, String>,
    exported: HashSet<String>,
    unset: HashSet<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Substitution {
    task: WorkerTask,
    name: String,
    args: Vec<String>,
    variables: HashMap<String, String>,
    arrays: HashMap<String, BTreeMap<usize, String>>,
    aliases: HashMap<String, String>,
    functions: HashMap<String, Script>,
    exported: HashSet<String>,
    unset: HashSet<String>,
    depth: u8,
    status: u8,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum WorkerTask {
    Script(String),
    List(Script),
    Complete {
        function: String,
        words: Vec<String>,
    },
}

impl<'a> Invocation<'a> {
    pub fn new(name: &'a str, args: &[String]) -> Self {
        Self {
            name,
            args: args.to_vec(),
            variables: HashMap::from([("IFS".into(), DEFAULT_IFS.into())]),
            arrays: HashMap::new(),
            aliases: HashMap::new(),
            functions: HashMap::new(),
            completions: HashMap::new(),
            exported: HashSet::new(),
            unset: HashSet::new(),
            substitution_depth: 0,
            script_depth: 0,
            loop_depth: 0,
            loop_control: None,
            locals: Vec::new(),
            returned: false,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            foreground: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            agent_deadline: None,
            status: 0,
            exited: false,
            interactive: false,
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
                Statement::Array { .. } => {}
                Statement::Pipeline(stages) => {
                    for command in stages {
                        if let Some(compound) = &mut command.compound {
                            self.expand_aliases(std::slice::from_mut(compound.as_mut()))?;
                        } else {
                            self.expand_alias(command)?;
                        }
                    }
                }
                Statement::Negated(inner)
                | Statement::Async(inner)
                | Statement::Redirected { command: inner, .. } => {
                    self.expand_aliases(std::slice::from_mut(inner.as_mut()))?
                }
                Statement::AndOr { first, rest } => {
                    self.expand_aliases(std::slice::from_mut(first.as_mut()))?;
                    for (_, statement) in rest {
                        self.expand_aliases(std::slice::from_mut(statement))?;
                    }
                }
                Statement::For { body, .. }
                | Statement::Function { body, .. }
                | Statement::Group(body)
                | Statement::Subshell(body) => self.expand_aliases(body)?,
                Statement::While {
                    condition, body, ..
                } => {
                    self.expand_aliases(condition)?;
                    self.expand_aliases(body)?;
                }
                Statement::Case { clauses, .. } => {
                    for clause in clauses {
                        self.expand_aliases(&mut clause.body)?;
                    }
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
            let [
                WordPart {
                    kind: Part::Literal(name),
                    ..
                },
            ] = parts.as_slice()
            else {
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
        self.run_source_with_descriptors(source, input, &inherited_descriptors())
    }

    fn run_source_with_descriptors(
        &mut self,
        source: &str,
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) -> Result<(), String> {
        let mut pending = String::new();
        for line in source.split_inclusive('\n') {
            pending.push_str(line);
            match self.parse(&pending) {
                Ok(commands) => self.run_with_descriptors(&commands, input, descriptors),
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
        self.run_with_descriptors(script, input, &inherited_descriptors());
    }

    fn run_with_descriptors(
        &mut self,
        script: &[Statement],
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        for statement in script {
            if self.interrupted() {
                break;
            }
            match statement {
                Statement::Pipeline(pipeline) => {
                    self.run_pipeline_with_descriptors(pipeline, input, descriptors)
                }
                Statement::Group(body) => self.run_with_descriptors(body, input, descriptors),
                Statement::Redirected { command, redirects } => {
                    let mut local = descriptors.clone();
                    if let Err(error) = self.redirect(redirects, &mut local, &mut None) {
                        report(&local[2], &error);
                        self.status = if self.exited { 2 } else { 1 };
                    } else {
                        self.run_with_descriptors(
                            std::slice::from_ref(command.as_ref()),
                            input,
                            &local,
                        );
                    }
                }
                Statement::Subshell(body) => {
                    self.status = match self
                        .subshell_stage(body, descriptors)
                        .and_then(|stage| self.start_stages(vec![stage], false))
                    {
                        Ok(status) => status,
                        Err(error) => {
                            report(&descriptors[2], &format!("subshell: {error}"));
                            2
                        }
                    };
                }
                Statement::Negated(inner) => {
                    self.run_with_descriptors(
                        std::slice::from_ref(inner.as_ref()),
                        input,
                        descriptors,
                    );
                    if !self.interrupted() {
                        self.status = u8::from(self.status == 0);
                    }
                }
                Statement::Async(inner) => {
                    self.status = match self.start_async(inner, descriptors) {
                        Ok(status) => status,
                        Err(error) => {
                            report(&descriptors[2], &format!("background list: {error}"));
                            1
                        }
                    };
                }
                Statement::AndOr { first, rest } => {
                    self.run_with_descriptors(
                        std::slice::from_ref(first.as_ref()),
                        input,
                        descriptors,
                    );
                    for (on_success, statement) in rest {
                        if self.interrupted() {
                            break;
                        }
                        if (self.status == 0) == *on_success {
                            self.run_with_descriptors(
                                std::slice::from_ref(statement),
                                input,
                                descriptors,
                            );
                        }
                    }
                }
                Statement::Array { name, words, local } => {
                    self.status = match self.assign_array(name, words, *local) {
                        Ok(status) => status,
                        Err(error) => {
                            report(&descriptors[2], &error);
                            2
                        }
                    };
                }
                Statement::Function { name, body } => {
                    self.functions.insert(name.clone(), body.clone());
                    self.status = 0;
                }
                Statement::For { name, words, body } => {
                    let items = match words {
                        Some(words) => match self.expand_words(words, &mut None) {
                            Ok(items) => items,
                            Err(error) => {
                                report(&descriptors[2], &error);
                                self.status = 2;
                                continue;
                            }
                        },
                        None => self.args.clone(),
                    };
                    self.status = 0;
                    self.loop_depth += 1;
                    for item in items {
                        self.assign(&format!("{name}={item}"));
                        self.run_with_descriptors(body, input, descriptors);
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
                        self.run_with_descriptors(condition, input, descriptors);
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
                        self.run_with_descriptors(body, input, descriptors);
                        last = self.status;
                        if !self.next_iteration() {
                            break;
                        }
                    }
                    self.loop_depth -= 1;
                    self.status = last;
                }
                Statement::Case { word, clauses } => match self.select_case(word, clauses) {
                    Ok(Some(index)) => {
                        self.status = 0;
                        for clause in &clauses[index..] {
                            self.run_with_descriptors(&clause.body, input, descriptors);
                            if !clause.fallthrough || self.interrupted() {
                                break;
                            }
                        }
                    }
                    Ok(_) => self.status = 0,
                    Err(error) => {
                        report(&descriptors[2], &error);
                        self.status = 2;
                    }
                },
                Statement::If {
                    branches,
                    otherwise,
                } => {
                    let mut selected = otherwise;
                    for (condition, body) in branches {
                        self.run_with_descriptors(condition, input, descriptors);
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
                        self.run_with_descriptors(selected, input, descriptors);
                    }
                }
            }
        }
    }

    fn select_case(
        &mut self,
        word: &Word,
        clauses: &[CaseClause],
    ) -> Result<Option<usize>, String> {
        let word = self.expand(word, &mut None)?.join(" ");
        for (index, clause) in clauses.iter().enumerate() {
            for pattern in &clause.patterns {
                if self
                    .expand_pattern(pattern, &mut None)?
                    .matches(word.as_bytes())
                {
                    return Ok(Some(index));
                }
            }
        }
        Ok(None)
    }

    fn expand_pattern(
        &mut self,
        word: &Word,
        status: &mut Option<u8>,
    ) -> Result<crate::pattern::TextPattern, String> {
        let fields = self.expand_parts(word, status, false, false)?;
        let chars = fields.iter().enumerate().flat_map(|(index, field)| {
            (index != 0)
                .then_some((' ', false))
                .into_iter()
                .chain(field.iter().flat_map(|part| {
                    let quoted = part.origin == Origin::Quoted;
                    part.text.chars().map(move |ch| (ch, quoted))
                }))
        });
        crate::pattern::TextPattern::compile(chars)
    }

    fn interrupted(&self) -> bool {
        self.exited || self.returned || self.loop_control.is_some()
    }

    fn next_iteration(&mut self) -> bool {
        if self.exited || self.returned {
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
        self.run_pipeline_with_descriptors(pipeline, input, &inherited_descriptors());
    }

    pub fn run_pipeline_builtin(
        command: &Command,
        input: &mut impl BufRead,
        closed: u8,
        state: File,
    ) -> serde_json::Result<u8> {
        // Consume and close the private descriptor before evaluating any commands.
        let state: PipelineVariables = serde_json::from_reader(BufReader::new(state))?;
        let mut shell = Invocation::new("zledsh", &[]);
        shell.variables = state.variables;
        shell.exported = state.exported;
        shell.unset = state.unset;
        let mut descriptors = inherited_descriptors();
        for (fd, stream) in descriptors[..3].iter_mut().enumerate() {
            if closed & (1 << fd) != 0 {
                *stream = Descriptor::Closed;
            }
        }
        shell.run_pipeline_with_descriptors(std::slice::from_ref(command), input, &descriptors);
        Ok(shell.status)
    }

    fn run_pipeline_with_descriptors(
        &mut self,
        pipeline: &[Command],
        input: &mut dyn BufRead,
        original: &Descriptors,
    ) {
        let background = pipeline.last().is_some_and(|command| command.background);
        if background || pipeline.len() != 1 {
            self.status = match self.pipeline(pipeline, background, original) {
                Ok(status) => status,
                Err(error) => {
                    report(&original[2], &format!("pipeline: {error}"));
                    1
                }
            };
            return;
        }
        let command = &pipeline[0];
        let mut substitution_status = None;
        let assignment_only =
            !command.words.is_empty() && command.words.iter().all(|word| word.assignment);
        let argv = if assignment_only {
            Vec::new()
        } else {
            match self.expand_command_words(&command.words, &mut substitution_status) {
                Ok(argv) => argv,
                Err(error) => {
                    report(&original[2], &error);
                    self.status = 2;
                    return;
                }
            }
        };
        let mut descriptors = original.clone();
        if let Err(error) = self.redirect(
            &command.redirects,
            &mut descriptors,
            &mut substitution_status,
        ) {
            report(&descriptors[2], &error);
            self.status = if self.exited { 2 } else { 1 };
            return;
        }
        if assignment_only {
            for word in &command.words {
                match self.expand_assignment(word, &mut substitution_status) {
                    Ok(fields) if fields.len() == 1 => self.assign(&fields[0]),
                    result => {
                        report(
                            &descriptors[2],
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
        let mut argv = argv.as_slice();
        let mut bypass_functions = false;
        while let [prefix, rest @ ..] = argv {
            if prefix != "command" {
                break;
            }
            bypass_functions = true;
            match rest {
                [flag, names @ ..] if flag == "-v" => {
                    self.status = self.describe_commands(names, &descriptors);
                    return;
                }
                [flag, ..] if flag.starts_with('-') && flag != "--" => {
                    report(&descriptors[2], "command: expected -v or a command name");
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
        if self.agent_command()
            && matches!(
                name.as_str(),
                "exit" | "fg" | "bg" | "wait" | "eval" | "." | "source" | "complete"
            )
        {
            report(
                &descriptors[2],
                &format!("{name} must be submitted by the user"),
            );
            self.status = 2;
            return;
        }
        if !bypass_functions && self.functions.contains_key(name) {
            if self.agent_command() {
                report(
                    &descriptors[2],
                    "shell functions must be submitted by the user",
                );
                self.status = 2;
            } else {
                self.call_function(name, args, input, &descriptors);
            }
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
                        &descriptors[2],
                        &format!("{name}: expected a valid enclosing loop level"),
                    );
                    self.status = 2;
                }
            }
            "local" => {
                self.status = self.set_locals(args, &descriptors[2]);
            }
            "shift" => {
                let count = match args {
                    [] => Some(1),
                    [arg] => arg.parse::<usize>().ok(),
                    _ => None,
                };
                if let Some(count) = count.filter(|&n| n <= self.args.len()) {
                    self.args.drain(..count);
                    self.status = 0;
                } else {
                    report(&descriptors[2], "shift: expected a valid argument count");
                    self.status = 1;
                }
            }
            "return" => {
                let code = match args {
                    [] => Some(self.status),
                    [arg] => arg.parse::<i64>().ok().map(|n| n as u8),
                    _ => None,
                };
                if let Some(code) = code.filter(|_| !self.locals.is_empty()) {
                    self.status = code;
                    self.returned = true;
                } else {
                    report(
                        &descriptors[2],
                        "return: expected a function and an optional numeric status",
                    );
                    self.status = 2;
                }
            }
            "eval" => self.run_nested(name, &args.join(" "), input, &descriptors),
            "." | "source" => {
                let Some((path, args)) = args.split_first() else {
                    report(&descriptors[2], &format!("{name}: expected a file"));
                    self.status = 2;
                    return;
                };
                let path = if path.contains('/') {
                    std::path::PathBuf::from(path)
                } else {
                    std::env::split_paths(&self.command_path())
                        .map(|dir| dir.join(path))
                        .find(|path| path.is_file())
                        .unwrap_or_else(|| path.into())
                };
                match std::fs::read_to_string(&path) {
                    Ok(source) => {
                        let saved = (!args.is_empty())
                            .then(|| std::mem::replace(&mut self.args, args.to_vec()));
                        self.run_nested(
                            &format!("{name}: {}", path.display()),
                            &source,
                            input,
                            &descriptors,
                        );
                        if let Some(saved) = saved {
                            self.args = saved;
                        }
                    }
                    Err(error) => {
                        report(
                            &descriptors[2],
                            &format!("{name}: {}: {error}", path.display()),
                        );
                        self.status = 1;
                    }
                }
            }
            "exit" => {
                self.exited = true;
                self.status = match args {
                    [] => self.status,
                    [arg] => match arg.parse::<i64>() {
                        Ok(value) => value as u8,
                        Err(_) => {
                            report(&descriptors[2], "exit: expected numeric status");
                            2
                        }
                    },
                    _ => {
                        report(&descriptors[2], "exit: too many arguments");
                        2
                    }
                };
            }
            "cd" | "pwd" => {
                let result = if name == "cd" {
                    self.change_directory(args, &descriptors[1])
                } else if args.is_empty() {
                    std::env::current_dir()
                        .and_then(|path| descriptors[1].write(&format!("{}\n", path.display())))
                        .map_err(|error| error.to_string())
                } else {
                    Err("arguments are not supported".into())
                };
                self.status = match result {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("{name}: {error}"));
                        1
                    }
                };
            }
            "export" => {
                self.status = 0;
                for arg in args {
                    let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
                    if !valid_name(name) {
                        report(
                            &descriptors[2],
                            "export: expected a variable name or assignment",
                        );
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
                    if let Some((array, index)) =
                        name.strip_suffix(']').and_then(|name| name.split_once('['))
                    {
                        if let Ok(index) = index.parse::<usize>() {
                            if let Some(values) = self.arrays.get_mut(array) {
                                values.remove(&index);
                            }
                            continue;
                        }
                    }
                    if !valid_name(name) {
                        report(&descriptors[2], "unset: expected a variable name");
                        self.status = 2;
                        break;
                    }
                    self.arrays.remove(name);
                    self.variables.remove(name);
                    self.exported.remove(name);
                    self.unset.insert(name.clone());
                }
            }
            "complete" => {
                self.status = match args {
                    [flag, function, commands @ ..]
                        if flag == "-F" && valid_name(function) && !commands.is_empty() =>
                    {
                        for command in commands {
                            self.completions.insert(command.clone(), function.clone());
                        }
                        0
                    }
                    _ => {
                        report(
                            &descriptors[2],
                            "complete: expected -F function command ...",
                        );
                        2
                    }
                };
            }
            "compgen" => {
                let mut matched = false;
                self.status = match args {
                    [flag, words, sep, prefix] if flag == "-W" && sep == "--" => {
                        // The word list is data, not shell source.
                        let mut result = Ok(());
                        for word in words
                            .split_ascii_whitespace()
                            .filter(|word| word.starts_with(prefix))
                        {
                            matched = true;
                            result = descriptors[1].write(&format!("{word}\n"));
                            if result.is_err() {
                                break;
                            }
                        }
                        u8::from(result.is_err() || !matched)
                    }
                    _ => {
                        report(&descriptors[2], "compgen: expected -W wordlist -- prefix");
                        2
                    }
                };
            }
            "alias" | "unalias" => {
                if let Err(error) = self.update_aliases(name, args, &descriptors[1]) {
                    report(&descriptors[2], &format!("{name}: {error}"));
                    self.status = 1;
                } else {
                    self.status = 0;
                }
            }
            "read" => {
                self.status =
                    match read_from(&descriptors[0], input, |reader| self.read(args, reader)) {
                        Ok(complete) => u8::from(!complete),
                        Err(error) => {
                            report(&descriptors[2], &format!("read: {error}"));
                            1
                        }
                    };
            }
            "printf" => {
                self.status = match printf(args)
                    .and_then(|text| descriptors[1].write(&text).map_err(|e| e.to_string()))
                {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("printf: {error}"));
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
                            .and_then(|text| descriptors[1].write(&text))
                            .map(|()| 0),
                    }
                };
                self.status = result.unwrap_or_else(|error| {
                    report(&descriptors[2], &format!("{name}: {error}"));
                    if name == "wait" { 127 } else { 1 }
                });
            }
            _ => self.status = self.external(argv, descriptors),
        }
    }

    fn call_function(
        &mut self,
        name: &str,
        args: &[String],
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        if self.script_depth >= 32 {
            report(&descriptors[2], &format!("{name}: nesting too deep"));
            self.status = 2;
            return;
        }
        let body = self.functions[name].clone();
        let previous = std::mem::replace(&mut self.args, args.to_vec());
        self.locals.push(HashMap::new());
        self.script_depth += 1;
        self.run_with_descriptors(&body, input, descriptors);
        self.script_depth -= 1;
        for (name, saved) in self.locals.pop().unwrap() {
            if let Some(value) = saved.value {
                self.variables.insert(name.clone(), value);
            } else {
                self.variables.remove(&name);
            }
            if let Some(array) = saved.array {
                self.arrays.insert(name.clone(), array);
            } else {
                self.arrays.remove(&name);
            }
            if saved.unset {
                self.unset.insert(name.clone());
            } else {
                self.unset.remove(&name);
            }
            if saved.exported {
                self.exported.insert(name);
            } else {
                self.exported.remove(&name);
            }
        }
        self.returned = false;
        self.args = previous;
    }

    fn assign_array(&mut self, name: &str, words: &[Word], local: bool) -> Result<u8, String> {
        let mut status = None;
        let values = self.expand_words(words, &mut status)?;
        if local {
            self.save_local(name)?;
        }
        self.variables.remove(name);
        self.unset.insert(name.into());
        self.arrays
            .insert(name.into(), values.into_iter().enumerate().collect());
        Ok(status.unwrap_or(0))
    }

    fn save_local(&mut self, name: &str) -> Result<(), String> {
        let frame = self
            .locals
            .last_mut()
            .ok_or("local: only available in a function")?;
        frame.entry(name.into()).or_insert_with(|| SavedVariable {
            value: self.variables.get(name).cloned(),
            array: self.arrays.get(name).cloned(),
            exported: self.exported.contains(name),
            unset: self.unset.contains(name),
        });
        Ok(())
    }

    fn set_locals(&mut self, args: &[String], output: &Descriptor) -> u8 {
        if self.locals.is_empty() {
            report(output, "local: only available in a function");
            return 2;
        }
        for arg in args {
            let (name, value) = arg.split_once('=').unwrap_or((arg, ""));
            if !valid_name(name) {
                report(output, "local: expected a variable name or assignment");
                return 2;
            }
            self.save_local(name).expect("local frame checked above");
            self.arrays.remove(name);
            self.variables.insert(name.into(), value.into());
            self.unset.remove(name);
        }
        0
    }

    fn run_nested(
        &mut self,
        name: &str,
        source: &str,
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        if self.script_depth >= 32 {
            report(&descriptors[2], &format!("{name}: nesting too deep"));
            self.status = 2;
            return;
        }
        self.script_depth += 1;
        self.status = 0;
        if let Err(error) = self.run_source_with_descriptors(source, input, descriptors) {
            report(&descriptors[2], &format!("{name}: {error}"));
            self.status = 2;
        }
        self.script_depth -= 1;
    }

    fn update_aliases(
        &mut self,
        builtin: &str,
        args: &[String],
        output: &Descriptor,
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

    fn change_directory(&mut self, args: &[String], output: &Descriptor) -> Result<(), String> {
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

    fn describe_commands(&self, names: &[String], descriptors: &Descriptors) -> u8 {
        let path = self.command_path();
        let mut status = u8::from(names.is_empty());
        for name in names {
            let description = if let Some(alias) = self.aliases.get(name) {
                format!("alias {name}='{}'", alias.replace("'", "'\\''"))
            } else if is_builtin(name) || self.functions.contains_key(name) {
                name.clone()
            } else if let Some(path) = process::find_executable(name, &path) {
                path.to_string_lossy().into_owned()
            } else {
                status = 1;
                continue;
            };
            if let Err(error) = descriptors[1].write(&format!("{description}\n")) {
                report(&descriptors[2], &format!("command: {error}"));
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
        original: &Descriptors,
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
            Descriptor::File(Rc::new(File::open("/dev/null")?))
        } else {
            original[0].clone()
        };
        let mut stages = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let next = if index + 1 == commands.len() {
                None
            } else {
                Some(process::pipe()?)
            };
            let (stdout, next_input) = match next {
                Some((read, write)) => (Descriptor::File(Rc::new(File::from(write))), Some(read)),
                None => (original[1].clone(), None),
            };
            let mut descriptors = original.clone();
            descriptors[0] = stdin;
            descriptors[1] = stdout;
            if let Some(compound) = &command.compound {
                if self.agent_command() {
                    return Err(io::Error::other(
                        "compound commands must be submitted by the user",
                    ));
                }
                stages.push(
                    self.subshell_stage(std::slice::from_ref(compound.as_ref()), &descriptors)?,
                );
                stdin = next_input.map_or(Descriptor::Closed, |read| {
                    Descriptor::File(Rc::new(File::from(read)))
                });
                continue;
            }
            let mut status = None;
            let argv = match self.expand_command_words(&command.words, &mut status) {
                Err(error) => {
                    report(&descriptors[2], &error);
                    vec!["exit".into(), "2".into()]
                }
                Ok(argv) => {
                    if let Err(error) =
                        self.redirect(&command.redirects, &mut descriptors, &mut status)
                    {
                        report(&descriptors[2], &error);
                        vec!["exit".into(), "1".into()]
                    } else if argv.is_empty() || command.words.iter().all(|word| word.assignment) {
                        vec!["exit".into(), status.unwrap_or(0).to_string()]
                    } else {
                        argv
                    }
                }
            };
            if self.exited {
                return Ok(2);
            }
            if self.agent_command()
                && matches!(
                    direct_name(&argv),
                    Some("eval" | "." | "source" | "complete")
                )
            {
                return Err(io::Error::other(
                    "eval, source and complete must be submitted by the user",
                ));
            }
            let name = argv[0].clone();
            if self.functions.contains_key(&name) && name != "command" {
                if self.agent_command() {
                    return Err(io::Error::other(
                        "shell functions must be submitted by the user",
                    ));
                }
                let command = Command {
                    words: argv.into_iter().map(Word::literal).collect(),
                    ..Command::default()
                };
                let body = [Statement::Pipeline(vec![command])];
                stages.push(self.subshell_stage(&body, &descriptors)?);
                stdin = next_input.map_or(Descriptor::Closed, |read| {
                    Descriptor::File(Rc::new(File::from(read)))
                });
                continue;
            }
            let (exec, worker_state) = if is_builtin(&name) {
                let executable = std::env::current_exe()?;
                let state = worker_payload(&PipelineVariables {
                    variables: self.variables.clone(),
                    exported: self.exported.clone(),
                    unset: self.unset.clone(),
                })?;
                let state_fd = process::duplicate(state.as_raw_fd())?;
                let mut args = vec![
                    executable
                        .to_str()
                        .ok_or_else(|| io::Error::other("shell path must be UTF-8"))?
                        .to_owned(),
                    crate::PIPELINE_BUILTIN.into(),
                    // Some runtimes reopen closed standard descriptors at startup.
                    closed_stdio(&descriptors).to_string(),
                    state_fd.as_raw_fd().to_string(),
                ];
                args.extend(argv);
                (
                    process::Exec::new(&args, &path, env.clone())?,
                    Some(state_fd),
                )
            } else {
                (process::Exec::new(&argv, &path, env.clone())?, None)
            };
            let mut child = stage(exec, &name, &descriptors)?;
            child.worker_state = worker_state;
            stages.push(child);
            match next_input {
                Some(read) => stdin = Descriptor::File(Rc::new(File::from(read))),
                None => break,
            }
        }
        self.start_stages(stages, background)
    }

    fn start_async(&mut self, statement: &Statement, original: &Descriptors) -> io::Result<u8> {
        if self.agent_command() {
            return Err(io::Error::other("agent cannot start background jobs"));
        }
        if self.substitution_depth != 0 {
            return Err(io::Error::other(
                "background jobs are not supported in shell workers",
            ));
        }
        let mut descriptors = original.clone();
        descriptors[0] = Descriptor::File(Rc::new(File::open("/dev/null")?));
        let worker = self.subshell_stage(std::slice::from_ref(statement), &descriptors)?;
        self.start_stages(vec![worker], true)
    }

    fn start_stages(&mut self, stages: Vec<process::Stage>, background: bool) -> io::Result<u8> {
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn run_editor(&mut self, argv: &[String]) -> bool {
        self.external(argv, inherited_descriptors()) == 0
    }

    fn external(&mut self, argv: &[String], mut descriptors: Descriptors) -> u8 {
        let (path, env) = self.execution_environment();
        let name = &argv[0];
        let exec = match process::Exec::new(argv, &path, env) {
            Ok(exec) => exec,
            Err(error) => {
                report(&descriptors[2], &format!("{name}: {error}"));
                return 126;
            }
        };
        if self.agent_command() {
            match File::open("/dev/null") {
                Ok(file) => descriptors[0] = Descriptor::File(Rc::new(file)),
                Err(error) => {
                    report(&descriptors[2], &format!("{name}: {error}"));
                    return 1;
                }
            }
        }
        let child = match stage(exec, name, &descriptors) {
            Ok(child) => child,
            Err(error) => {
                report(&descriptors[2], &format!("{name}: {error}"));
                return 1;
            }
        };
        self.start_stages(vec![child], false)
            .unwrap_or_else(|error| {
                report(&descriptors[2], &format!("{name}: {error}"));
                126
            })
    }

    fn read(&mut self, args: &[String], input: &mut dyn BufRead) -> Result<bool, String> {
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
    pub fn parameter_text(&mut self, value: &str) -> Result<String, String> {
        self.expand(&crate::syntax::parameter_text(value)?, &mut None)
            .map(|fields| fields.join(" "))
    }

    fn expand_words(
        &mut self,
        words: &[Word],
        status: &mut Option<u8>,
    ) -> Result<Vec<String>, String> {
        let mut fields = Vec::new();
        for word in words {
            self.expand_argument(word, status, &mut fields)?;
        }
        Ok(fields)
    }

    fn expand_command_words(
        &mut self,
        words: &[Word],
        status: &mut Option<u8>,
    ) -> Result<Vec<String>, String> {
        let mut fields = Vec::new();
        let assignment_only = words.iter().all(|word| word.assignment);
        for word in words {
            if word.assignment
                && (assignment_only || matches!(direct_name(&fields), Some("export" | "local")))
            {
                fields.extend(self.expand_assignment(word, status)?);
            } else {
                self.expand_argument(word, status, &mut fields)?;
            }
        }
        Ok(fields)
    }

    fn expand_argument(
        &mut self,
        word: &Word,
        status: &mut Option<u8>,
        output: &mut Vec<String>,
    ) -> Result<(), String> {
        let ifs = self.variable("IFS");
        for field in self.expand_parts(word, status, true, false)? {
            for field in split_field(field, ifs.as_deref().unwrap_or(DEFAULT_IFS)) {
                let chars = field.iter().flat_map(|part| {
                    let quoted = part.origin == Origin::Quoted;
                    part.text.chars().map(move |ch| (ch, quoted))
                });
                if let Some(pattern) = crate::pathname::pattern(chars)? {
                    if self.agent_command() {
                        return Err("pathname expansion must be submitted by the user".into());
                    }
                    let matches = crate::pathname::expand(pattern)?;
                    if !matches.is_empty() {
                        output.extend(matches);
                        continue;
                    }
                }
                output.push(field_text(field));
            }
        }
        Ok(())
    }

    pub fn run_substitution() -> Result<u8, String> {
        Self::run_worker(process::Stdin, None)
    }

    pub fn run_subshell(state: File, closed: u8) -> Result<u8, String> {
        Self::run_worker(state, Some(closed))
    }

    fn run_worker(state: impl Read, closed: Option<u8>) -> Result<u8, String> {
        let saved: Substitution =
            serde_json::from_reader(state).map_err(|_| "invalid shell worker state")?;
        let mut shell = Invocation::new(&saved.name, &saved.args);
        shell.variables = saved.variables;
        shell.arrays = saved.arrays;
        shell.aliases = saved.aliases;
        shell.functions = saved.functions;
        shell.exported = saved.exported;
        shell.unset = saved.unset;
        shell.substitution_depth = saved.depth;
        shell.status = saved.status;
        if let Some(closed) = closed {
            let WorkerTask::List(body) = saved.task else {
                return Err("invalid subshell task".into());
            };
            let mut descriptors = inherited_descriptors();
            for (fd, descriptor) in descriptors[..3].iter_mut().enumerate() {
                if (closed & (1 << fd)) != 0 {
                    *descriptor = Descriptor::Closed;
                }
            }
            let mut input = BufReader::with_capacity(1, process::Stdin);
            shell.run_with_descriptors(&body, &mut input, &descriptors);
            return Ok(shell.status);
        }
        process::null_stdin().map_err(|error| error.to_string())?;
        match saved.task {
            WorkerTask::Script(source) => {
                shell.run_source(&source, &mut io::empty())?;
                Ok(shell.status)
            }
            WorkerTask::Complete { function, words } => shell.complete_in_worker(&function, words),
            WorkerTask::List(_) => Err("invalid command substitution task".into()),
        }
    }

    fn complete_in_worker(&mut self, function: &str, words: Vec<String>) -> Result<u8, String> {
        if !self.functions.contains_key(function) || words.len() < 2 {
            return Ok(1);
        }
        let index = words.len() - 1;
        let args = [
            words[0].clone(),
            words[index].clone(),
            words[index - 1].clone(),
        ];
        self.arrays
            .insert("COMP_WORDS".into(), words.into_iter().enumerate().collect());
        self.arrays.insert("COMPREPLY".into(), BTreeMap::new());
        self.variables
            .insert("COMP_CWORD".into(), index.to_string());
        self.arrays.remove("COMP_CWORD");
        let null = Descriptor::File(Rc::new(
            File::options()
                .write(true)
                .open("/dev/null")
                .map_err(|error| error.to_string())?,
        ));
        let mut descriptors = inherited_descriptors();
        descriptors[1] = null.clone();
        descriptors[2] = null;
        self.call_function(function, &args, &mut io::empty(), &descriptors);
        let candidates: Vec<_> = self
            .arrays
            .get("COMPREPLY")
            .into_iter()
            .flat_map(|array| array.values())
            .filter(|value| value.len() <= 4096)
            .take(512)
            .collect();
        serde_json::to_writer(io::stdout().lock(), &candidates)
            .map_err(|_| "could not write completion candidates")?;
        Ok(0)
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn programmed_completion(&self, words: Vec<String>) -> Option<Vec<String>> {
        let function = self.completions.get(words.first()?)?;
        let command = self
            .worker_command(WorkerTask::Complete {
                function: function.clone(),
                words,
            })
            .ok()?;
        // A registered callback is trusted shell code, but cannot see the keyboard,
        // change this shell's state, print to the terminal, or stall the editor indefinitely.
        Some(crate::completion::run_callback(command).unwrap_or_default())
    }

    fn worker_state(&self, task: WorkerTask) -> Substitution {
        Substitution {
            task,
            name: self.name.into(),
            args: self.args.clone(),
            variables: self.variables.clone(),
            arrays: self.arrays.clone(),
            aliases: self.aliases.clone(),
            functions: self.functions.clone(),
            exported: self.exported.clone(),
            unset: self.unset.clone(),
            depth: self.substitution_depth + 1,
            status: self.status,
        }
    }

    fn worker_command(&self, task: WorkerTask) -> io::Result<std::process::Command> {
        let saved = self.worker_state(task);
        let mut command = std::process::Command::new(std::env::current_exe()?);
        command
            .arg(crate::SUBSTITUTION)
            .stdin(worker_payload(&saved)?);
        Ok(command)
    }

    fn subshell_stage(
        &self,
        body: &[Statement],
        descriptors: &Descriptors,
    ) -> io::Result<process::Stage> {
        if self.substitution_depth >= 32 {
            return Err(io::Error::other("subshells nested too deeply"));
        }
        let state = worker_payload(&self.worker_state(WorkerTask::List(body.to_vec())))?;
        let state_fd = process::duplicate(state.as_raw_fd())?;
        let executable = std::env::current_exe()?;
        let args = [
            executable
                .to_str()
                .ok_or_else(|| io::Error::other("shell path must be UTF-8"))?
                .into(),
            crate::SUBSHELL.into(),
            closed_stdio(descriptors).to_string(),
            state_fd.as_raw_fd().to_string(),
        ];
        let (path, env) = self.execution_environment();
        let exec = process::Exec::new(&args, &path, env)?;
        let mut child = stage(exec, "subshell", descriptors)?;
        child.worker_state = Some(state_fd);
        Ok(child)
    }

    fn substitute(&self, source: &str) -> Result<(String, u8), String> {
        const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
        if self.agent_command() {
            return Err("command substitutions must be submitted by the user".into());
        }
        if self.substitution_depth >= 16 {
            return Err("command substitutions nested too deeply".into());
        }
        let capture = || -> io::Result<_> {
            let mut command = self.worker_command(WorkerTask::Script(source.into()))?;
            command.stdout(Stdio::piped());
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

    fn expand(&mut self, word: &Word, status: &mut Option<u8>) -> Result<Vec<String>, String> {
        Ok(self
            .expand_parts(word, status, false, false)?
            .into_iter()
            .map(field_text)
            .collect())
    }

    fn expand_assignment(
        &mut self,
        word: &Word,
        status: &mut Option<u8>,
    ) -> Result<Vec<String>, String> {
        Ok(self
            .expand_parts(word, status, false, true)?
            .into_iter()
            .map(field_text)
            .collect())
    }

    fn parameter_value(&self, name: &str) -> Option<OsString> {
        if name == "#" {
            Some(self.args.len().to_string().into())
        } else if let Ok(index) = name.parse::<usize>() {
            if index == 0 {
                Some(self.name.into())
            } else {
                self.args.get(index - 1).map(OsString::from)
            }
        } else {
            self.variable_os(name)
        }
    }

    fn parameter_error(&mut self, message: String) -> String {
        if !self.interactive && !self.agent_command() {
            self.exited = true;
        }
        message
    }

    fn expand_parts(
        &mut self,
        word: &Word,
        status: &mut Option<u8>,
        field_splitting: bool,
        assignment: bool,
    ) -> Result<Vec<Field>, String> {
        let mut fields = Vec::new();
        let mut field = Field::new();
        for part in &word.parts {
            let origin = if part.quoted {
                Origin::Quoted
            } else {
                Origin::Expansion
            };
            let fragment = |text| Fragment { text, origin };
            let protected = |text: &str| Fragment {
                text: text.into(),
                origin: Origin::Quoted,
            };
            match &part.kind {
                Part::Literal(text) => field.push(Fragment {
                    text: text.clone(),
                    origin: if part.quoted {
                        Origin::Quoted
                    } else {
                        Origin::Literal
                    },
                }),
                Part::DollarQuoted(text) => {
                    field.push(protected(&syntax::decode_dollar_quote(text)?))
                }
                Part::Tilde {
                    login,
                    assignment_only,
                    followed_by_slash,
                } => {
                    let home = if *assignment_only && !assignment {
                        None
                    } else if login.is_empty() {
                        self.variable("HOME")
                    } else {
                        login_home(login)
                    };
                    if let Some(mut home) = home {
                        if *followed_by_slash && home.ends_with('/') {
                            home.pop();
                        }
                        field.push(protected(&home));
                    } else {
                        field.push(Fragment {
                            text: format!("~{login}"),
                            origin: Origin::Literal,
                        });
                    }
                }
                Part::Name => field.push(fragment(self.name.into())),
                Part::Status => field.push(fragment(self.status.to_string())),
                Part::LastBackground => {
                    #[cfg(any(target_os = "macos", target_os = "linux"))]
                    let pid = self
                        .foreground
                        .as_ref()
                        .and_then(|jobs| jobs.last_background);
                    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                    let pid: Option<u32> = None;
                    field.push(fragment(pid.map(|pid| pid.to_string()).unwrap_or_default()));
                }
                Part::Positional { star } if *star && (part.quoted || !field_splitting) => {
                    // POSIX.1-2024 2.5.2: only join $* in a non-splitting context.
                    let ifs = self.variable("IFS");
                    let separator = ifs.as_deref().unwrap_or(DEFAULT_IFS);
                    let first = separator
                        .chars()
                        .next()
                        .map_or("", |ch| &separator[..ch.len_utf8()]);
                    field.push(fragment(self.args.join(first)));
                }
                Part::Positional { .. } => {
                    let Some((first, rest)) = self.args.split_first() else {
                        continue;
                    };
                    field.push(fragment(first.clone()));
                    for arg in rest {
                        fields.push(std::mem::take(&mut field));
                        field.push(fragment(arg.clone()));
                    }
                }
                Part::Array { name, index } => {
                    let array = self.arrays.get(name);
                    match index {
                        ArrayIndex::All => {
                            if let Some(array) = array {
                                let mut values = array.values();
                                let Some(first) = values.next() else { continue };
                                field.push(protected(first));
                                for value in values {
                                    fields.push(std::mem::take(&mut field));
                                    field.push(protected(value));
                                }
                            } else if let Some(value) = self.variable(name) {
                                field.push(protected(&value));
                            } else {
                                continue;
                            }
                        }
                        _ => {
                            let mut text = String::new();
                            let index = match index {
                                ArrayIndex::Number(index) => Some(*index),
                                ArrayIndex::Variable(name) => {
                                    self.variable(name).and_then(|value| value.parse().ok())
                                }
                                ArrayIndex::All => unreachable!(),
                            };
                            if let Some(index) = index {
                                if let Some(value) = array.and_then(|array| array.get(&index)) {
                                    text.push_str(value);
                                } else if index == 0 && array.is_none() {
                                    text = self.variable(name).unwrap_or_default();
                                }
                            }
                            field.push(fragment(text));
                        }
                    }
                }
                Part::Arithmetic(expression) => {
                    let expression = self.expand(expression, status)?.join(" ");
                    let value = crate::arithmetic::evaluate(&expression, |name, assignment| {
                        if let Some(value) = assignment {
                            self.variables.insert(name.into(), value.to_string());
                            None
                        } else {
                            self.variable(name)
                        }
                    })
                    .map_err(|error| {
                        self.parameter_error(format!("arithmetic expansion: {error}"))
                    })?;
                    field.push(fragment(value.to_string()));
                }
                Part::Substitution(source) => {
                    let (text, code) = self.substitute(source)?;
                    *status = Some(code);
                    field.push(fragment(text));
                }
                Part::Trim {
                    name,
                    pattern,
                    prefix,
                    longest,
                } => {
                    let value = self.parameter_value(name).unwrap_or_default();
                    let value = value.to_string_lossy();
                    let pattern = self.expand_pattern(pattern, status)?;
                    field.push(fragment(pattern.trim(&value, *prefix, *longest).into()));
                }
                Part::Length(name) => {
                    let length = self
                        .parameter_value(name)
                        .map_or(0, |value| value.to_string_lossy().chars().count());
                    field.push(fragment(length.to_string()));
                }
                Part::Variable {
                    name,
                    default,
                    if_empty,
                    operator,
                } => {
                    let value = self
                        .parameter_value(name)
                        .filter(|value| !if_empty || !value.is_empty());
                    let alternate = *operator == ParameterOperator::Alternate;
                    if value.is_some() == alternate {
                        if matches!(
                            operator,
                            ParameterOperator::Assign | ParameterOperator::Error
                        ) {
                            let expanded = match default {
                                Some(word) => self.expand(word, status)?.join(" "),
                                None => String::new(),
                            };
                            if *operator == ParameterOperator::Error {
                                let description = if expanded.is_empty() {
                                    "parameter is unset or null".into()
                                } else {
                                    expanded
                                };
                                return Err(self.parameter_error(format!("{name}: {description}")));
                            }
                            if !valid_name(name) {
                                return Err(self.parameter_error(format!(
                                    "{name}: cannot assign to this parameter"
                                )));
                            }
                            self.variables.insert(name.clone(), expanded.clone());
                            field.push(fragment(expanded));
                            continue;
                        }
                        if let Some(default) =
                            default.as_ref().filter(|word| !word.parts.is_empty())
                        {
                            let expanded =
                                self.expand_parts(default, status, field_splitting, false)?;
                            if !field_splitting && expanded.len() > 1 {
                                let text = expanded
                                    .into_iter()
                                    .map(field_text)
                                    .collect::<Vec<_>>()
                                    .join(" ");
                                field.push(fragment(text));
                                continue;
                            }
                            let mut expanded = expanded.into_iter();
                            if let Some(first) = expanded.next() {
                                let append = |field: &mut Field, parts: Field| {
                                    field.extend(parts.into_iter().map(|mut part| {
                                        if part.origin == Origin::Literal {
                                            part.origin = origin;
                                        }
                                        part
                                    }));
                                };
                                append(&mut field, first);
                                for next in expanded {
                                    fields.push(std::mem::take(&mut field));
                                    append(&mut field, next);
                                }
                            }
                            continue;
                        }
                        field.push(fragment(String::new()));
                    } else {
                        field.push(fragment(
                            value
                                .filter(|_| !alternate)
                                .map(|value| value.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        ));
                    }
                }
            }
        }
        if !field.is_empty() {
            fields.push(field);
        }
        Ok(fields)
    }

    fn redirect(
        &mut self,
        redirects: &[Redirect],
        descriptors: &mut Descriptors,
        status: &mut Option<u8>,
    ) -> Result<(), String> {
        for redirect in redirects {
            let (fd, target, kind) = match redirect {
                Redirect::File { fd, target, kind } => (*fd, target, *kind),
                Redirect::Here { fd, document } => {
                    if self.agent_command() {
                        return Err("here-documents must be submitted by the user".into());
                    }
                    let text = document.text.borrow();
                    let expanded;
                    let text = if document.expand {
                        expanded = self.expand(&syntax::here_word(&text)?, status)?.join(" ");
                        &expanded
                    } else {
                        &text
                    };
                    // Use an anonymous file so big bodies cannot fill an unconsumed pipe.
                    let mut file = tempfile::tempfile().map_err(here_document_error)?;
                    file.write_all(text.as_bytes())
                        .map_err(here_document_error)?;
                    file.rewind().map_err(here_document_error)?;
                    descriptors[*fd] = Descriptor::File(Rc::new(file));
                    continue;
                }
            };
            if self.agent_command()
                && (!matches!(fd, 1 | 2)
                    || matches!(
                        kind,
                        RedirectKind::Read | RedirectKind::ReadWrite | RedirectKind::DuplicateInput
                    ))
            {
                return Err(
                    "this file descriptor redirection must be submitted by the user".into(),
                );
            }
            let targets = self.expand(target, status)?;
            let [target] = targets.as_slice() else {
                return Err("ambiguous redirection".into());
            };
            descriptors[fd] = match kind {
                RedirectKind::DuplicateInput | RedirectKind::DuplicateOutput => {
                    if self.agent_command() && !matches!(target.as_str(), "1" | "2") {
                        return Err(
                            "this file descriptor redirection must be submitted by the user".into(),
                        );
                    }
                    if target == "-" {
                        Descriptor::Closed
                    } else {
                        let from = syntax::descriptor_number(target).ok_or_else(|| {
                            format!("{target}: expected descriptor 0 through 9 or -")
                        })?;
                        let source = &descriptors[from];
                        if !source.is_open() {
                            return Err(format!("{from}: bad file descriptor"));
                        }
                        source.clone()
                    }
                }
                _ => {
                    let file = match kind {
                        RedirectKind::Read => File::open(target),
                        RedirectKind::ReadWrite => OpenOptions::new()
                            .read(true)
                            .write(true)
                            .create(true)
                            .truncate(false)
                            .open(target),
                        RedirectKind::Append => {
                            OpenOptions::new().append(true).create(true).open(target)
                        }
                        _ => File::create(target),
                    }
                    .map_err(|error| format!("{target}: {error}"))?;
                    Descriptor::File(Rc::new(file))
                }
            };
        }
        Ok(())
    }
}

// Send private shell variables via an anonymous fd, not argv or the environment.
fn worker_payload(state: &impl serde::Serialize) -> io::Result<File> {
    let mut payload = tempfile::tempfile()?;
    serde_json::to_writer(&mut payload, state)?;
    payload.rewind()?;
    Ok(payload)
}

fn here_document_error(error: io::Error) -> String {
    format!("here-document: {error}")
}

pub(crate) const BUILTINS: &[&str] = &[
    "exit", "cd", "pwd", "export", "unset", "read", "printf", "fg", "alias", "unalias", "jobs",
    "bg", "wait", "command", ":", "true", "false", "eval", "break", "continue", ".", "source",
    "local", "shift", "return", "complete", "compgen",
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

type Descriptors = [Descriptor; crate::SHELL_FDS];

fn inherited_descriptors() -> Descriptors {
    std::array::from_fn(|fd| Descriptor::Inherited(fd as RawFd))
}

fn closed_stdio(streams: &Descriptors) -> u8 {
    streams[..3]
        .iter()
        .enumerate()
        .fold(0, |mask, (fd, stream)| {
            mask | if matches!(stream, Descriptor::Closed) {
                1 << fd
            } else {
                0
            }
        })
}

fn stage(exec: process::Exec, name: &str, descriptors: &Descriptors) -> io::Result<process::Stage> {
    let mut streams = std::array::from_fn(|_| process::FdAction::Keep);
    for (fd, descriptor) in descriptors.iter().enumerate() {
        streams[fd] = match descriptor {
            Descriptor::Inherited(source) if *source == fd as RawFd => process::FdAction::Keep,
            Descriptor::Closed => process::FdAction::Close,
            _ => process::FdAction::Set(descriptor.duplicate()?),
        };
    }
    Ok(process::Stage {
        exec,
        streams,
        worker_state: None,
        missing: format!("zledsh: {name}: command not found\n").into_bytes(),
        failed: format!("zledsh: {name}: cannot execute\n").into_bytes(),
    })
}

#[derive(Clone)]
enum Descriptor {
    Inherited(RawFd),
    File(Rc<File>),
    Closed,
}

impl Descriptor {
    fn is_open(&self) -> bool {
        match self {
            Self::Inherited(fd) => process::inherited_descriptor(*fd),
            Self::File(_) => true,
            Self::Closed => false,
        }
    }

    fn duplicate(&self) -> io::Result<OwnedFd> {
        match self {
            Self::Inherited(fd) => process::duplicate(*fd),
            Self::File(file) => process::duplicate(file.as_raw_fd()),
            Self::Closed => Err(io::Error::from_raw_os_error(libc::EBADF)),
        }
    }

    fn write(&self, text: &str) -> io::Result<()> {
        match self {
            Self::Inherited(1) => {
                let mut output = io::stdout().lock();
                output.write_all(text.as_bytes())?;
                output.flush()
            }
            Self::Inherited(2) => io::stderr().lock().write_all(text.as_bytes()),
            Self::File(file) => file.as_ref().write_all(text.as_bytes()),
            _ => File::from(self.duplicate()?).write_all(text.as_bytes()),
        }
    }
}

fn read_from<T>(
    stream: &Descriptor,
    stdin: &mut dyn BufRead,
    read: impl FnOnce(&mut dyn BufRead) -> Result<T, String>,
) -> Result<T, String> {
    if matches!(stream, Descriptor::Inherited(0)) {
        return read(stdin);
    }
    let fd = stream.duplicate().map_err(|error| error.to_string())?;
    // Preserve unread bytes for the next builtin or external command.
    read(&mut BufReader::with_capacity(1, File::from(fd)))
}

fn report(output: &Descriptor, message: &str) {
    let _ = output.write(&format!("zledsh: {message}\n"));
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
        const CHILD: &str = "ZLEDSH_TEST_AGENT_FOREGROUND_CHILD";
        const TEST: &str =
            "runtime::tests::agent_external_command_keeps_terminal_handoff_with_null_stdin";
        if std::env::var_os(CHILD).is_some() {
            let mut invocation = Invocation::new("zledsh", &[]);
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
