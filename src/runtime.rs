use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::Stdio;
use std::rc::Rc;

use crate::{options::ShellOptions, process};

use crate::syntax::{
    self, ArrayIndex, CaseClause, Command, ParameterOperator, ParseError, Part, Redirect,
    RedirectKind, Script, Statement, Word,
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
    pub diagnostic_marker: Option<std::path::PathBuf>,
    directory_stack: Vec<String>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub interactive_history: Option<crate::history::History>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub listed_history: bool,
    name: &'a str,
    args: Vec<String>,
    getopts_offset: usize,
    persistent_fds: Descriptors,
    fd_versions: [u64; crate::SHELL_FDS],
    variables: HashMap<String, String>,
    arrays: HashMap<String, BTreeMap<usize, String>>,
    aliases: HashMap<String, String>,
    functions: HashMap<String, Script>,
    completions: HashMap<String, String>,
    exported: HashSet<String>,
    readonly: HashSet<String>,
    unset: HashSet<String>,
    substitution_depth: u8,
    inherited_background_pid: Option<libc::pid_t>,
    shell_worker: bool,
    script_depth: u8,
    sourced_depth: u8,
    loop_depth: usize,
    loop_control: Option<LoopControl>,
    locals: Vec<HashMap<String, SavedVariable>>,
    returned: bool,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub foreground: Option<crate::foreground::Foreground>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    running_agent_command: bool,
    pub status: u8,
    pub exited: bool,
    pub options: ShellOptions,
    traps: crate::traps::Traps,
    running_trap: bool,
    ignore_errexit: bool,
}

struct SavedVariable {
    value: Option<String>,
    array: Option<BTreeMap<usize, String>>,
    exported: bool,
    readonly: bool,
    getopts_offset: Option<usize>,
    unset: bool,
}

#[derive(Clone, Copy)]
enum LoopControl {
    Break(usize),
    Continue(usize),
}

#[derive(serde::Serialize, serde::Deserialize)]
struct PipelineVariables {
    name: String,
    args: Vec<String>,
    getopts_offset: usize,
    options: ShellOptions,
    trap_snapshot: BTreeMap<libc::c_int, String>,
    directory_stack: Vec<String>,
    ignore_errexit: bool,
    variables: HashMap<String, String>,
    exported: HashSet<String>,
    readonly: HashSet<String>,
    unset: HashSet<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Substitution {
    diagnostic_marker: Option<std::path::PathBuf>,
    task: WorkerTask,
    options: ShellOptions,
    trap_snapshot: BTreeMap<libc::c_int, String>,
    directory_stack: Vec<String>,
    ignore_errexit: bool,
    name: String,
    args: Vec<String>,
    getopts_offset: usize,
    variables: HashMap<String, String>,
    arrays: HashMap<String, BTreeMap<usize, String>>,
    aliases: HashMap<String, String>,
    functions: HashMap<String, Script>,
    exported: HashSet<String>,
    readonly: HashSet<String>,
    unset: HashSet<String>,
    depth: u8,
    status: u8,
    last_background_pid: Option<libc::pid_t>,
}

#[derive(serde::Serialize, serde::Deserialize)]
enum WorkerTask {
    Script(String),
    List(Script),
    Stage(Command),
    Complete {
        function: String,
        words: Vec<String>,
    },
}

impl<'a> Invocation<'a> {
    pub fn new(name: &'a str, args: &[String]) -> Self {
        Self {
            diagnostic_marker: None,
            directory_stack: Vec::new(),
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            interactive_history: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            listed_history: false,
            name,
            args: args.to_vec(),
            getopts_offset: 0,
            persistent_fds: inherited_descriptors(),
            fd_versions: [0; crate::SHELL_FDS],
            variables: HashMap::from([
                ("IFS".into(), DEFAULT_IFS.into()),
                ("OPTIND".into(), "1".into()),
            ]),
            arrays: HashMap::new(),
            aliases: HashMap::new(),
            functions: HashMap::new(),
            completions: HashMap::new(),
            exported: HashSet::new(),
            readonly: HashSet::new(),
            unset: HashSet::new(),
            substitution_depth: 0,
            inherited_background_pid: None,
            shell_worker: false,
            script_depth: 0,
            sourced_depth: 0,
            loop_depth: 0,
            loop_control: None,
            locals: Vec::new(),
            returned: false,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            foreground: None,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            running_agent_command: false,
            status: 0,
            exited: false,
            options: ShellOptions::default(),
            traps: crate::traps::Traps::default(),
            running_trap: false,
            ignore_errexit: false,
        }
    }

    pub fn parse(&self, source: &str) -> Result<Script, ParseError> {
        syntax::parse_with_aliases(source, Some(&self.aliases))
    }

    pub fn run_source(&mut self, source: &str, input: &mut impl BufRead) -> Result<(), String> {
        self.run_source_with_descriptors(source, input, &self.persistent_fds.clone())
    }

    fn run_source_with_descriptors(
        &mut self,
        source: &str,
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) -> Result<(), String> {
        let versions = self.fd_versions;
        let mut pending = String::new();
        for line in source.split_inclusive('\n') {
            pending.push_str(line);
            match self.parse(&pending) {
                Ok(commands) => {
                    let current = self.updated_descriptors(descriptors, versions);
                    self.run_with_descriptors(&commands, input, &current);
                }
                Err(ParseError::Incomplete(_)) => continue,
                Err(error) => return Err(error.message().to_owned()),
            }
            if self.exited || self.returned || self.loop_control.is_some() {
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
        self.running_agent_command = true;
        self.run_pipeline(&pipeline, &mut io::empty());
        self.running_agent_command = false;
    }

    fn agent_command(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.running_agent_command
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }

    pub fn run(&mut self, script: &[Statement], input: &mut impl BufRead) {
        self.run_with_descriptors(script, input, &self.persistent_fds.clone());
    }

    fn run_with_descriptors(
        &mut self,
        script: &[Statement],
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        let versions = self.fd_versions;
        for statement in script {
            if self.interrupted() {
                break;
            }
            let base = descriptors;
            let current = self.updated_descriptors(base, versions);
            self.dispatch_traps(input, &current);
            if self.interrupted() {
                break;
            }
            let descriptors = &current;
            match statement {
                Statement::Pipeline(pipeline) => {
                    self.run_pipeline_with_descriptors(pipeline, input, descriptors);
                    self.dispatch_traps(input, &self.updated_descriptors(base, versions));
                    self.check_errexit();
                }
                Statement::Conditional(condition) => {
                    self.status = match self.evaluate_condition(condition) {
                        Ok(matched) => u8::from(!matched),
                        Err(error) => {
                            report(&descriptors[2], &format!("[[: {error}"));
                            2
                        }
                    };
                    self.check_errexit();
                }
                Statement::Arithmetic(expression) => {
                    self.status = match self.evaluate_arithmetic(expression, &mut None) {
                        Ok(value) => u8::from(value == 0),
                        Err(error) => {
                            report(&descriptors[2], &format!("arithmetic command: {error}"));
                            if self.exited { 2 } else { 1 }
                        }
                    };
                    self.check_errexit();
                }
                Statement::Group(body) => self.run_with_descriptors(body, input, descriptors),
                Statement::Redirected { command, redirects } => {
                    let mut local = descriptors.clone();
                    if let Err(error) = self.redirect(redirects, &mut local, &mut None) {
                        report(&local[2], &error);
                        self.status = if self.exited { 2 } else { 1 };
                        self.check_errexit();
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
                    self.check_errexit();
                }
                Statement::Negated(inner) => {
                    self.run_tested_list(std::slice::from_ref(inner.as_ref()), input, descriptors);
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
                    self.run_tested_list(std::slice::from_ref(first.as_ref()), input, descriptors);
                    for (index, (on_success, statement)) in rest.iter().enumerate() {
                        if self.interrupted() {
                            break;
                        }
                        if (self.status == 0) == *on_success {
                            let current = self.updated_descriptors(base, versions);
                            let descriptors = &current;
                            let command = std::slice::from_ref(statement);
                            if index + 1 == rest.len() {
                                self.run_with_descriptors(command, input, descriptors);
                            } else {
                                self.run_tested_list(command, input, descriptors);
                            }
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
                    self.check_errexit();
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
                                self.check_errexit();
                                continue;
                            }
                        },
                        None => self.args.clone(),
                    };
                    self.status = 0;
                    self.loop_depth += 1;
                    for item in items {
                        if let Err(error) = self.assign_value(name, item) {
                            report(&descriptors[2], &error);
                            self.status = 2;
                            self.check_errexit();
                            break;
                        }
                        self.run_with_descriptors(
                            body,
                            input,
                            &self.updated_descriptors(base, versions),
                        );
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
                        self.run_tested_list(
                            condition,
                            input,
                            &self.updated_descriptors(base, versions),
                        );
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
                        self.run_with_descriptors(
                            body,
                            input,
                            &self.updated_descriptors(base, versions),
                        );
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
                            self.run_with_descriptors(
                                &clause.body,
                                input,
                                &self.updated_descriptors(base, versions),
                            );
                            if !clause.fallthrough || self.interrupted() {
                                break;
                            }
                        }
                    }
                    Ok(_) => self.status = 0,
                    Err(error) => {
                        report(&descriptors[2], &error);
                        self.status = 2;
                        self.check_errexit();
                    }
                },
                Statement::If {
                    branches,
                    otherwise,
                } => {
                    let mut selected = otherwise;
                    for (condition, body) in branches {
                        self.run_tested_list(
                            condition,
                            input,
                            &self.updated_descriptors(base, versions),
                        );
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
                        self.run_with_descriptors(
                            selected,
                            input,
                            &self.updated_descriptors(base, versions),
                        );
                    }
                }
            }
        }
    }

    fn inherit_traps(&mut self, snapshot: BTreeMap<libc::c_int, String>) {
        let pipe_ignored = snapshot.get(&libc::SIGPIPE).is_some_and(String::is_empty);
        crate::traps::inherit_pipe_ignore(pipe_ignored);
        self.traps.inherited_pipe_ignore = pipe_ignored;
        self.traps.inherited = Some(snapshot);
    }

    fn dispatch_traps(&mut self, input: &mut dyn BufRead, descriptors: &Descriptors) {
        if self.running_trap || self.exited || self.agent_command() {
            return;
        }
        for signal in crate::traps::take_pending() {
            if let Some(action) = self.traps.get(signal).map(str::to_owned) {
                self.run_trap_action(&action, input, descriptors);
                if self.exited {
                    break;
                }
            }
        }
    }

    fn run_trap_action(
        &mut self,
        action: &str,
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        let status = self.status;
        self.running_trap = true;
        if let Err(error) = self.run_source_with_descriptors(action, input, descriptors) {
            report(&descriptors[2], &format!("trap: {error}"));
        }
        self.running_trap = false;
        if !self.exited {
            self.status = status;
        }
    }

    pub fn finish(&mut self, input: &mut dyn BufRead, result: Result<u8, String>) -> u8 {
        self.status = match result {
            Ok(status) => status,
            Err(error) => {
                report(&self.persistent_fds[2], &error);
                2
            }
        };
        if self.parse_only() {
            return self.status;
        }
        self.dispatch_traps(input, &self.persistent_fds.clone());
        if let Some(action) = self.traps.take_exit() {
            self.exited = false;
            self.run_trap_action(&action, input, &self.persistent_fds.clone());
            self.exited = true;
        }
        self.status
    }

    fn trap(&mut self, args: &[String], output: &Descriptor) -> Result<(), String> {
        let (defaults, args) = match args {
            [flag, rest @ ..] if flag == "-p" => (true, rest),
            _ => (false, args),
        };
        let args = args.strip_prefix(&[String::from("--")]).unwrap_or(args);
        if defaults || args.is_empty() {
            let conditions = args
                .iter()
                .map(|name| crate::traps::condition(name))
                .collect::<Result<Vec<_>, _>>()?;
            for (name, action) in self.traps.listing(defaults, &conditions) {
                output
                    .write(&format!("trap -- {} {name}\n", quote_value(&action)))
                    .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        let (action, names) =
            if args[0].bytes().all(|byte| byte.is_ascii_digit()) && !args[0].is_empty() {
                ("-", args)
            } else {
                (args[0].as_str(), &args[1..])
            };
        if names.is_empty() {
            return Err("expected an action and at least one condition".into());
        }
        let conditions = names
            .iter()
            .map(|name| crate::traps::condition(name))
            .collect::<Result<Vec<_>, _>>()?;
        self.traps.inherited = None;
        for signal in conditions {
            self.traps
                .set(
                    signal,
                    action,
                    self.options.interactive && !self.shell_worker,
                )
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn updated_descriptors(
        &self,
        base: &Descriptors,
        versions: [u64; crate::SHELL_FDS],
    ) -> Descriptors {
        // Keep command-local redirects except where exec has since replaced a descriptor.
        std::array::from_fn(|fd| {
            if self.fd_versions[fd] == versions[fd] {
                base[fd].clone()
            } else {
                self.persistent_fds[fd].clone()
            }
        })
    }

    pub fn read_stdin_line(&self, input: &mut dyn BufRead, text: &mut String) -> io::Result<usize> {
        if matches!(self.persistent_fds[0], Descriptor::Inherited(0)) {
            return input.read_line(text);
        }
        if matches!(self.persistent_fds[0], Descriptor::Closed) {
            return Ok(0);
        }
        let fd = self.persistent_fds[0].duplicate()?;
        BufReader::with_capacity(1, File::from(fd)).read_line(text)
    }

    fn run_tested_list(
        &mut self,
        script: &[Statement],
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) {
        let previous = std::mem::replace(&mut self.ignore_errexit, true);
        self.run_with_descriptors(script, input, descriptors);
        self.ignore_errexit = previous;
    }

    fn check_errexit(&mut self) {
        if self.options.errexit && !self.ignore_errexit && self.status != 0 && !self.agent_command()
        {
            self.exited = true;
        }
    }

    fn evaluate_condition(&mut self, condition: &syntax::Condition) -> Result<bool, String> {
        use syntax::Condition;
        let value =
            |shell: &mut Self, word| shell.expand(word, &mut None).map(|fields| fields.join(" "));
        match condition {
            Condition::Not(inner) => Ok(!self.evaluate_condition(inner)?),
            Condition::All(terms) => {
                for term in terms {
                    if !self.evaluate_condition(term)? {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
            Condition::Any(terms) => {
                for term in terms {
                    if self.evaluate_condition(term)? {
                        return Ok(true);
                    }
                }
                Ok(false)
            }
            Condition::Value(word) => Ok(!value(self, word)?.is_empty()),
            Condition::Unary(operator, word) => {
                let text = value(self, word)?;
                if operator == "-v" {
                    Ok(self.variable_os(&text).is_some())
                } else {
                    Ok(crate::condition::unary(operator, &text))
                }
            }
            Condition::Binary(left, operator, right) => {
                let left = value(self, left)?;
                match operator.as_str() {
                    "=" | "==" | "!=" => {
                        let matched = self
                            .expand_pattern(right, &mut None)?
                            .matches(left.as_bytes());
                        Ok(matched != (operator == "!="))
                    }
                    "=~" => {
                        let fields = self.expand_parts(right, &mut None, false, false)?;
                        let regex = fields
                            .iter()
                            .map(|field| {
                                field
                                    .iter()
                                    .map(|part| {
                                        if part.origin == Origin::Quoted {
                                            crate::condition::quote_regex(&part.text)
                                        } else {
                                            part.text.clone()
                                        }
                                    })
                                    .collect::<String>()
                            })
                            .collect::<Vec<_>>()
                            .join(" ");
                        crate::condition::regex(&left, &regex)
                    }
                    "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
                        let right = value(self, right)?;
                        let a = self.evaluate_arithmetic_text(&left)?;
                        let b = self.evaluate_arithmetic_text(&right)?;
                        Ok(match operator.as_str() {
                            "-eq" => a == b,
                            "-ne" => a != b,
                            "-lt" => a < b,
                            "-le" => a <= b,
                            "-gt" => a > b,
                            _ => a >= b,
                        })
                    }
                    _ => Ok(crate::condition::binary(
                        operator,
                        &left,
                        &value(self, right)?,
                    )),
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

    fn parse_only(&self) -> bool {
        self.options.noexec && !self.options.interactive
    }

    fn interrupted(&self) -> bool {
        self.parse_only() || self.exited || self.returned || self.loop_control.is_some()
    }

    fn next_iteration(&mut self) -> bool {
        if self.parse_only() || self.exited || self.returned {
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
        self.run_pipeline_with_descriptors(pipeline, input, &self.persistent_fds.clone());
    }

    pub fn run_pipeline_builtin(
        command: &Command,
        input: &mut impl BufRead,
        closed: u8,
        state: File,
    ) -> serde_json::Result<u8> {
        // Consume and close the private descriptor before evaluating any commands.
        let state: PipelineVariables = serde_json::from_reader(BufReader::new(state))?;
        let mut shell = Invocation::new(&state.name, &state.args);
        shell.options = state.options;
        shell.inherit_traps(state.trap_snapshot);
        shell.directory_stack = state.directory_stack;
        shell.getopts_offset = state.getopts_offset;
        shell.ignore_errexit = state.ignore_errexit;
        shell.shell_worker = true;
        shell.variables = state.variables;
        shell.exported = state.exported;
        shell.readonly = state.readonly;
        shell.unset = state.unset;
        let mut descriptors = inherited_descriptors();
        for (fd, stream) in descriptors[..3].iter_mut().enumerate() {
            if closed & (1 << fd) != 0 {
                *stream = Descriptor::Closed;
            }
        }
        shell.run_pipeline_with_descriptors(std::slice::from_ref(command), input, &descriptors);
        Ok(shell.finish(input, Ok(shell.status)))
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
        self.run_simple_command(&pipeline[0], input, original, false);
    }

    fn run_simple_command(
        &mut self,
        command: &Command,
        input: &mut dyn BufRead,
        original: &Descriptors,
        pipeline_stage: bool,
    ) {
        let mut substitution_status = None;
        let prefix_len = command
            .words
            .iter()
            .take_while(|word| word.assignment)
            .count();
        let (prefix, words) = command.words.split_at(prefix_len);
        if !prefix.is_empty() && !words.is_empty() && self.agent_command() {
            report(
                &original[2],
                "command assignment prefixes must be submitted by the user",
            );
            self.status = 2;
            return;
        }
        let argv = match self.expand_command_words(words, &mut substitution_status) {
            Ok(argv) => argv,
            Err(error) => {
                report(&original[2], &error);
                self.status = 2;
                return;
            }
        };
        let mut descriptors = original.clone();
        let direct_special = argv.first().is_some_and(|name| is_special_builtin(name));
        let mut applied = 0;
        let result = command.redirects.iter().try_for_each(|redirect| {
            self.redirect(
                std::slice::from_ref(redirect),
                &mut descriptors,
                &mut substitution_status,
            )?;
            applied += 1;
            Ok::<(), String>(())
        });
        if let Err(error) = result {
            report(&descriptors[2], &error);
            self.status = if self.exited { 2 } else { 1 };
            self.special_builtin_error(direct_special);
            if !self.exited && !self.agent_command() && direct_name(&argv) == Some("exec") {
                self.commit_exec_redirects(&command.redirects[..applied], &descriptors);
            }
            return;
        }
        let temporary = !argv.is_empty() && !direct_special;
        let export_prefix = temporary || argv.first().is_some_and(|name| name == "exec");
        let mut saved = HashMap::new();
        let result = (|| -> Result<(), String> {
            for word in prefix {
                let fields = self.expand_assignment(word, &mut substitution_status)?;
                let [field] = fields.as_slice() else {
                    return Err("assignment must expand to one value".into());
                };
                let (name, _) = field.split_once('=').ok_or("expected an assignment")?;
                self.ensure_writable(name)?;
                if temporary {
                    saved
                        .entry(name.to_owned())
                        .or_insert_with(|| self.save_variable(name));
                }
                if export_prefix {
                    self.exported.insert(name.into());
                }
                self.assign(field)?;
            }
            Ok(())
        })();
        if let Err(error) = result {
            let error = self.parameter_error(error);
            report(&descriptors[2], &error);
            self.status = 2;
        } else {
            self.execute_argv(
                &argv,
                input,
                descriptors,
                substitution_status,
                &command.redirects,
                pipeline_stage,
            );
        }
        self.restore_variables(saved);
    }

    fn execute_argv(
        &mut self,
        mut argv: &[String],
        input: &mut dyn BufRead,
        descriptors: Descriptors,
        substitution_status: Option<u8>,
        redirects: &[Redirect],
        pipeline_stage: bool,
    ) {
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
                "exit"
                    | "fg"
                    | "bg"
                    | "wait"
                    | "eval"
                    | "exec"
                    | "."
                    | "source"
                    | "complete"
                    | "set"
                    | "readonly"
                    | "umask"
                    | "ulimit"
                    | "fc"
                    | "pushd"
                    | "popd"
                    | "dirs"
                    | "trap"
            )
        {
            report(
                &descriptors[2],
                &format!("{name} must be submitted by the user"),
            );
            self.status = 2;
            return;
        }
        if !bypass_functions && !is_special_builtin(name) && self.functions.contains_key(name) {
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
        let direct_special = !bypass_functions && is_special_builtin(name);
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
                    self.special_builtin_error(direct_special);
                }
            }
            "local" => {
                self.status = self.set_locals(args, &descriptors[2]);
            }
            "set" => {
                self.status = match self.set(args, &descriptors[1]) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("set: {error}"));
                        self.special_builtin_error(direct_special);
                        2
                    }
                };
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
                    self.special_builtin_error(direct_special);
                }
            }
            "return" => {
                let code = match args {
                    [] => Some(self.status),
                    [arg] => arg.parse::<i64>().ok().map(|n| n as u8),
                    _ => None,
                };
                if let Some(code) =
                    code.filter(|_| self.sourced_depth > 0 || !self.locals.is_empty())
                {
                    self.status = code;
                    self.returned = true;
                } else {
                    report(
                        &descriptors[2],
                        "return: expected a function or sourced file and an optional numeric status",
                    );
                    self.status = 2;
                    self.special_builtin_error(direct_special);
                }
            }
            "eval" => {
                if !self.run_nested(name, &args.join(" "), input, &descriptors) {
                    self.special_builtin_error(direct_special);
                }
            }
            "." | "source" => {
                let Some((path, args)) = args.split_first() else {
                    report(&descriptors[2], &format!("{name}: expected a file"));
                    self.status = 2;
                    self.special_builtin_error(direct_special);
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
                        self.sourced_depth += 1;
                        let ran = self.run_nested(
                            &format!("{name}: {}", path.display()),
                            &source,
                            input,
                            &descriptors,
                        );
                        self.sourced_depth -= 1;
                        self.returned = false;
                        if let Some(saved) = saved {
                            self.args = saved;
                        }
                        if !ran {
                            self.special_builtin_error(direct_special);
                        }
                    }
                    Err(error) => {
                        report(
                            &descriptors[2],
                            &format!("{name}: {}: {error}", path.display()),
                        );
                        self.status = 1;
                        self.special_builtin_error(direct_special);
                    }
                }
            }
            "exec" => {
                let args = args.strip_prefix(&[String::from("--")]).unwrap_or(args);
                if args.is_empty() {
                    self.commit_exec_redirects(redirects, &descriptors);
                    self.status = 0;
                } else {
                    self.status = self.replace_shell(args, &descriptors);
                    self.commit_exec_redirects(redirects, &descriptors);
                    self.special_builtin_error(true);
                }
            }
            "exit" => {
                let code = match args {
                    [] => Ok(self.status),
                    [arg] => arg
                        .parse::<i64>()
                        .map(|value| value as u8)
                        .map_err(|_| "expected numeric status"),
                    _ => Err("too many arguments"),
                };
                match code {
                    Ok(code) => {
                        self.exited = true;
                        self.status = code;
                    }
                    Err(error) => {
                        report(&descriptors[2], &format!("exit: {error}"));
                        self.status = 2;
                        self.special_builtin_error(direct_special);
                    }
                }
            }
            "pushd" | "popd" | "dirs" => {
                self.status = match self.directory_stack_command(name, args, &descriptors[1]) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("{name}: {error}"));
                        1
                    }
                };
            }
            "cd" | "pwd" => {
                let result = if name == "cd" {
                    self.change_directory(args, Some(&descriptors[1]))
                } else {
                    self.print_directory(args, &descriptors[1])
                };
                self.status = match result {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("{name}: {error}"));
                        1
                    }
                };
            }
            "trap" => {
                self.status = match self.trap(args, &descriptors[1]) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("trap: {error}"));
                        self.special_builtin_error(direct_special);
                        2
                    }
                };
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            "fc" => {
                self.status = match self
                    .interactive_history
                    .as_mut()
                    .ok_or_else(|| "requires interactive shell history".to_owned())
                    .and_then(|history| history.list(args))
                    .and_then(|listing| {
                        self.listed_history = true;
                        descriptors[1]
                            .write(&listing)
                            .map_err(|error| error.to_string())
                    }) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("fc: {error}"));
                        1
                    }
                };
            }
            "ulimit" => {
                self.status = match crate::ulimit::run(args).and_then(|output| {
                    if output.is_empty() {
                        Ok(())
                    } else {
                        descriptors[1]
                            .write(&output)
                            .map_err(|error| error.to_string())
                    }
                }) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("ulimit: {error}"));
                        1
                    }
                };
            }
            "umask" => {
                self.status = match umask(args, &descriptors[1]) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("umask: {error}"));
                        1
                    }
                };
            }
            "getopts" => {
                self.status = match self.getopts(args, &descriptors[2]) {
                    Ok(status) => status,
                    Err(error) => {
                        report(&descriptors[2], &format!("{}: getopts: {error}", self.name));
                        2
                    }
                };
            }
            "readonly" => {
                self.status = match self.mark_readonly(args, &descriptors[1]) {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("readonly: {error}"));
                        self.special_builtin_error(direct_special);
                        2
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
                    if let Err(error) = self.assign(arg) {
                        report(&descriptors[2], &format!("export: {error}"));
                        self.status = 2;
                        break;
                    }
                    self.exported.insert(name.to_owned());
                }
                if self.status != 0 {
                    self.special_builtin_error(direct_special);
                }
            }
            "unset" => {
                self.status = 0;
                for name in args.strip_prefix(&[String::from("-v")]).unwrap_or(args) {
                    if let Some((array, index)) =
                        name.strip_suffix(']').and_then(|name| name.split_once('['))
                    {
                        if let Ok(index) = index.parse::<usize>() {
                            if let Err(error) = self.ensure_writable(array) {
                                report(&descriptors[2], &format!("unset: {error}"));
                                self.status = 2;
                                break;
                            }
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
                    if let Err(error) = self.ensure_writable(name) {
                        report(&descriptors[2], &format!("unset: {error}"));
                        self.status = 2;
                        break;
                    }
                    if name == "OPTIND" {
                        self.getopts_offset = 0;
                    }
                    self.arrays.remove(name);
                    self.variables.remove(name);
                    self.exported.remove(name);
                    self.unset.insert(name.clone());
                }
                if self.status != 0 {
                    self.special_builtin_error(direct_special);
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
                self.status = match read_from(&descriptors[0], input, |reader| {
                    self.read(args, reader, &descriptors)
                }) {
                    Ok(complete) => u8::from(!complete),
                    Err(error) => {
                        report(&descriptors[2], &format!("read: {error}"));
                        2
                    }
                };
            }
            "print" | "printf" => {
                let text = if name == "print" {
                    print(args)
                } else {
                    printf(args)
                };
                self.status = match text
                    .and_then(|text| descriptors[1].write(&text).map_err(|e| e.to_string()))
                {
                    Ok(()) => 0,
                    Err(error) => {
                        report(&descriptors[2], &format!("{name}: {error}"));
                        1
                    }
                };
            }
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            "fg" | "bg" | "jobs" | "wait" => {
                if pipeline_stage {
                    report(
                        &descriptors[2],
                        &format!("{name}: not available in a pipeline stage"),
                    );
                    self.status = 1;
                    return;
                }
                let jobs = self
                    .foreground
                    .get_or_insert_with(crate::foreground::Foreground::noninteractive);
                let selector = args.first().map(String::as_str);
                let result = if matches!(name.as_str(), "fg" | "bg") && args.len() > 1 {
                    Err(io::Error::other("unexpected job arguments"))
                } else {
                    match name.as_str() {
                        "fg" => jobs.resume(selector),
                        "bg" => jobs.background(selector),
                        "wait" => jobs.wait_background(args),
                        _ => jobs
                            .listing(args)
                            .and_then(|text| descriptors[1].write(&text))
                            .map(|()| 0),
                    }
                };
                self.status = result.unwrap_or_else(|error| {
                    report(&descriptors[2], &format!("{name}: {error}"));
                    1
                });
            }
            _ => self.status = self.external(argv, descriptors, pipeline_stage),
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
        let locals = self.locals.pop().unwrap();
        self.restore_variables(locals);
        self.returned = false;
        self.args = previous;
    }

    fn restore_variables(&mut self, variables: HashMap<String, SavedVariable>) {
        for (name, saved) in variables {
            if let Some(offset) = saved.getopts_offset {
                self.getopts_offset = offset;
            }
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
            if saved.readonly {
                self.readonly.insert(name.clone());
            } else {
                self.readonly.remove(&name);
            }
            if saved.exported {
                self.exported.insert(name);
            } else {
                self.exported.remove(&name);
            }
        }
    }

    fn assign_array(&mut self, name: &str, words: &[Word], local: bool) -> Result<u8, String> {
        self.ensure_writable(name)?;
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
        let saved = self.save_variable(name);
        let frame = self
            .locals
            .last_mut()
            .ok_or("local: only available in a function")?;
        frame.entry(name.into()).or_insert(saved);
        Ok(())
    }

    fn save_variable(&self, name: &str) -> SavedVariable {
        SavedVariable {
            value: self.variables.get(name).cloned(),
            array: self.arrays.get(name).cloned(),
            exported: self.exported.contains(name),
            readonly: self.readonly.contains(name),
            getopts_offset: (name == "OPTIND").then_some(self.getopts_offset),
            unset: self.unset.contains(name),
        }
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
            if let Err(error) = self.ensure_writable(name) {
                report(output, &format!("local: {error}"));
                return 2;
            }
            self.save_local(name).expect("local frame checked above");
            self.arrays.remove(name);
            self.assign_value(name, value.into())
                .expect("writable variable checked above");
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
    ) -> bool {
        if self.script_depth >= 32 {
            report(&descriptors[2], &format!("{name}: nesting too deep"));
            self.status = 2;
            return false;
        }
        self.script_depth += 1;
        self.status = 0;
        let result = self.run_source_with_descriptors(source, input, descriptors);
        self.script_depth -= 1;
        if let Err(error) = result {
            report(&descriptors[2], &format!("{name}: {error}"));
            self.status = 2;
            return false;
        }
        true
    }

    fn special_builtin_error(&mut self, direct: bool) {
        if direct && (!self.options.interactive || self.shell_worker) && !self.agent_command() {
            self.exited = true;
        }
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

    fn print_directory(&self, args: &[String], output: &Descriptor) -> Result<(), String> {
        let (physical, _, operands) = directory_options(args, false)?;
        if !operands.is_empty() {
            return Err("expected pwd [-L|-P]".into());
        }
        self.working_directory(physical)
            .and_then(|path| output.write(&format!("{}\n", path.display())))
            .map_err(|error| error.to_string())
    }

    fn working_directory(&self, physical: bool) -> io::Result<std::path::PathBuf> {
        if physical {
            std::env::current_dir()
        } else {
            crate::directory::logical(self.variable_os("PWD").as_deref().map(std::path::Path::new))
        }
    }

    fn change_directory(
        &mut self,
        args: &[String],
        output: Option<&Descriptor>,
    ) -> Result<(), String> {
        use std::path::{Path, PathBuf};

        let (physical, require_pwd, operands) = directory_options(args, true)?;
        let print_previous = operands == ["-"];
        let destination = match operands {
            [] => self.variable_os("HOME").ok_or("HOME is not set")?,
            [_] if print_previous => self.variable_os("OLDPWD").ok_or("OLDPWD is not set")?,
            [path] => path.into(),
            _ => return Err("expected at most one directory".into()),
        };
        if destination.is_empty() {
            return Err("empty directory name".into());
        }
        self.ensure_writable("PWD")?;
        self.ensure_writable("OLDPWD")?;
        let previous = self
            .working_directory(false)
            .map_err(|error| error.to_string())?;
        let operand = Path::new(&destination);
        let mut path = PathBuf::from(operand);
        let mut print = print_previous;
        if !operand.is_absolute() && !operand.starts_with(".") && !operand.starts_with("..") {
            if let Some(cdpath) = self.variable("CDPATH") {
                for entry in cdpath.split(':') {
                    let candidate = Path::new(entry).join(operand);
                    if candidate.is_dir() {
                        path = candidate;
                        print |= !entry.is_empty();
                        break;
                    }
                }
            }
        }
        let chdir_error = |error| format!("{}: {error}", path.display());
        let logical = if physical {
            None
        } else {
            Some(crate::directory::normalize(&previous.join(&path)).map_err(chdir_error)?)
        };
        std::env::set_current_dir(logical.as_deref().unwrap_or(&path)).map_err(chdir_error)?;
        self.assign_value("OLDPWD", previous.to_string_lossy().into_owned())?;
        self.exported.insert("OLDPWD".into());
        let current = match logical.map_or_else(std::env::current_dir, Ok) {
            Ok(path) => path,
            Err(error) if require_pwd && physical => return Err(error.to_string()),
            Err(_) => return Ok(()),
        };
        self.assign_value("PWD", current.to_string_lossy().into_owned())?;
        self.exported.insert("PWD".into());
        if let Some(output) = output.filter(|_| print) {
            output
                .write(&format!("{}\n", current.display()))
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    fn directory_stack_command(
        &mut self,
        name: &str,
        args: &[String],
        output: &Descriptor,
    ) -> Result<(), String> {
        let (stay, args) = match args {
            [flag, rest @ ..] if flag == "-n" && name != "dirs" => (true, rest),
            _ => (false, args),
        };
        let quoted_path = args.first().is_some_and(|arg| arg == "--");
        let args = if quoted_path { &args[1..] } else { args };
        let cwd = self
            .working_directory(false)
            .map_err(|error| error.to_string())?
            .into_os_string()
            .into_string()
            .map_err(|_| "directory is not UTF-8")?;
        let mut stack = vec![cwd.clone()];
        stack.extend(self.directory_stack.iter().cloned());
        let index = |arg: &str| -> Result<usize, String> {
            let number = arg
                .get(1..)
                .and_then(|n| n.parse::<usize>().ok())
                .filter(|&n| n < stack.len())
                .ok_or("invalid stack index")?;
            Ok(if arg.starts_with('-') {
                stack.len() - number - 1
            } else {
                number
            })
        };
        match (name, args) {
            ("dirs", []) => {}
            ("pushd", []) if stay => {}
            ("pushd", []) if stack.len() > 1 => stack.swap(0, 1),
            ("pushd", [arg]) if !quoted_path && arg.starts_with(['+', '-']) => {
                let index = index(arg)?;
                if !stay {
                    stack.rotate_left(index);
                }
            }
            ("pushd", [path]) if stay => {
                let absolute = std::path::Path::new(&cwd).join(path);
                stack.insert(1, absolute.to_string_lossy().into_owned());
            }
            ("pushd", [path]) => {
                self.change_directory(&["--".into(), path.clone()], None)?;
                stack.insert(
                    0,
                    self.working_directory(false)
                        .map_err(|error| error.to_string())?
                        .to_string_lossy()
                        .into_owned(),
                );
            }
            ("popd", [] | [_]) if stack.len() > 1 => {
                let index = match args {
                    [arg] if arg.starts_with(['+', '-']) => index(arg)?,
                    [] => 0,
                    _ => return Err("expected a stack index".into()),
                };
                stack.remove(if stay && index == 0 { 1 } else { index });
            }
            ("pushd" | "popd", []) => return Err("directory stack is empty".into()),
            _ => return Err("unexpected directory stack arguments".into()),
        }
        if self
            .working_directory(false)
            .map_err(|error| error.to_string())?
            != std::path::Path::new(&stack[0])
        {
            self.change_directory(&["--".into(), stack[0].clone()], None)?;
        }
        self.directory_stack = stack[1..].to_vec();
        output
            .write(&format!("{}\n", stack.join(" ")))
            .map_err(|error| error.to_string())
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

    fn set(&mut self, args: &[String], output: &Descriptor) -> Result<(), String> {
        if args.is_empty() {
            let mut variables: BTreeMap<_, _> = std::env::vars_os()
                .filter_map(|(name, value)| {
                    Some((name.into_string().ok()?, value.into_string().ok()?))
                })
                .filter(|(name, _)| valid_name(name) && !self.unset.contains(name))
                .collect();
            variables.extend(self.variables.clone());
            for (name, value) in variables {
                let quoted = quote_value(&value);
                output
                    .write(&format!("{name}={quoted}\n"))
                    .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        let mut remaining = args;
        let mut options = self.options;
        while let [flag, rest @ ..] = remaining {
            match flag.as_str() {
                "-o" | "+o" => {
                    let enabled = flag == "-o";
                    if let Some((name, rest)) = rest.split_first() {
                        options.set_named(name, enabled)?;
                        remaining = rest;
                    } else {
                        for (name, active) in options.named() {
                            let line = if enabled {
                                format!("{name} {}\n", if active { "on" } else { "off" })
                            } else {
                                format!("set {}o {name}\n", if active { '-' } else { '+' })
                            };
                            output.write(&line).map_err(|error| error.to_string())?;
                        }
                        break;
                    }
                }
                "--" => {
                    self.args = rest.to_vec();
                    break;
                }
                value if value.len() > 1 && value.starts_with(['-', '+']) => {
                    for letter in value[1..].chars() {
                        options.set_short(letter, value.starts_with('-'))?;
                    }
                    remaining = rest;
                }
                _ => {
                    self.args = remaining.to_vec();
                    break;
                }
            }
        }
        self.options = options;
        Ok(())
    }

    fn command_path(&self) -> OsString {
        self.variable_os("PATH")
            .unwrap_or_else(|| OsString::from("/usr/bin:/bin"))
    }

    fn execution_environment(&self) -> (OsString, HashMap<OsString, OsString>) {
        let path = self.command_path();
        let mut env: HashMap<OsString, OsString> = std::env::vars_os()
            .filter(|(name, _)| {
                !name.to_str().is_some_and(|name| {
                    self.unset.contains(name)
                        || (matches!(name, "OPTIND" | "OPTARG") && !self.exported.contains(name))
                })
            })
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
            if !self.agent_command() {
                let child = if let Some(compound) = &command.compound {
                    self.subshell_stage(std::slice::from_ref(compound.as_ref()), &descriptors)?
                } else {
                    self.worker_stage(WorkerTask::Stage(command.clone()), &descriptors)?
                };
                stages.push(child);
                stdin = next_input.map_or(Descriptor::Closed, |read| {
                    Descriptor::File(Rc::new(File::from(read)))
                });
                continue;
            }
            // Assistant commands must pass the existing checks before launch;
            // never send an unchecked assistant command to a shell worker.
            if command.compound.is_some() {
                return Err(io::Error::other(
                    "compound commands must be submitted by the user",
                ));
            }
            if command.words.first().is_some_and(|word| word.assignment) {
                return Err(io::Error::other(
                    "command assignment prefixes must be submitted by the user",
                ));
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
                    } else if argv.is_empty() {
                        vec!["exit".into(), status.unwrap_or(0).to_string()]
                    } else {
                        argv
                    }
                }
            };
            if self.exited {
                return Ok(2);
            }
            if matches!(
                direct_name(&argv),
                Some(
                    "eval"
                        | "exec"
                        | "."
                        | "source"
                        | "complete"
                        | "set"
                        | "readonly"
                        | "umask"
                        | "ulimit"
                        | "fc"
                        | "pushd"
                        | "popd"
                        | "dirs"
                        | "trap"
                )
            ) {
                return Err(io::Error::other(
                    "this builtin must be submitted by the user",
                ));
            }
            let name = argv[0].clone();
            if !is_special_builtin(&name) && self.functions.contains_key(&name) && name != "command"
            {
                return Err(io::Error::other(
                    "shell functions must be submitted by the user",
                ));
            }
            let (exec, worker_state) = if is_builtin(&name) {
                let executable = std::env::current_exe()?;
                let state = worker_payload(&PipelineVariables {
                    name: self.name.into(),
                    args: self.args.clone(),
                    getopts_offset: self.getopts_offset,
                    options: self.options,
                    trap_snapshot: self.traps.snapshot(),
                    directory_stack: self.directory_stack.clone(),
                    ignore_errexit: self.ignore_errexit,
                    variables: self.variables.clone(),
                    exported: self.exported.clone(),
                    readonly: self.readonly.clone(),
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
            let agent_command = self.agent_command();
            if background {
                return self
                    .foreground
                    .get_or_insert_with(crate::foreground::Foreground::noninteractive)
                    .run_background(stages, self.options.pipefail);
            }
            if let Some(tty) = self.foreground.as_mut().filter(|tty| tty.has_terminal()) {
                return tty.run_pipeline(stages, self.options.pipefail, agent_command);
            }
        }
        let (mut job, gate) = process::Pipeline::start(stages, false, self.options.pipefail)?;
        drop(gate);
        match job.wait()? {
            process::Outcome::Exited { code, .. } | process::Outcome::Stopped(code) => Ok(code),
        }
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn run_editor(&mut self, argv: &[String]) -> bool {
        self.external(argv, inherited_descriptors(), false) == 0
    }

    fn commit_exec_redirects(&mut self, redirects: &[Redirect], descriptors: &Descriptors) {
        for redirect in redirects {
            let fd = match redirect {
                Redirect::File { fd, .. }
                | Redirect::Here { fd, .. }
                | Redirect::HereString { fd, .. } => *fd,
            };
            self.fd_versions[fd] += 1;
            self.persistent_fds[fd] = descriptors[fd].clone();
        }
    }

    fn replace_shell(&self, argv: &[String], descriptors: &Descriptors) -> u8 {
        let (path, env) = self.execution_environment();
        let name = &argv[0];
        let error = match process::Exec::new(argv, &path, env)
            .and_then(|exec| stage(exec, name, descriptors))
        {
            Ok(stage) => process::replace(&stage),
            Err(error) => error,
        };
        if error.raw_os_error() == Some(libc::ENOENT) {
            report(&descriptors[2], &format!("exec: {name}: command not found"));
            127
        } else {
            report(&descriptors[2], &format!("exec: {name}: {error}"));
            126
        }
    }

    fn external(
        &mut self,
        argv: &[String],
        mut descriptors: Descriptors,
        pipeline_stage: bool,
    ) -> u8 {
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
        if pipeline_stage {
            // The stage PID is already registered with the parent job. Keep it
            // when launching an external command instead of waiting on a child.
            let error = process::replace(&child);
            let (message, status) = if error.raw_os_error() == Some(libc::ENOENT) {
                ("command not found", 127)
            } else {
                ("cannot execute", 126)
            };
            report(&descriptors[2], &format!("{name}: {message}"));
            return status;
        }
        self.start_stages(vec![child], false)
            .unwrap_or_else(|error| {
                report(&descriptors[2], &format!("{name}: {error}"));
                126
            })
    }

    fn read(
        &mut self,
        args: &[String],
        input: &mut dyn BufRead,
        descriptors: &Descriptors,
    ) -> Result<bool, String> {
        let mut raw = false;
        let mut delimiter = b'\n';
        let mut names = args;
        while let [flag, rest @ ..] = names {
            match flag.as_str() {
                "-r" => raw = true,
                "-d" | "-rd" | "-dr" => {
                    raw |= flag != "-d";
                    let (value, rest) = rest.split_first().ok_or("-d requires a delimiter")?;
                    delimiter = match value.as_bytes() {
                        [] => 0,
                        [byte] => *byte,
                        _ => return Err("-d requires one byte or an empty delimiter".into()),
                    };
                    names = rest;
                    continue;
                }
                "--" => {
                    names = rest;
                    break;
                }
                _ => break,
            }
            names = rest;
        }
        if names.is_empty() || names.iter().any(|name| !valid_name(name)) {
            return Err("expected read [-r] [-d delimiter] name [name ...]".into());
        }
        let mut characters = Vec::new();
        let mut line = Vec::new();
        let complete = 'lines: loop {
            line.clear();
            input
                .read_until(delimiter, &mut line)
                .map_err(|error| error.to_string())?;
            let terminated = line.last() == Some(&delimiter);
            if terminated {
                line.pop();
            }
            let mut chars = std::str::from_utf8(&line)
                .map_err(|_| "input is not UTF-8")?
                .chars();
            while let Some(ch) = chars.next() {
                if ch == '\\' && !raw {
                    match chars.next() {
                        Some(escaped) => characters.push((escaped, true)),
                        None if terminated => {
                            #[cfg(any(target_os = "macos", target_os = "linux"))]
                            if self.options.interactive
                                && delimiter == b'\n'
                                && descriptors[0].is_terminal()
                                && !self.agent_command()
                            {
                                let prompt = crate::prompt::render(self, true, 0);
                                let _ = descriptors[2].write(&prompt);
                            }
                            continue 'lines;
                        }
                        None => break 'lines false,
                    }
                } else {
                    characters.push((ch, false));
                }
            }
            break terminated;
        };
        let ifs = self.variable("IFS");
        let fields = crate::read_fields::split(
            &characters,
            ifs.as_deref().unwrap_or(DEFAULT_IFS),
            names.len(),
        );
        for (name, value) in names.iter().zip(fields) {
            self.assign_value(name, value)?;
        }
        Ok(complete)
    }

    fn getopts(&mut self, args: &[String], stderr: &Descriptor) -> Result<u8, String> {
        let [spec, name, explicit @ ..] = args else {
            return Err("expected getopts optstring name [arg ...]".into());
        };
        if !valid_name(name) {
            return Err("expected a variable name".into());
        }
        for target in [name.as_str(), "OPTARG", "OPTIND"] {
            self.ensure_writable(target)?;
        }
        let index = self
            .variable("OPTIND")
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|index| *index > 0)
            .unwrap_or(1);
        let parameters = if explicit.is_empty() {
            &self.args
        } else {
            explicit
        };
        let step = crate::getopts::next(spec, parameters, index, self.getopts_offset);
        self.assign_value("OPTIND", step.index.to_string())?;
        self.getopts_offset = step.offset;
        if let Some(argument) = step.argument {
            self.assign_value("OPTARG", argument)?;
        } else {
            self.variables.remove("OPTARG");
            self.exported.remove("OPTARG");
            self.unset.insert("OPTARG".into());
        }
        self.assign_value(name, step.option.to_string())?;
        if let Some(diagnostic) = step.diagnostic {
            report(stderr, &format!("{}: getopts: {diagnostic}", self.name));
        }
        Ok(step.status)
    }

    fn mark_readonly(&mut self, args: &[String], output: &Descriptor) -> Result<(), String> {
        if args.is_empty() || args == ["-p"] {
            let mut names: Vec<_> = self.readonly.iter().collect();
            names.sort_unstable();
            for name in names {
                let binding = match self.variable(name) {
                    Some(value) => format!("{name}={}", quote_value(&value)),
                    None => name.clone(),
                };
                output
                    .write(&format!("readonly {binding}\n"))
                    .map_err(|error| error.to_string())?;
            }
            return Ok(());
        }
        for arg in args.strip_prefix(&[String::from("--")]).unwrap_or(args) {
            let name = arg.split_once('=').map_or(arg.as_str(), |(name, _)| name);
            if !valid_name(name) {
                return Err("expected a variable name or assignment".into());
            }
            self.assign(arg)?;
            self.readonly.insert(name.into());
        }
        Ok(())
    }

    fn ensure_writable(&self, name: &str) -> Result<(), String> {
        if self.readonly.contains(name) {
            Err(format!("{name}: read-only variable"))
        } else {
            Ok(())
        }
    }

    fn assign(&mut self, assignment: &str) -> Result<(), String> {
        if let Some((name, value)) = assignment.split_once('=') {
            self.assign_value(name, value.into())?;
        }
        Ok(())
    }

    fn assign_value(&mut self, name: &str, value: String) -> Result<(), String> {
        self.ensure_writable(name)?;
        if name == "OPTIND" {
            self.getopts_offset = 0;
        }
        self.variables.insert(name.into(), value);
        if self.options.allexport {
            self.exported.insert(name.into());
        }
        Ok(())
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

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn expand_prompt(&mut self, value: &str) -> Result<String, String> {
        self.expand(&syntax::here_word(value)?, &mut None)
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
        for word in words {
            if word.assignment
                && matches!(direct_name(&fields), Some("export" | "local" | "readonly"))
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
        if let Some(alternatives) = crate::brace::alternatives(word)? {
            if self.agent_command() {
                return Err("brace expansion must be submitted by the user".into());
            }
            for alternative in alternatives {
                self.expand_argument_fields(&alternative, status, output)?;
            }
            return Ok(());
        }
        self.expand_argument_fields(word, status, output)
    }

    fn expand_argument_fields(
        &mut self,
        word: &Word,
        status: &mut Option<u8>,
        output: &mut Vec<String>,
    ) -> Result<(), String> {
        let ifs = self.variable("IFS");
        for field in self.expand_parts(word, status, true, false)? {
            for field in split_field(field, ifs.as_deref().unwrap_or(DEFAULT_IFS)) {
                if self.options.noglob && !self.agent_command() {
                    output.push(field_text(field));
                    continue;
                }
                let chars = field.iter().flat_map(|part| {
                    let quoted = part.origin == Origin::Quoted;
                    part.text.chars().map(move |ch| (ch, quoted))
                });
                if let Some(pattern) = crate::pathname::pattern(chars, self.options.globstar)? {
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
        shell.options = saved.options;
        shell.diagnostic_marker = saved.diagnostic_marker;
        shell.inherit_traps(saved.trap_snapshot);
        shell.directory_stack = saved.directory_stack;
        shell.getopts_offset = saved.getopts_offset;
        shell.ignore_errexit = saved.ignore_errexit;
        shell.shell_worker = true;
        shell.variables = saved.variables;
        shell.arrays = saved.arrays;
        shell.aliases = saved.aliases;
        shell.functions = saved.functions;
        shell.exported = saved.exported;
        shell.readonly = saved.readonly;
        shell.unset = saved.unset;
        shell.substitution_depth = saved.depth;
        shell.status = saved.status;
        shell.inherited_background_pid = saved.last_background_pid;
        if let Some(closed) = closed {
            let mut descriptors = inherited_descriptors();
            for (fd, descriptor) in descriptors[..3].iter_mut().enumerate() {
                if (closed & (1 << fd)) != 0 {
                    *descriptor = Descriptor::Closed;
                }
            }
            shell.persistent_fds = descriptors.clone();
            let mut input = BufReader::with_capacity(1, process::Stdin);
            match saved.task {
                WorkerTask::List(body) => {
                    shell.run_with_descriptors(&body, &mut input, &descriptors)
                }
                WorkerTask::Stage(command) => {
                    shell.run_simple_command(&command, &mut input, &descriptors, true)
                }
                _ => return Err("invalid subshell task".into()),
            }
            return Ok(shell.finish(&mut input, Ok(shell.status)));
        }
        process::null_stdin().map_err(|error| error.to_string())?;
        match saved.task {
            WorkerTask::Script(source) => {
                let result = shell
                    .run_source(&source, &mut io::empty())
                    .map(|()| shell.status);
                Ok(shell.finish(&mut io::empty(), result))
            }
            WorkerTask::Complete { function, words } => shell.complete_in_worker(&function, words),
            WorkerTask::List(_) | WorkerTask::Stage(_) => {
                Err("invalid command substitution task".into())
            }
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
            diagnostic_marker: self.diagnostic_marker.clone(),
            task,
            options: self.options,
            trap_snapshot: self.traps.snapshot(),
            directory_stack: self.directory_stack.clone(),
            ignore_errexit: self.ignore_errexit,
            name: self.name.into(),
            args: self.args.clone(),
            getopts_offset: self.getopts_offset,
            variables: self.variables.clone(),
            arrays: self.arrays.clone(),
            aliases: self.aliases.clone(),
            functions: self.functions.clone(),
            exported: self.exported.clone(),
            readonly: self.readonly.clone(),
            unset: self.unset.clone(),
            depth: self.substitution_depth + 1,
            status: self.status,
            last_background_pid: self.last_background_pid(),
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
        self.worker_stage(WorkerTask::List(body.to_vec()), descriptors)
    }

    fn worker_stage(
        &self,
        task: WorkerTask,
        descriptors: &Descriptors,
    ) -> io::Result<process::Stage> {
        if self.substitution_depth >= 32 {
            return Err(io::Error::other("subshells nested too deeply"));
        }
        let state = worker_payload(&self.worker_state(task))?;
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

    fn evaluate_arithmetic(
        &mut self,
        expression: &Word,
        status: &mut Option<u8>,
    ) -> Result<i64, String> {
        let expression = self.expand(expression, status)?.join(" ");
        self.evaluate_arithmetic_text(&expression)
    }

    fn evaluate_arithmetic_text(&mut self, expression: &str) -> Result<i64, String> {
        let mut unset = None;
        let result = crate::arithmetic::evaluate(expression, |name, assignment| {
            if let Some(value) = assignment {
                self.assign_value(name, value.to_string())
                    .map_err(|_| "assignment to a read-only variable")?;
                Ok(None)
            } else {
                if self.options.nounset && self.variable_os(name).is_none() {
                    unset = Some(format!("{name}: parameter is unset"));
                    return Err("unset arithmetic parameter");
                }
                Ok(self.variable(name))
            }
        });
        if let Some(error) = unset {
            return Err(self.parameter_error(error));
        }
        result.map_err(str::to_owned)
    }

    fn parameter_value(&self, name: &str) -> Option<OsString> {
        if name == "-" {
            Some(self.options.letters().into())
        } else if name == "#" {
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

    fn require_parameter<T>(&mut self, name: &str, value: Option<T>) -> Result<Option<T>, String> {
        if self.options.nounset && value.is_none() {
            return Err(self.parameter_error(format!("{name}: parameter is unset")));
        }
        Ok(value)
    }

    fn parameter_error(&mut self, message: String) -> String {
        if (!self.options.interactive || self.shell_worker) && !self.agent_command() {
            self.exited = true;
        }
        message
    }

    fn last_background_pid(&self) -> Option<libc::pid_t> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(pid) = self
            .foreground
            .as_ref()
            .and_then(|jobs| jobs.last_background)
        {
            return Some(pid);
        }
        self.inherited_background_pid
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
                    let pid = self.require_parameter("!", self.last_background_pid())?;
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
                    let value = self
                        .evaluate_arithmetic(expression, status)
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
                    let value = self
                        .require_parameter(name, self.parameter_value(name))?
                        .unwrap_or_default();
                    let value = value.to_string_lossy();
                    let pattern = self.expand_pattern(pattern, status)?;
                    field.push(fragment(pattern.trim(&value, *prefix, *longest).into()));
                }
                Part::Length(name) => {
                    let length = self
                        .require_parameter(name, self.parameter_value(name))?
                        .map_or(0, |value| value.to_string_lossy().chars().count());
                    field.push(fragment(length.to_string()));
                }
                Part::Variable {
                    name,
                    default,
                    if_empty,
                    operator,
                } => {
                    let value = self.parameter_value(name);
                    let value = if default.is_none() {
                        self.require_parameter(name, value)?
                    } else {
                        value
                    };
                    let value = value.filter(|value| !if_empty || !value.is_empty());
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
                            if let Err(error) = self.assign_value(name, expanded.clone()) {
                                return Err(self.parameter_error(error));
                            }
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
                    descriptors[*fd] = here_input(text.as_bytes(), false)?;
                    continue;
                }
                Redirect::HereString { fd, word } => {
                    if self.agent_command() {
                        return Err("here-strings must be submitted by the user".into());
                    }
                    let text = self.expand(word, status)?.join(" ");
                    descriptors[*fd] = here_input(text.as_bytes(), true)?;
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
                        RedirectKind::Write if self.options.noclobber => open_noclobber(target),
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

fn directory_options(mut args: &[String], cd: bool) -> Result<(bool, bool, &[String]), String> {
    let mut physical = false;
    let mut require_pwd = false;
    while let [first, tail @ ..] = args {
        if first == "--" {
            return Ok((physical, require_pwd, tail));
        }
        if first == "-" || !first.starts_with('-') {
            break;
        }
        for flag in first[1..].chars() {
            match flag {
                'L' => physical = false,
                'P' => physical = true,
                'e' if cd => require_pwd = true,
                _ => return Err(format!("unsupported option: -{flag}")),
            }
        }
        args = tail;
    }
    Ok((physical, require_pwd, args))
}

fn umask(args: &[String], output: &Descriptor) -> Result<(), String> {
    let (symbolic, args) = match args {
        [flag, rest @ ..] if flag == "-S" => (true, rest),
        _ => (false, args),
    };
    let operands = args.strip_prefix(&[String::from("--")]);
    match operands.unwrap_or(args) {
        [] => {
            let mask = crate::umask::current();
            let value = if symbolic {
                crate::umask::symbolic(mask)
            } else {
                format!("{mask:04o}")
            };
            output
                .write(&format!("{value}\n"))
                .map_err(|error| error.to_string())
        }
        [value] if operands.is_some() || !value.starts_with('-') => {
            let mask = crate::umask::parse(value, crate::umask::current()).ok_or("invalid mask")?;
            unsafe { libc::umask(mask) };
            Ok(())
        }
        _ => Err("expected umask [-S] [mask]".into()),
    }
}

fn open_noclobber(target: &str) -> io::Result<File> {
    let exists = match OpenOptions::new().write(true).create_new(true).open(target) {
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => error,
        result => return result,
    };
    if std::fs::metadata(target)?.is_file() {
        return Err(exists);
    }
    // Devices and FIFOs may be opened, but never truncate here. If the path
    // was replaced after metadata(), refuse to write to a regular file.
    let file = OpenOptions::new().write(true).open(target)?;
    if file.metadata()?.is_file() {
        return Err(exists);
    }
    Ok(file)
}

fn here_input(text: &[u8], newline: bool) -> Result<Descriptor, String> {
    // An anonymous file avoids blocking before the reader starts.
    let mut file = tempfile::tempfile().map_err(here_document_error)?;
    file.write_all(text).map_err(here_document_error)?;
    if newline {
        file.write_all(b"\n").map_err(here_document_error)?;
    }
    file.rewind().map_err(here_document_error)?;
    Ok(Descriptor::File(Rc::new(file)))
}

fn here_document_error(error: io::Error) -> String {
    format!("here-document: {error}")
}

pub(crate) const BUILTINS: &[&str] = &[
    "exit",
    "exec",
    "cd",
    "pwd",
    "pushd",
    "popd",
    "dirs",
    "export",
    "readonly",
    "unset",
    "read",
    "print",
    "printf",
    "fg",
    "alias",
    "unalias",
    "jobs",
    "bg",
    "wait",
    "command",
    ":",
    "true",
    "false",
    "eval",
    "break",
    "continue",
    ".",
    "source",
    "local",
    "set",
    "shift",
    "getopts",
    "umask",
    "trap",
    "return",
    "complete",
    "compgen",
    "ulimit",
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    "fc",
];

pub(crate) fn is_builtin(name: &str) -> bool {
    BUILTINS.contains(&name)
}

fn is_special_builtin(name: &str) -> bool {
    matches!(
        name,
        ":" | "."
            | "break"
            | "continue"
            | "eval"
            | "exec"
            | "exit"
            | "export"
            | "readonly"
            | "return"
            | "set"
            | "shift"
            | "trap"
            | "unset"
    )
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

pub(crate) fn quote_value(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
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
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fn is_terminal(&self) -> bool {
        match self {
            Self::Inherited(fd) => unsafe { libc::isatty(*fd) == 1 },
            Self::File(file) => unsafe { libc::isatty(file.as_raw_fd()) == 1 },
            Self::Closed => false,
        }
    }

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

fn print(args: &[String]) -> Result<String, String> {
    let mut args = args.iter().peekable();
    let (mut newline, mut raw) = (true, false);
    while let Some(option) = args.next_if(|arg| arg.starts_with('-') && arg.len() > 1) {
        if option == "--" {
            break;
        }
        for flag in option[1..].chars() {
            match flag {
                'n' => newline = false,
                'r' => raw = true,
                _ => return Err(format!("unsupported option: -{flag}")),
            }
        }
    }
    let mut output = String::new();
    for (index, arg) in args.enumerate() {
        if index != 0 {
            output.push(' ');
        }
        if raw {
            output.push_str(arg);
            continue;
        }
        let mut chars = arg.chars();
        while let Some(ch) = chars.next() {
            if ch != '\\' {
                output.push(ch);
                continue;
            }
            let escaped = match chars.next() {
                Some('a') => '\x07',
                Some('b') => '\x08',
                Some('c') => return Ok(output),
                Some('e') => '\x1b',
                Some('f') => '\x0c',
                Some('n') => '\n',
                Some('r') => '\r',
                Some('t') => '\t',
                Some('v') => '\x0b',
                Some('\\') => '\\',
                Some(other) => {
                    output.push('\\');
                    other
                }
                None => '\\',
            };
            output.push(escaped);
        }
    }
    if newline {
        output.push('\n');
    }
    Ok(output)
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

    const CHILD: &str = "ZLEDSH_TEST_AGENT_FOREGROUND_CHILD";
    const TEST: &str =
        "runtime::tests::agent_external_command_keeps_terminal_handoff_with_null_stdin";

    #[test]
    fn agent_external_command_keeps_terminal_handoff_with_null_stdin() {
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
            if std::env::var_os(CHILD).as_deref() == Some(std::ffi::OsStr::new("long")) {
                run("/bin/sleep 31");
            }
            run("/usr/bin/touch foo.txt");
            run("/bin/test ! -t 0 > output 2>&1");
            assert!(crate::foreground::Foreground::new().is_ok());
            return;
        }

        check_agent_foreground("short");
    }

    #[test]
    #[ignore = "waits 31 seconds to cross the former command timeout"]
    fn agent_external_command_runs_past_former_timeout() {
        check_agent_foreground("long");
    }

    fn check_agent_foreground(mode: &str) {
        // Re-exec the child test on a controlling PTY, not Cargo's stdin.
        let work = tempfile::tempdir().unwrap();
        let output = crate::test_terminal::test_command(TEST, false)
            .env(CHILD, mode)
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
