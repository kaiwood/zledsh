use std::{cell::RefCell, collections::HashMap, iter::Peekable, rc::Rc, str::Chars};

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Command {
    pub words: Vec<Word>,
    pub background: bool,
    pub redirects: Vec<Redirect>,
    pub compound: Option<Box<Statement>>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Redirect {
    File {
        fd: usize,
        target: Word,
        kind: RedirectKind,
    },
    Here {
        fd: usize,
        document: HereDocument,
    },
    HereString {
        fd: usize,
        word: Word,
    },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct HereDocument {
    pub text: Rc<RefCell<String>>,
    pub expand: bool,
}

#[derive(Clone)]
struct PendingHere {
    delimiter: String,
    strip_tabs: bool,
    document: HereDocument,
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub(crate) enum RedirectKind {
    Read,
    ReadWrite,
    Write,
    ForceWrite,
    Append,
    DuplicateInput,
    DuplicateOutput,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Word {
    pub parts: Vec<WordPart>,
    pub assignment: bool,
    pub bare: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct WordPart {
    pub kind: Part,
    pub quoted: bool,
}

impl Word {
    pub(crate) fn literal(text: String) -> Self {
        let mut word = Self::default();
        word.push(Part::Literal(text), true);
        word
    }

    fn push(&mut self, kind: Part, quoted: bool) {
        self.parts.push(WordPart { kind, quoted });
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Part {
    Literal(String),
    DollarQuoted(String),
    Tilde {
        login: String,
        assignment_only: bool,
        followed_by_slash: bool,
    },
    Name,
    Status,
    LastBackground,
    Positional {
        star: bool,
    },
    Array {
        name: String,
        index: ArrayIndex,
    },
    Substitution(String),
    Arithmetic(Word),
    Length(String),
    Trim {
        name: String,
        pattern: Word,
        prefix: bool,
        longest: bool,
    },
    Variable {
        name: String,
        default: Option<Word>,
        if_empty: bool,
        operator: ParameterOperator,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum ParameterOperator {
    Default,
    Alternate,
    Assign,
    Error,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum ArrayIndex {
    All,
    Number(usize),
    Variable(String),
}

#[derive(Clone)]
struct Source<'a> {
    chars: Peekable<Chars<'a>>,
    offset: usize,
    aliases: Option<&'a HashMap<String, String>>,
    replacements: Vec<Replacement<'a>>,
    alias_next: bool,
    substitutions: Option<usize>,
    pending_here: Vec<PendingHere>,
}

#[derive(Clone)]
struct Replacement<'a> {
    name: &'a str,
    chars: Peekable<Chars<'a>>,
    separator: bool,
    trailing_blank: bool,
}

impl<'a> Source<'a> {
    fn new(source: &'a str, substitutions: Option<usize>) -> Self {
        Self {
            chars: source.chars().peekable(),
            offset: 0,
            aliases: None,
            replacements: Vec::new(),
            alias_next: false,
            substitutions,
            pending_here: Vec::new(),
        }
    }

    fn peek(&mut self) -> Option<&char> {
        while self
            .replacements
            .last_mut()
            .is_some_and(|frame| frame.chars.peek().is_none() && !frame.separator)
        {
            self.alias_next |= self.replacements.pop().unwrap().trailing_blank;
        }
        match self.replacements.last_mut() {
            Some(frame) => frame.chars.peek().or(Some(&' ')),
            None => self.chars.peek(),
        }
    }

    fn next_if(&mut self, test: impl FnOnce(&char) -> bool) -> Option<char> {
        let ch = *self.peek().filter(|ch| test(ch))?;
        if let Some(frame) = self.replacements.last_mut() {
            if frame.chars.next().is_none() {
                frame.separator = false;
            }
        } else {
            self.chars.next();
        }
        if !matches!(ch, ' ' | '\t') {
            self.alias_next = false;
        }
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn expand_aliases(&mut self, command_position: bool) -> ParseResult<()> {
        let Some(aliases) = self.aliases else {
            return Ok(());
        };
        loop {
            skip_space(self);
            if !command_position && !std::mem::take(&mut self.alias_next) {
                return Ok(());
            }
            let mut after = self.clone();
            let name: String = std::iter::from_fn(|| {
                after.next_if(|ch| ch.is_ascii_alphanumeric() || "_-.!%,@".contains(*ch))
            })
            .collect();
            if name.is_empty()
                || after.peek().is_some_and(|&ch| !boundary(ch))
                || matches!(
                    name.as_str(),
                    "!" | "if"
                        | "then"
                        | "elif"
                        | "else"
                        | "fi"
                        | "for"
                        | "while"
                        | "until"
                        | "do"
                        | "done"
                        | "case"
                        | "esac"
                        | "in"
                )
            {
                return Ok(());
            }
            let Some((name, text)) = aliases.get_key_value(&name) else {
                return Ok(());
            };
            if self.replacements.iter().any(|frame| frame.name == name) {
                return Ok(());
            }
            if self.replacements.len() >= 64 {
                return Err("alias substitution nested too deeply".into());
            }
            // Only consume the name, leaving the parent frame active for recursion checks.
            self.by_ref().take(name.len()).for_each(drop);
            self.replacements.push(Replacement {
                name,
                chars: text.chars().peekable(),
                separator: true,
                trailing_blank: text.ends_with([' ', '\t']),
            });
            // The first word of an argument-position replacement is eligible too.
            self.alias_next = true;
        }
    }

    fn next_if_eq(&mut self, expected: &char) -> Option<char> {
        self.next_if(|ch| ch == expected)
    }

    fn read_here_documents(&mut self) -> ParseResult<()> {
        // Newlines end the command header. The queued bodies are data, not tokens.
        for pending in std::mem::take(&mut self.pending_here) {
            let body = pending.read(self)?;
            *pending.document.text.borrow_mut() = body;
        }
        Ok(())
    }

    fn physical_line(&mut self) -> Option<String> {
        self.peek()?;
        let mut line = String::new();
        for ch in self.by_ref() {
            line.push(ch);
            if ch == '\n' {
                break;
            }
        }
        Some(line)
    }

    fn require_here_documents(&self) -> ParseResult<()> {
        if self.pending_here.is_empty() {
            Ok(())
        } else {
            Err(ParseError::Incomplete("expected here-document body"))
        }
    }
}

impl PendingHere {
    fn read(&self, source: &mut Source<'_>) -> ParseResult<String> {
        let mut body = String::new();
        let mut line = String::new();
        while let Some(physical) = source.physical_line() {
            if self.document.expand {
                if let Some(text) = physical.strip_suffix('\n') {
                    let slashes = text.bytes().rev().take_while(|&byte| byte == b'\\').count();
                    if slashes % 2 != 0 {
                        line.push_str(&text[..text.len() - 1]);
                        continue;
                    }
                }
            }
            line.push_str(&physical);
            let text = if self.strip_tabs {
                line.trim_start_matches('\t')
            } else {
                &line
            };
            if text.strip_suffix('\n').unwrap_or(text) == self.delimiter {
                return Ok(body);
            }
            body.push_str(text);
            line.clear();
        }
        Err(ParseError::Incomplete("missing here-document delimiter"))
    }
}

impl Iterator for Source<'_> {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        self.next_if(|_| true)
    }
}
type ParseResult<T> = Result<T, ParseError>;

pub(crate) enum ParseError {
    Invalid(String),
    Incomplete(&'static str),
}

impl ParseError {
    pub fn message(&self) -> &str {
        match self {
            Self::Invalid(message) => message,
            Self::Incomplete(message) => message,
        }
    }
}

impl From<&str> for ParseError {
    fn from(message: &str) -> Self {
        Self::Invalid(message.to_owned())
    }
}

pub(crate) type Script = Vec<Statement>;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Statement {
    Group(Script),
    Arithmetic(Word),
    Redirected {
        command: Box<Statement>,
        redirects: Vec<Redirect>,
    },
    Subshell(Script),
    Pipeline(Vec<Command>),
    Array {
        name: String,
        words: Vec<Word>,
        local: bool,
    },
    Function {
        name: String,
        body: Script,
    },
    Negated(Box<Statement>),
    Async(Box<Statement>),
    AndOr {
        first: Box<Statement>,
        rest: Vec<(bool, Statement)>,
    },
    For {
        name: String,
        words: Option<Vec<Word>>,
        body: Script,
    },
    While {
        until: bool,
        condition: Script,
        body: Script,
    },
    Case {
        word: Word,
        clauses: Vec<CaseClause>,
    },
    If {
        branches: Vec<(Script, Script)>,
        otherwise: Script,
    },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct CaseClause {
    pub patterns: Vec<Word>,
    pub body: Script,
    pub fallthrough: bool,
}

#[cfg(test)]
pub(crate) fn parse(source: &str) -> ParseResult<Script> {
    parse_with_aliases(source, None)
}

pub(crate) fn parse_with_aliases(
    source: &str,
    aliases: Option<&HashMap<String, String>>,
) -> ParseResult<Script> {
    let mut input = Source::new(source, Some(0));
    input.aliases = aliases;
    let script = list(&mut input, &[], 0)?;
    input.require_here_documents()?;
    Ok(script)
}

fn skip_space(input: &mut Source<'_>) {
    while let Some(&ch) = input.peek() {
        match ch {
            ' ' | '\t' => {
                input.next();
            }
            '#' => while input.next_if(|&ch| ch != '\n').is_some() {},
            _ => break,
        }
    }
}

fn boundary(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\n' | ';' | '<' | '>' | '|' | '&' | ')')
}

fn head(input: &Source<'_>) -> String {
    input.clone().take_while(|&ch| !boundary(ch)).collect()
}

fn list(input: &mut Source<'_>, stops: &[&str], depth: usize) -> ParseResult<Script> {
    let mut script = Vec::new();
    loop {
        input.expand_aliases(true)?;
        skip_space(input);
        if input.next_if_eq(&'\n').is_some() {
            input.read_here_documents()?;
            continue;
        }
        if input.peek().is_none() {
            break;
        }
        if at_operator_stop(input, stops) {
            break;
        }
        let name = head(input);
        if stops.contains(&name.as_str()) {
            break;
        }
        let mut statement = and_or(input, depth)?;
        skip_space(input);
        if at_operator_stop(input, stops) {
            script.push(statement);
            break;
        }
        let separator = input.next();
        match separator {
            Some('&') => match &mut statement {
                Statement::Pipeline(stages) => stages.last_mut().unwrap().background = true,
                _ => statement = Statement::Async(Box::new(statement)),
            },
            Some('\n') => input.read_here_documents()?,
            Some(';') | None => {}
            _ => return Err("expected a command separator".into()),
        }
        script.push(statement);
        if separator.is_none() {
            break;
        }
    }
    Ok(script)
}

fn at_operator_stop(input: &mut Source<'_>, stops: &[&str]) -> bool {
    (stops.contains(&")") && input.peek() == Some(&')'))
        || (stops.contains(&";;") && input.clone().take(2).eq([';', ';']))
        || (stops.contains(&";&") && input.clone().take(2).eq([';', '&']))
}

fn and_or(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    let first = statement(input, depth)?;
    let mut rest = Vec::new();
    loop {
        skip_space(input);
        let mut ahead = input.clone();
        let on_success = match (ahead.next(), ahead.next()) {
            (Some('&'), Some('&')) => true,
            (Some('|'), Some('|')) => false,
            _ => break,
        };
        *input = ahead;
        skip_newlines(input)?;
        if input.peek().is_none() {
            return Err(ParseError::Incomplete("expected a command after && or ||"));
        }
        rest.push((on_success, statement(input, depth)?));
    }
    Ok(if rest.is_empty() {
        first
    } else {
        Statement::AndOr {
            first: Box::new(first),
            rest,
        }
    })
}

fn skip_newlines(input: &mut Source<'_>) -> ParseResult<()> {
    loop {
        skip_space(input);
        if input.next_if_eq(&'\n').is_none() {
            return Ok(());
        }
        input.read_here_documents()?;
    }
}

fn statement(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    input.expand_aliases(true)?;
    let negated = head(input) == "!";
    if negated {
        input.next();
        skip_space(input);
    }
    let first = command_form(input, depth)?;
    let statement = if pipeline_separator(input)? {
        let mut stages = vec![pipeline_stage(first)];
        loop {
            stages.push(pipeline_stage(command_form(input, depth)?));
            if !pipeline_separator(input)? {
                break;
            }
        }
        Statement::Pipeline(stages)
    } else {
        first
    };
    Ok(if negated {
        Statement::Negated(Box::new(statement))
    } else {
        statement
    })
}

fn pipeline_stage(statement: Statement) -> Command {
    match statement {
        Statement::Pipeline(mut commands) => commands.pop().expect("single simple command"),
        compound => Command {
            compound: Some(Box::new(compound)),
            ..Command::default()
        },
    }
}

fn pipeline_separator(input: &mut Source<'_>) -> ParseResult<bool> {
    skip_space(input);
    let mut ahead = input.clone();
    if ahead.next() != Some('|') || ahead.peek() == Some(&'|') {
        return Ok(false);
    }
    *input = ahead;
    skip_newlines(input)?;
    Ok(true)
}

fn command_form(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    input.expand_aliases(true)?;
    let name = head(input);
    let statement = if input.clone().take(2).eq(['(', '(']) {
        input.next();
        input.next();
        Statement::Arithmetic(arithmetic_word(input, 0)?)
    } else if input.next_if_eq(&'(').is_some() {
        if depth >= 32 {
            return Err("compound commands nested too deeply".into());
        }
        let body = list(input, &[")"], depth + 1)?;
        input
            .next_if_eq(&')')
            .ok_or(ParseError::Incomplete("expected ) after subshell"))?;
        Statement::Subshell(require_list(body, "empty subshell")?)
    } else if matches!(
        name.as_str(),
        "if" | "for" | "while" | "until" | "case" | "{"
    ) {
        if depth >= 32 {
            return Err("compound commands nested too deeply".into());
        }
        input.by_ref().take(name.len()).count();
        match name.as_str() {
            "if" => conditional(input, depth + 1)?,
            "for" => for_loop(input, depth + 1)?,
            "case" => case_statement(input, depth + 1)?,
            "{" => {
                let body = list(input, &["}"], depth + 1)?;
                expect_keyword(input, "}", "expected } after group")?;
                Statement::Group(require_list(body, "empty group")?)
            }
            _ => while_loop(input, depth + 1, name == "until")?,
        }
    } else if let Some((name, local)) = array_header(input) {
        let mut words = Vec::new();
        loop {
            skip_newlines(input)?;
            match input.peek() {
                Some(')') => {
                    input.next();
                    break;
                }
                None => return Err(ParseError::Incomplete("unclosed array literal")),
                _ => words.push(word(input)?),
            }
        }
        Statement::Array { name, words, local }
    } else if let Some(name) = function_header(input) {
        if depth >= 32 {
            return Err("compound commands nested too deeply".into());
        }
        skip_newlines(input)?;
        expect_keyword(input, "{", "expected { after function header")?;
        let body = list(input, &["}"], depth + 1)?;
        expect_keyword(input, "}", "expected } after function body")?;
        Statement::Function {
            name,
            body: require_list(body, "empty function body")?,
        }
    } else {
        Statement::Pipeline(vec![simple_command(input)?])
    };
    let statement = match statement {
        Statement::Pipeline(_) | Statement::Array { .. } => statement,
        Statement::Function { name, body } => Statement::Function {
            name,
            body: vec![compound_redirects(input, Statement::Group(body))?],
        },
        compound => compound_redirects(input, compound)?,
    };
    Ok(statement)
}

fn compound_redirects(input: &mut Source<'_>, command: Statement) -> ParseResult<Statement> {
    let mut redirects = Vec::new();
    loop {
        skip_space(input);
        let Some(redirect) = redirect(input)? else {
            break;
        };
        redirects.push(redirect);
    }
    if redirects.is_empty() {
        Ok(command)
    } else {
        Ok(Statement::Redirected {
            command: Box::new(command),
            redirects,
        })
    }
}

fn array_header(input: &mut Source<'_>) -> Option<(String, bool)> {
    let mut ahead = input.clone();
    let mut name = variable_name(&mut ahead)?;
    let local = name == "local" && ahead.peek().is_some_and(|ch| matches!(ch, ' ' | '\t'));
    if local {
        skip_space(&mut ahead);
        name = variable_name(&mut ahead)?;
    }
    ahead.next_if_eq(&'=')?;
    ahead.next_if_eq(&'(')?;
    *input = ahead;
    Some((name, local))
}

fn function_header(input: &mut Source<'_>) -> Option<String> {
    let mut ahead = input.clone();
    let name = variable_name(&mut ahead)?;
    skip_space(&mut ahead);
    ahead.next_if_eq(&'(')?;
    skip_space(&mut ahead);
    ahead.next_if_eq(&')')?;
    *input = ahead;
    Some(name)
}

fn expect_keyword(
    input: &mut Source<'_>,
    keyword: &'static str,
    message: &'static str,
) -> ParseResult<()> {
    if input.peek().is_none() {
        return Err(ParseError::Incomplete(message));
    }
    if head(input) != keyword {
        return Err(message.into());
    }
    input.by_ref().take(keyword.len()).count();
    Ok(())
}

fn require_list(script: Script, message: &'static str) -> ParseResult<Script> {
    if script.is_empty() {
        Err(message.into())
    } else {
        Ok(script)
    }
}

fn conditional(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    let mut branches = Vec::new();
    loop {
        let condition = list(input, &["then"], depth)?;
        expect_keyword(input, "then", "expected then in if command")?;
        let condition = require_list(condition, "empty condition in if command")?;
        let body = list(input, &["elif", "else", "fi"], depth)?;
        if input.peek().is_none() {
            return Err(ParseError::Incomplete("expected fi in if command"));
        }
        branches.push((condition, require_list(body, "empty branch in if command")?));
        if head(input) != "elif" {
            break;
        }
        input.by_ref().take(4).count();
    }
    let otherwise = if head(input) == "else" {
        input.by_ref().take(4).count();
        let body = list(input, &["fi"], depth)?;
        if input.peek().is_none() {
            return Err(ParseError::Incomplete("expected fi in if command"));
        }
        require_list(body, "empty else branch in if command")?
    } else {
        Vec::new()
    };
    expect_keyword(input, "fi", "expected fi in if command")?;
    Ok(Statement::If {
        branches,
        otherwise,
    })
}

fn loop_body(input: &mut Source<'_>, depth: usize) -> ParseResult<Script> {
    expect_keyword(input, "do", "expected do in loop")?;
    let body = list(input, &["done"], depth)?;
    expect_keyword(input, "done", "expected done in loop")?;
    require_list(body, "empty loop body")
}

fn for_loop(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    skip_space(input);
    if input.peek().is_none() {
        return Err(ParseError::Incomplete("expected for variable"));
    }
    let name = variable_name(input).ok_or("expected a variable name after for")?;
    if input
        .peek()
        .is_some_and(|&ch| !matches!(ch, ' ' | '\t' | ';' | '\n'))
    {
        return Err("invalid for variable name".into());
    }
    skip_space(input);
    let words = if head(input) == "in" {
        input.by_ref().take(2).count();
        let mut words = Vec::new();
        loop {
            skip_space(input);
            match input.peek() {
                None | Some(';' | '\n') => break,
                _ => words.push(word(input)?),
            }
        }
        Some(words)
    } else {
        None
    };
    match input.next() {
        None => return Err(ParseError::Incomplete("expected do in for loop")),
        Some('\n') => input.read_here_documents()?,
        Some(';') => {}
        _ => return Err("expected a separator before do".into()),
    }
    skip_newlines(input)?;
    Ok(Statement::For {
        name,
        words,
        body: loop_body(input, depth)?,
    })
}

fn while_loop(input: &mut Source<'_>, depth: usize, until: bool) -> ParseResult<Statement> {
    let condition = list(input, &["do"], depth)?;
    let body = loop_body(input, depth)?;
    Ok(Statement::While {
        until,
        condition: require_list(condition, "empty loop condition")?,
        body,
    })
}

fn case_statement(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    skip_space(input);
    if input.peek().is_none() {
        return Err(ParseError::Incomplete("expected a case word"));
    }
    let subject = word(input)?;
    skip_newlines(input)?;
    expect_keyword(input, "in", "expected in after case word")?;
    let mut clauses = Vec::new();
    loop {
        skip_newlines(input)?;
        if head(input) == "esac" {
            break;
        }
        if input.peek().is_none() {
            return Err(ParseError::Incomplete("expected esac in case command"));
        }
        input.next_if_eq(&'(');
        let mut patterns = Vec::new();
        loop {
            skip_space(input);
            patterns.push(word(input)?);
            skip_space(input);
            match input.next() {
                Some('|') => continue,
                Some(')') => break,
                None => return Err(ParseError::Incomplete("expected ) after case pattern")),
                _ => return Err("expected | or ) after case pattern".into()),
            }
        }
        let body = list(input, &[";;", ";&", "esac"], depth)?;
        let fallthrough = at_operator_stop(input, &[";&"]);
        clauses.push(CaseClause {
            patterns,
            body,
            fallthrough,
        });
        if at_operator_stop(input, &[";;", ";&"]) {
            input.next();
            input.next();
        } else {
            break;
        }
    }
    expect_keyword(input, "esac", "expected esac in case command")?;
    Ok(Statement::Case {
        word: subject,
        clauses,
    })
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn last_argument(source: &str) -> Option<&str> {
    let mut input = Source::new(source, Some(0));
    let (_, argument) = simple_command_with_last(&mut input).ok()?;
    skip_newlines(&mut input).ok()?;
    if input.peek().is_some() || input.require_here_documents().is_err() {
        return None;
    }
    argument.map(|range| &source[range])
}

fn simple_command(input: &mut Source<'_>) -> ParseResult<Command> {
    simple_command_with_last(input).map(|(command, _)| command)
}

fn simple_command_with_last(
    input: &mut Source<'_>,
) -> ParseResult<(Command, Option<std::ops::Range<usize>>)> {
    let mut command = Command::default();
    let mut has_name = false;
    let mut last = None;
    loop {
        input.expand_aliases(!has_name)?;
        skip_space(input);
        let Some(&next) = input.peek() else {
            break;
        };
        if matches!(next, '\n' | ';' | '&' | '|' | ')') {
            break;
        }
        if let Some(redirect) = redirect(input)? {
            command.redirects.push(redirect);
        } else {
            if command.words.is_empty() {
                let name = head(input);
                if matches!(
                    name.as_str(),
                    "if" | "then"
                        | "elif"
                        | "else"
                        | "fi"
                        | "for"
                        | "while"
                        | "until"
                        | "do"
                        | "done"
                        | "case"
                        | "esac"
                        | "select"
                        | "function"
                        | "!"
                        | "[["
                        | "]]"
                        | "time"
                ) {
                    return Err(ParseError::Invalid(format!(
                        "unexpected or unsupported shell keyword: {name}"
                    )));
                }
            }
            let start = input.offset;
            let word = word(input)?;
            if has_name {
                last = Some(start..input.offset);
            } else if !word.assignment {
                has_name = true;
            }
            command.words.push(word);
        }
    }
    if command.words.is_empty() && command.redirects.is_empty() {
        return if input.peek().is_none() {
            Err(ParseError::Incomplete("missing pipeline command"))
        } else {
            Err("missing pipeline command or unexpected separator".into())
        };
    }
    Ok((command, last))
}

fn redirect(input: &mut Source<'_>) -> ParseResult<Option<Redirect>> {
    let mut ahead = input.clone();
    let mut digits = String::new();
    while let Some(digit) = ahead.next_if(char::is_ascii_digit) {
        digits.push(digit);
    }
    let operator = match ahead.next() {
        Some(operator @ ('<' | '>')) => operator,
        _ => return Ok(None),
    };
    let fd = if digits.is_empty() {
        usize::from(operator == '>')
    } else {
        descriptor_number(&digits).ok_or("only descriptors 0 through 9 are supported")?
    };
    let kind = if operator == '<' {
        if ahead.next_if_eq(&'<').is_some() {
            if ahead.next_if_eq(&'<').is_some() {
                while ahead.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
                if matches!(ahead.peek(), None | Some('#')) {
                    return Err("missing here-string word".into());
                }
                let word = word(&mut ahead)?;
                *input = ahead;
                return Ok(Some(Redirect::HereString { fd, word }));
            }
            let strip_tabs = ahead.next_if_eq(&'-').is_some();
            while ahead.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
            let (delimiter, expand) = here_delimiter(&mut ahead)?;
            let document = HereDocument {
                text: Rc::default(),
                expand,
            };
            ahead.pending_here.push(PendingHere {
                delimiter,
                strip_tabs,
                document: document.clone(),
            });
            *input = ahead;
            return Ok(Some(Redirect::Here { fd, document }));
        }
        match ahead.next_if(|ch| matches!(ch, '>' | '&')) {
            Some('>') => RedirectKind::ReadWrite,
            Some('&') => RedirectKind::DuplicateInput,
            _ => RedirectKind::Read,
        }
    } else {
        match ahead.next_if(|ch| matches!(ch, '&' | '>' | '|')) {
            Some('&') => RedirectKind::DuplicateOutput,
            Some('>') => RedirectKind::Append,
            Some('|') => RedirectKind::ForceWrite,
            _ => RedirectKind::Write,
        }
    };
    while ahead.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
    if matches!(ahead.peek(), None | Some('#')) {
        return Err("missing redirection target".into());
    }
    let target = word(&mut ahead)?;
    *input = ahead;
    Ok(Some(Redirect::File { fd, target, kind }))
}

pub(crate) fn descriptor_number(text: &str) -> Option<usize> {
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    text.parse().ok().filter(|&fd| fd < crate::SHELL_FDS)
}

// Parse the delimiter literally: quote removal, but no word expansion.
fn here_delimiter(input: &mut Source<'_>) -> ParseResult<(String, bool)> {
    let mut text = String::new();
    let mut quoted = false;
    while let Some(ch) = input.next_if(|&ch| !boundary(ch) && ch != '(') {
        match ch {
            '\'' => {
                quoted = true;
                text.push_str(&single_quoted(input)?);
            }
            '"' => {
                quoted = true;
                loop {
                    match input.next() {
                        Some('"') => break,
                        Some('\\') => {
                            match input.next_if(|ch| matches!(ch, '$' | '`' | '"' | '\\' | '\n')) {
                                Some('\n') => {}
                                Some(ch) => text.push(ch),
                                None => text.push('\\'),
                            }
                        }
                        Some(ch) => text.push(ch),
                        None => {
                            return Err(ParseError::Incomplete(
                                "unclosed here-document delimiter quote",
                            ));
                        }
                    }
                }
            }
            '\\' => match input.next() {
                Some('\n') => {}
                Some(ch) => {
                    quoted = true;
                    text.push(ch);
                }
                None => {
                    return Err(ParseError::Incomplete(
                        "trailing backslash in here-document delimiter",
                    ));
                }
            },
            '$' if input.next_if_eq(&'\'').is_some() => {
                quoted = true;
                text.push_str(&decode_dollar_quote(&dollar_quoted(input)?)?);
            }
            '#' if text.is_empty() && !quoted => break,
            ch => text.push(ch),
        }
    }
    if text.is_empty() && !quoted {
        return Err("missing here-document delimiter".into());
    }
    Ok((text, !quoted))
}

fn word(input: &mut Source<'_>) -> ParseResult<Word> {
    let mut ahead = input.clone();
    let assignment = variable_name(&mut ahead).is_some() && ahead.next_if_eq(&'=').is_some();
    let mut word = Word {
        assignment,
        bare: true,
        ..Word::default()
    };
    let mut started = false;
    let mut assignment_prefix = false;
    while let Some(&ch) = input.peek() {
        if boundary(ch) {
            break;
        }
        input.next();
        let after_assignment_separator = assignment
            && (input.offset == ahead.offset || (input.offset > ahead.offset && ch == ':'));
        let tilde_position = !started || assignment_prefix;
        let assignment_only = started;
        if ch == '\\' && input.peek() == Some(&'\n') {
            input.next();
            word.bare = false;
            continue;
        }
        assignment_prefix = after_assignment_separator;
        word.bare &= !matches!(ch, '\'' | '"' | '\\');
        match ch {
            '\'' => word.push(Part::Literal(single_quoted(input)?), true),
            '"' => double_quoted(input, &mut word, 0)?,
            '\\' => match input.next() {
                Some(ch) => literal(&mut word, ch, true),
                None => return Err("trailing backslash".into()),
            },
            '$' if input.next_if_eq(&'\'').is_some() => {
                word.bare = false;
                word.push(Part::DollarQuoted(dollar_quoted(input)?), true);
            }
            '$' => parameter_at(input, &mut word, false, 0)?,
            '[' if !started && input.peek().is_none_or(|&ch| boundary(ch)) => {
                literal(&mut word, ch, false)
            }
            '~' if tilde_position => {
                if !tilde(input, &mut word, assignment_only, boundary) {
                    literal(&mut word, ch, false);
                }
            }
            '(' if word.assignment => return Err("array literals are not supported".into()),
            '`' => backquoted(input, &mut word, false)?,
            '<' | '(' | '{' | '}' => {
                return Err(ParseError::Invalid(format!(
                    "unsupported unquoted syntax: {ch}"
                )));
            }
            _ => literal(&mut word, ch, false),
        }
        started = true;
    }
    if !started && word.parts.is_empty() {
        return Err("expected a word".into());
    }
    Ok(word)
}

fn tilde(
    input: &mut Source<'_>,
    word: &mut Word,
    assignment_only: bool,
    end: impl Fn(char) -> bool,
) -> bool {
    let mut tail = input.clone();
    let mut login = String::new();
    while let Some(&ch) = tail.peek() {
        if end(ch) || ch == '/' || (assignment_only && ch == ':') {
            break;
        }
        if !ch.is_ascii_alphanumeric() && !matches!(ch, '-' | '_' | '.') {
            return false;
        }
        login.push(tail.next().unwrap());
    }
    let followed_by_slash = tail.peek() == Some(&'/');
    word.push(
        Part::Tilde {
            login,
            assignment_only,
            followed_by_slash,
        },
        true,
    );
    *input = tail;
    true
}

fn single_quoted(input: &mut Source<'_>) -> ParseResult<String> {
    let mut text = String::new();
    for ch in input.by_ref() {
        if ch == '\'' {
            return Ok(text);
        }
        text.push(ch);
    }
    Err(ParseError::Incomplete("unclosed single quote"))
}

fn dollar_quoted(input: &mut Source<'_>) -> ParseResult<String> {
    let mut text = String::new();
    while let Some(ch) = input.next() {
        if ch == '\'' {
            return Ok(text);
        }
        text.push(ch);
        if ch == '\\' {
            if let Some(escaped) = input.next() {
                text.push(escaped);
            }
        }
    }
    Err(ParseError::Incomplete("unclosed dollar-single quote"))
}

pub(crate) fn decode_dollar_quote(text: &str) -> Result<String, &'static str> {
    let mut input = text.bytes().peekable();
    let mut result = Vec::with_capacity(text.len());
    while let Some(byte) = input.next() {
        let byte = if byte == b'\\' {
            match input
                .next()
                .ok_or("incomplete dollar-single quote escape")?
            {
                byte @ (b'\\' | b'\'' | b'"') => byte,
                b'a' => 7,
                b'b' => 8,
                b'e' => 27,
                b'f' => 12,
                b'n' => b'\n',
                b'r' => b'\r',
                b't' => b'\t',
                b'v' => 11,
                b'c' => match input.next().ok_or("missing control character")? {
                    b'?' => 127,
                    b'\\' if input.next_if_eq(&b'\\').is_some() => 28,
                    byte @ (b'@'..=b'_' | b'a'..=b'z') => byte & 31,
                    _ => return Err("invalid control character"),
                },
                first @ (b'x' | b'0'..=b'7') => {
                    let radix = if first == b'x' { 16 } else { 8 };
                    let mut value = if radix == 8 {
                        u32::from(first - b'0')
                    } else {
                        0
                    };
                    let mut digits = 0;
                    while digits < 2 {
                        let Some(digit) = input
                            .peek()
                            .and_then(|byte| char::from(*byte).to_digit(radix))
                        else {
                            break;
                        };
                        input.next();
                        value = value * radix + digit;
                        digits += 1;
                    }
                    if radix == 16 && digits == 0 {
                        return Err("missing hexadecimal digit");
                    }
                    u8::try_from(value)
                        .map_err(|_| "dollar-single quote escape exceeds one byte")?
                }
                byte => {
                    result.push(b'\\');
                    byte
                }
            }
        } else {
            byte
        };
        // Use the POSIX-permitted truncating interpretation of a NUL escape.
        if byte == 0 {
            break;
        }
        result.push(byte);
    }
    String::from_utf8(result).map_err(|_| "dollar-single quote result is not UTF-8")
}

fn double_quoted(input: &mut Source<'_>, word: &mut Word, depth: usize) -> ParseResult<()> {
    // Keep even an empty quoted word, but not an empty "$@".
    if input.next_if_eq(&'"').is_some() {
        word.push(Part::Literal(String::new()), true);
        return Ok(());
    }
    while let Some(ch) = input.next() {
        match ch {
            '"' => return Ok(()),
            '$' => parameter_at(input, word, true, depth)?,
            '`' => backquoted(input, word, true)?,
            '\\' => match input.next_if(|ch| matches!(ch, '$' | '`' | '"' | '\\' | '\n')) {
                Some('\n') => {}
                Some(escaped) => literal(word, escaped, true),
                None => literal(word, '\\', true),
            },
            _ => literal(word, ch, true),
        }
    }
    Err(ParseError::Incomplete("unclosed double quote"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn parameter_text(value: &str) -> Result<Word, String> {
    expansion_text(value, None)
}

pub(crate) fn here_word(value: &str) -> Result<Word, String> {
    expansion_text(value, Some(0))
}

fn expansion_text(value: &str, substitutions: Option<usize>) -> Result<Word, String> {
    let mut word = Word::default();
    let mut input = Source::new(value, substitutions);
    while let Some(ch) = input.next() {
        match ch {
            '$' => parameter_at(&mut input, &mut word, false, 0)
                .map_err(|error| error.message().to_owned())?,
            '`' => backquoted(&mut input, &mut word, false)
                .map_err(|error| error.message().to_owned())?,
            '\\' if input
                .peek()
                .is_some_and(|ch| matches!(ch, '$' | '`' | '\\' | '\n')) =>
            {
                if let Some(next) = input.next().filter(|ch| *ch != '\n') {
                    literal(&mut word, next, true);
                }
            }
            _ => literal(&mut word, ch, false),
        }
    }
    Ok(word)
}

fn parameter_at(
    input: &mut Source<'_>,
    word: &mut Word,
    quoted: bool,
    depth: usize,
) -> ParseResult<()> {
    if depth >= 32 {
        return Err("parameter expansion nested too deeply".into());
    }
    let part = match input.peek() {
        Some('(') => {
            let mut ahead = input.clone();
            ahead.next();
            if ahead.next_if_eq(&'(').is_some() {
                *input = ahead;
                let expression = arithmetic_word(input, depth + 1)?;
                word.push(Part::Arithmetic(expression), quoted);
                return Ok(());
            }
            return substitution(input, word, quoted);
        }
        Some('!') => {
            input.next();
            Part::LastBackground
        }
        Some('?') => {
            input.next();
            Part::Status
        }
        Some('0') => {
            input.next();
            Part::Name
        }
        Some(ch) if matches!(ch, '#' | '-') || ch.is_ascii_digit() => Part::Variable {
            name: input.next().unwrap().to_string(),
            default: None,
            if_empty: false,
            operator: ParameterOperator::Default,
        },
        Some('@' | '*') => Part::Positional {
            star: input.next() == Some('*'),
        },
        Some('{') => {
            input.next();
            if let Some(symbol) = input.next_if(|ch| matches!(ch, '@' | '*')) {
                input
                    .next_if_eq(&'}')
                    .ok_or("unsupported braced parameter operator")?;
                word.push(
                    Part::Positional {
                        star: symbol == '*',
                    },
                    quoted,
                );
                return Ok(());
            }
            if input.next_if_eq(&'#').is_some() {
                if input.next_if_eq(&'}').is_none() {
                    let name = parameter_name(input).ok_or("expected a parameter after ${#")?;
                    input
                        .next_if_eq(&'}')
                        .ok_or("expected } after parameter length")?;
                    word.push(Part::Length(name), quoted);
                } else {
                    word.push(
                        Part::Variable {
                            name: "#".into(),
                            default: None,
                            if_empty: false,
                            operator: ParameterOperator::Default,
                        },
                        quoted,
                    );
                }
                return Ok(());
            }
            let name = parameter_name(input).ok_or("unsupported braced parameter")?;
            if input.next_if_eq(&'[').is_some() {
                let index = if input.next_if_eq(&'@').is_some() {
                    ArrayIndex::All
                } else if input.peek().is_some_and(char::is_ascii_digit) {
                    let digits: String =
                        std::iter::from_fn(|| input.next_if(char::is_ascii_digit)).collect();
                    ArrayIndex::Number(
                        digits
                            .parse()
                            .map_err(|_| ParseError::from("array index is too large"))?,
                    )
                } else {
                    ArrayIndex::Variable(variable_name(input).ok_or("expected an array index")?)
                };
                input
                    .next_if_eq(&']')
                    .ok_or("expected ] after array index")?;
                input
                    .next_if_eq(&'}')
                    .ok_or("expected } after array index")?;
                word.push(Part::Array { name, index }, quoted);
                return Ok(());
            }
            if let Some(op) = input.next_if(|ch| matches!(ch, '#' | '%')) {
                let longest = input.next_if_eq(&op).is_some();
                // Outer double quotes protect the result, not the pattern (2.6.2).
                let pattern = parameter_word(input, false, depth + 1)?;
                input
                    .next_if_eq(&'}')
                    .ok_or(ParseError::Incomplete("unclosed parameter trim"))?;
                word.push(
                    Part::Trim {
                        name,
                        pattern,
                        prefix: op == '#',
                        longest,
                    },
                    quoted,
                );
                return Ok(());
            }
            let if_empty = input.next_if_eq(&':').is_some();
            let operator = input.next_if(|ch| matches!(ch, '-' | '+' | '=' | '?'));
            let default = if operator.is_some() {
                Some(parameter_word(input, quoted, depth + 1)?)
            } else if if_empty {
                return Err("unsupported braced parameter operator".into());
            } else {
                None
            };
            input
                .next_if_eq(&'}')
                .ok_or(ParseError::Incomplete("unclosed braced parameter"))?;
            Part::Variable {
                name,
                default,
                if_empty,
                operator: match operator {
                    Some('+') => ParameterOperator::Alternate,
                    Some('=') => ParameterOperator::Assign,
                    Some('?') => ParameterOperator::Error,
                    _ => ParameterOperator::Default,
                },
            }
        }
        Some(ch) if ch.is_ascii_alphanumeric() || "_(*#?$!-".contains(*ch) => {
            let name = variable_name(input).ok_or("unsupported parameter")?;
            Part::Variable {
                name,
                default: None,
                if_empty: false,
                operator: ParameterOperator::Default,
            }
        }
        _ => {
            literal(word, '$', quoted);
            return Ok(());
        }
    };
    word.push(part, quoted);
    Ok(())
}

fn parameter_name(input: &mut Source<'_>) -> Option<String> {
    if input.next_if_eq(&'-').is_some() {
        Some("-".into())
    } else if input.peek().is_some_and(char::is_ascii_digit) {
        Some(std::iter::from_fn(|| input.next_if(char::is_ascii_digit)).collect())
    } else {
        variable_name(input)
    }
}

fn parameter_word(input: &mut Source<'_>, quoted: bool, depth: usize) -> ParseResult<Word> {
    let mut word = Word::default();
    let mut first = true;
    while let Some(ch) = input.next_if(|ch| *ch != '}') {
        match ch {
            '$' if !quoted && input.next_if_eq(&'\'').is_some() => {
                word.push(Part::DollarQuoted(dollar_quoted(input)?), true);
            }
            '$' => parameter_at(input, &mut word, quoted, depth)?,
            '\'' if !quoted => word.push(Part::Literal(single_quoted(input)?), true),
            '"' => double_quoted(input, &mut word, depth)?,
            '\\' => {
                match input
                    .next_if(|ch| !quoted || matches!(ch, '$' | '`' | '"' | '\\' | '}' | '\n'))
                {
                    Some('\n') => continue,
                    Some(ch) => literal(&mut word, ch, true),
                    None => literal(&mut word, '\\', quoted),
                }
            }
            '~' if first && !quoted => {
                if !tilde(input, &mut word, false, |ch| ch == '}') {
                    literal(&mut word, ch, false);
                }
            }
            '`' => backquoted(input, &mut word, quoted)?,
            _ => literal(&mut word, ch, quoted),
        }
        first = false;
    }
    Ok(word)
}

fn arithmetic_word(input: &mut Source<'_>, depth: usize) -> ParseResult<Word> {
    let mut expression = Word::default();
    let mut parentheses = 0usize;
    while let Some(ch) = input.next() {
        match ch {
            ')' if parentheses == 0 => {
                if input.next_if_eq(&')').is_some() {
                    return Ok(expression);
                }
                return Err("expected )) after arithmetic expression".into());
            }
            '(' => {
                parentheses += 1;
                literal(&mut expression, ch, true);
            }
            ')' => {
                parentheses -= 1;
                literal(&mut expression, ch, true);
            }
            '$' => parameter_at(input, &mut expression, true, depth)?,
            '`' => backquoted(input, &mut expression, true)?,
            '\\' if input.next_if_eq(&'\n').is_some() => {}
            _ => literal(&mut expression, ch, true),
        }
    }
    Err(ParseError::Incomplete("unclosed arithmetic expansion"))
}

fn backquoted(input: &mut Source<'_>, word: &mut Word, quoted: bool) -> ParseResult<()> {
    let depth = input
        .substitutions
        .ok_or("command substitution is not supported here")?;
    if depth >= 16 {
        return Err("command substitutions nested too deeply".into());
    }
    let mut source = String::new();
    while let Some(ch) = input.next() {
        match ch {
            '`' => {
                let mut nested = Source::new(&source, Some(depth + 1));
                list(&mut nested, &[], 0)?;
                nested.require_here_documents()?;
                word.push(Part::Substitution(source), quoted);
                return Ok(());
            }
            '\\' => match input
                .next_if(|ch| matches!(ch, '$' | '`' | '\\' | '\n') || (quoted && *ch == '"'))
            {
                Some('\n') => {}
                Some(ch) => source.push(ch),
                None => source.push('\\'),
            },
            ch => source.push(ch),
        }
    }
    Err(ParseError::Incomplete(
        "unclosed backquoted command substitution",
    ))
}

fn substitution(input: &mut Source<'_>, word: &mut Word, quoted: bool) -> ParseResult<()> {
    let Some(depth) = input.substitutions else {
        return Err("command substitution is not supported here".into());
    };
    if depth >= 16 {
        return Err("command substitutions nested too deeply".into());
    }
    input.next(); // '('
    let start = input.clone();
    let aliases = input.aliases.take();
    input.substitutions = Some(depth + 1);
    let pending = std::mem::take(&mut input.pending_here);
    list(input, &[")"], 0)?;
    input.require_here_documents()?;
    input.pending_here = pending;
    input.substitutions = Some(depth);
    input.aliases = aliases;
    let end = input.offset;
    if input.next_if_eq(&')').is_none() {
        return Err(ParseError::Incomplete("unclosed command substitution"));
    }
    // The worker re-parses the original substitution text with its alias table.
    let mut remaining = end - start.offset;
    let source = start
        .take_while(|ch| {
            if remaining == 0 {
                return false;
            }
            remaining -= ch.len_utf8();
            true
        })
        .collect();
    word.push(Part::Substitution(source), quoted);
    Ok(())
}

fn variable_name(input: &mut Source<'_>) -> Option<String> {
    let first = input.next_if(|ch| ch.is_ascii_alphabetic() || *ch == '_')?;
    let mut name = String::from(first);
    while let Some(ch) = input.next_if(|ch| ch.is_ascii_alphanumeric() || *ch == '_') {
        name.push(ch);
    }
    Some(name)
}

fn literal(word: &mut Word, ch: char, quoted: bool) {
    if let Some(WordPart {
        kind: Part::Literal(text),
        quoted: previous,
    }) = word.parts.last_mut()
        && *previous == quoted
    {
        text.push(ch);
    } else {
        word.push(Part::Literal(String::from(ch)), quoted);
    }
}
