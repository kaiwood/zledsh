use std::{iter::Peekable, str::Chars};

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Command {
    pub words: Vec<Word>,
    pub background: bool,
    pub redirects: Vec<Redirect>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Redirect {
    pub fd: usize,
    pub target: Word,
    pub duplicate: bool,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Word {
    pub parts: Vec<Part>,
    pub assignment: bool,
    pub bare: bool,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum Part {
    Literal(String),
    Name,
    Status,
    LastBackground,
    Arguments,
    Substitution {
        source: String,
        split: bool,
    },
    Variable {
        name: String,
        default: Option<Word>,
        if_empty: bool,
        alternate: bool,
    },
}

#[derive(Clone)]
struct Source<'a> {
    source: &'a str,
    chars: Peekable<Chars<'a>>,
    offset: usize,
    substitutions: Option<usize>,
}

impl<'a> Source<'a> {
    fn new(source: &'a str, substitutions: Option<usize>) -> Self {
        Self {
            source,
            chars: source.chars().peekable(),
            offset: 0,
            substitutions,
        }
    }

    fn peek(&mut self) -> Option<&char> {
        self.chars.peek()
    }

    fn next_if(&mut self, test: impl FnOnce(&char) -> bool) -> Option<char> {
        let ch = self.chars.next_if(test)?;
        self.offset += ch.len_utf8();
        Some(ch)
    }

    fn next_if_eq(&mut self, expected: &char) -> Option<char> {
        self.next_if(|ch| ch == expected)
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
    Pipeline(Vec<Command>),
    Function {
        name: String,
        body: Script,
    },
    Negated(Box<Statement>),
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
        clauses: Vec<(Vec<CasePattern>, Script)>,
    },
    If {
        branches: Vec<(Script, Script)>,
        otherwise: Script,
    },
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) enum CasePattern {
    Any,
    Exact(Word),
}

pub(crate) fn parse(source: &str) -> ParseResult<Script> {
    list(&mut Source::new(source, Some(0)), &[], 0)
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
    matches!(ch, ' ' | '\t' | '\n' | ';' | '>' | '|' | '&' | ')')
}

fn head(input: &Source<'_>) -> String {
    input.clone().take_while(|&ch| !boundary(ch)).collect()
}

fn list(input: &mut Source<'_>, stops: &[&str], depth: usize) -> ParseResult<Script> {
    let mut script = Vec::new();
    loop {
        skip_space(input);
        if input.next_if_eq(&'\n').is_some() {
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
                _ => return Err("background compound commands are not supported".into()),
            },
            Some(';' | '\n') | None => {}
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
        skip_newlines(input);
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

fn skip_newlines(input: &mut Source<'_>) {
    loop {
        skip_space(input);
        if input.next_if_eq(&'\n').is_none() {
            break;
        }
    }
}

fn statement(input: &mut Source<'_>, depth: usize) -> ParseResult<Statement> {
    let negated = head(input) == "!";
    if negated {
        input.next();
        skip_space(input);
    }
    let name = head(input);
    let statement = if matches!(name.as_str(), "if" | "for" | "while" | "until" | "case") {
        if depth >= 32 {
            return Err("compound commands nested too deeply".into());
        }
        input.by_ref().take(name.len()).count();
        match name.as_str() {
            "if" => conditional(input, depth + 1)?,
            "for" => for_loop(input, depth + 1)?,
            "case" => case_statement(input, depth + 1)?,
            _ => while_loop(input, depth + 1, name == "until")?,
        }
    } else if let Some(name) = function_header(input) {
        if depth >= 32 {
            return Err("compound commands nested too deeply".into());
        }
        skip_newlines(input);
        expect_keyword(input, "{", "expected { after function header")?;
        let body = list(input, &["}"], depth + 1)?;
        expect_keyword(input, "}", "expected } after function body")?;
        Statement::Function {
            name,
            body: require_list(body, "empty function body")?,
        }
    } else {
        Statement::Pipeline(pipeline(input)?)
    };
    Ok(if negated {
        Statement::Negated(Box::new(statement))
    } else {
        statement
    })
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
        Some(';' | '\n') => {}
        _ => return Err("expected a separator before do".into()),
    }
    skip_newlines(input);
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
    skip_newlines(input);
    expect_keyword(input, "in", "expected in after case word")?;
    let mut clauses = Vec::new();
    loop {
        skip_newlines(input);
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
            let pattern = if input.next_if_eq(&'*').is_some() {
                CasePattern::Any
            } else {
                CasePattern::Exact(word(input)?)
            };
            patterns.push(pattern);
            skip_space(input);
            match input.next() {
                Some('|') => continue,
                Some(')') => break,
                None => return Err(ParseError::Incomplete("expected ) after case pattern")),
                _ => return Err("expected | or ) after case pattern".into()),
            }
        }
        let body = list(input, &[";;", "esac"], depth)?;
        clauses.push((patterns, body));
        if at_operator_stop(input, &[";;"]) {
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

fn pipeline(input: &mut Source<'_>) -> ParseResult<Vec<Command>> {
    let mut stages = Vec::new();
    loop {
        let mut command = Command::default();
        loop {
            skip_space(input);
            let Some(&next) = input.peek() else {
                break;
            };
            if matches!(next, '\n' | ';' | '&' | '|' | ')') {
                break;
            }
            if let Some(fd) = redirect_fd(input)? {
                while input.next_if(|ch| ch.is_ascii_digit()).is_some() {}
                input.next(); // '>'
                let duplicate = input.next_if_eq(&'&').is_some();
                while input.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
                if matches!(input.peek(), None | Some('#')) {
                    return Err("missing redirection target".into());
                }
                command.redirects.push(Redirect {
                    fd,
                    target: word(input)?,
                    duplicate,
                });
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
                command.words.push(word(input)?);
            }
        }
        if command.words.is_empty() && command.redirects.is_empty() {
            return if input.peek().is_none() {
                Err(ParseError::Incomplete("missing pipeline command"))
            } else {
                Err("missing pipeline command or unexpected separator".into())
            };
        }
        stages.push(command);
        let mut ahead = input.clone();
        if ahead.next() != Some('|') || ahead.peek() == Some(&'|') {
            return Ok(stages);
        }
        *input = ahead;
        skip_newlines(input);
    }
}

fn redirect_fd(input: &Source<'_>) -> ParseResult<Option<usize>> {
    let mut ahead = input.clone();
    let mut digits = String::new();
    while let Some(digit) = ahead.next_if(char::is_ascii_digit) {
        digits.push(digit);
    }
    if ahead.next_if_eq(&'>').is_none() {
        return Ok(None);
    }
    match digits.as_str() {
        "" | "1" => Ok(Some(1)),
        "2" => Ok(Some(2)),
        _ => Err("only output descriptors 1 and 2 are supported".into()),
    }
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
    while let Some(&ch) = input.peek() {
        if boundary(ch) {
            break;
        }
        input.next();
        word.bare &= !matches!(ch, '\'' | '"' | '\\');
        match ch {
            '\'' => word.parts.push(Part::Literal(single_quoted(input)?)),
            '"' => double_quoted(input, &mut word)?,
            '\\' => match input.next() {
                Some('\n') => continue,
                Some(ch) => literal(&mut word, ch),
                None => return Err("trailing backslash".into()),
            },
            '$' if input.peek() == Some(&'(') => {
                let split = !word.assignment;
                substitution(input, &mut word, split)?;
            }
            '[' if !started && input.peek().is_none_or(|&ch| boundary(ch)) => {
                literal(&mut word, ch)
            }
            '(' if word.assignment => return Err("array literals are not supported".into()),
            '<' | '(' | '{' | '}' | '`' | '$' | '*' | '?' | '[' | '~' => {
                return Err(ParseError::Invalid(format!(
                    "unsupported unquoted syntax: {ch}"
                )));
            }
            _ => literal(&mut word, ch),
        }
        started = true;
    }
    if !started && word.parts.is_empty() {
        return Err("expected a word".into());
    }
    Ok(word)
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

fn double_quoted(input: &mut Source<'_>, word: &mut Word) -> ParseResult<()> {
    // Keep even an empty quoted word, but not an empty "$@".
    if input.next_if_eq(&'"').is_some() {
        word.parts.push(Part::Literal(String::new()));
        return Ok(());
    }
    while let Some(ch) = input.next() {
        match ch {
            '"' => return Ok(()),
            '$' => parameter(input, word)?,
            '`' => return Err("command substitution is not supported".into()),
            '\\' => match input.next_if(|ch| matches!(ch, '$' | '`' | '"' | '\\' | '\n')) {
                Some('\n') => {}
                Some(escaped) => literal(word, escaped),
                None => literal(word, '\\'),
            },
            _ => literal(word, ch),
        }
    }
    Err(ParseError::Incomplete("unclosed double quote"))
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) fn parameter_text(value: &str) -> Result<Word, String> {
    let mut word = Word::default();
    let mut input = Source::new(value, None);
    while let Some(ch) = input.next() {
        match ch {
            '$' => parameter(&mut input, &mut word).map_err(|error| error.message().to_owned())?,
            '`' => return Err("command substitution is not supported".into()),
            '\\' if input
                .peek()
                .is_some_and(|ch| matches!(ch, '$' | '`' | '\\' | '\n')) =>
            {
                if let Some(next) = input.next().filter(|ch| *ch != '\n') {
                    literal(&mut word, next);
                }
            }
            _ => literal(&mut word, ch),
        }
    }
    Ok(word)
}

fn parameter(input: &mut Source<'_>, word: &mut Word) -> ParseResult<()> {
    parameter_at(input, word, 0)
}

fn parameter_at(input: &mut Source<'_>, word: &mut Word, depth: usize) -> ParseResult<()> {
    if depth >= 32 {
        return Err("parameter expansion nested too deeply".into());
    }
    let part = match input.peek() {
        Some('(') => return substitution(input, word, false),
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
        Some(ch) if *ch == '#' || ch.is_ascii_digit() => Part::Variable {
            name: input.next().unwrap().to_string(),
            default: None,
            if_empty: false,
            alternate: false,
        },
        Some('@') => {
            input.next();
            Part::Arguments
        }
        Some('{') => {
            input.next();
            let name = if input.peek().is_some_and(char::is_ascii_digit) {
                std::iter::from_fn(|| input.next_if(char::is_ascii_digit)).collect()
            } else {
                variable_name(input).ok_or("unsupported braced parameter")?
            };
            let if_empty = input.next_if_eq(&':').is_some();
            let operator = input.next_if(|ch| matches!(ch, '-' | '+'));
            let default = if operator.is_some() {
                let mut value = Word::default();
                while let Some(ch) = input.next_if(|ch| *ch != '}') {
                    match ch {
                        '$' => parameter_at(input, &mut value, depth + 1)?,
                        '`' | '\\' | '\'' | '"' | '{' => {
                            return Err("unsupported syntax in parameter default".into());
                        }
                        _ => literal(&mut value, ch),
                    }
                }
                Some(value)
            } else if if_empty {
                return Err("unsupported braced parameter operator".into());
            } else {
                None
            };
            input
                .next_if_eq(&'}')
                .ok_or("unsupported or unclosed braced parameter")?;
            Part::Variable {
                name,
                default,
                if_empty,
                alternate: operator == Some('+'),
            }
        }
        Some(ch) if ch.is_ascii_alphanumeric() || "_(*#?$!-".contains(*ch) => {
            let name = variable_name(input).ok_or("unsupported parameter")?;
            Part::Variable {
                name,
                default: None,
                if_empty: false,
                alternate: false,
            }
        }
        _ => {
            literal(word, '$');
            return Ok(());
        }
    };
    word.parts.push(part);
    Ok(())
}

fn substitution(input: &mut Source<'_>, word: &mut Word, split: bool) -> ParseResult<()> {
    let Some(depth) = input.substitutions else {
        return Err("command substitution is not supported here".into());
    };
    if depth >= 16 {
        return Err("command substitutions nested too deeply".into());
    }
    input.next(); // '('
    if input.peek() == Some(&'(') {
        return Err("arithmetic expansion is not supported".into());
    }
    let start = input.offset;
    input.substitutions = Some(depth + 1);
    list(input, &[")"], 0)?;
    input.substitutions = Some(depth);
    let end = input.offset;
    if input.next_if_eq(&')').is_none() {
        return Err(ParseError::Incomplete("unclosed command substitution"));
    }
    word.parts.push(Part::Substitution {
        source: input.source[start..end].into(),
        split,
    });
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

fn literal(word: &mut Word, ch: char) {
    if let Some(Part::Literal(text)) = word.parts.last_mut() {
        text.push(ch);
    } else {
        word.parts.push(Part::Literal(String::from(ch)));
    }
}
