use std::{iter::Peekable, str::Chars};

#[derive(Default)]
pub(crate) struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<Redirect>,
}

pub(crate) struct Redirect {
    pub fd: usize,
    pub target: Word,
    pub duplicate: bool,
}

#[derive(Default)]
pub(crate) struct Word {
    pub parts: Vec<Part>,
    pub assignment: bool,
}

pub(crate) enum Part {
    Literal(String),
    Name,
    Arguments,
    Variable {
        name: String,
        default: Option<String>,
    },
}

type Source<'a> = Peekable<Chars<'a>>;
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

pub(crate) fn parse(source: &str) -> ParseResult<Vec<Command>> {
    let mut input = source.chars().peekable();
    let mut commands = Vec::new();
    let mut command = Command::default();
    while let Some(&next) = input.peek() {
        match next {
            ' ' | '\t' => {
                input.next();
            }
            '#' => {
                while input.peek().is_some_and(|&ch| ch != '\n') {
                    input.next();
                }
            }
            '\n' | ';' => {
                input.next();
                if !command.words.is_empty() || !command.redirects.is_empty() {
                    commands.push(std::mem::take(&mut command));
                } else if next == ';' {
                    return Err("unexpected ';'".into());
                }
            }
            _ => {
                if let Some(fd) = redirect_fd(&input)? {
                    while input.next_if(|ch| ch.is_ascii_digit()).is_some() {}
                    input.next(); // '>'
                    let duplicate = input.next_if_eq(&'&').is_some();
                    while input.next_if(|ch| matches!(ch, ' ' | '\t')).is_some() {}
                    if matches!(input.peek(), None | Some('#')) {
                        return Err("missing redirection target".into());
                    }
                    command.redirects.push(Redirect {
                        fd,
                        target: word(&mut input)?,
                        duplicate,
                    });
                } else {
                    if command.words.is_empty() {
                        let head: String = input
                            .clone()
                            .take_while(|ch| !matches!(ch, ' ' | '\t' | '\n' | ';' | '>'))
                            .collect();
                        if matches!(
                            head.as_str(),
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
                                "unsupported shell keyword: {head}"
                            )));
                        }
                    }
                    command.words.push(word(&mut input)?);
                }
            }
        }
    }
    if !command.words.is_empty() || !command.redirects.is_empty() {
        commands.push(command);
    }
    Ok(commands)
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
        ..Word::default()
    };
    let mut started = false;
    while let Some(&ch) = input.peek() {
        if matches!(ch, ' ' | '\t' | '\n' | ';' | '>') {
            break;
        }
        input.next();
        match ch {
            '\'' => word.parts.push(Part::Literal(single_quoted(input)?)),
            '"' => double_quoted(input, &mut word)?,
            '\\' => match input.next() {
                Some('\n') => continue,
                Some(ch) => literal(&mut word, ch),
                None => return Err("trailing backslash".into()),
            },
            '|' | '&' | '<' | '(' | ')' | '{' | '}' | '`' | '$' | '*' | '?' | '[' | '~' => {
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

fn parameter(input: &mut Source<'_>, word: &mut Word) -> ParseResult<()> {
    let part = match input.peek() {
        Some('0') => {
            input.next();
            Part::Name
        }
        Some('@') => {
            input.next();
            Part::Arguments
        }
        Some('{') => {
            input.next();
            let name = variable_name(input).ok_or("unsupported braced parameter")?;
            let default = if input.next_if_eq(&'-').is_some() {
                let mut text = String::new();
                while let Some(ch) = input.next_if(|ch| *ch != '}') {
                    if matches!(ch, '$' | '`' | '\\' | '\'' | '"' | '{') {
                        return Err("unsupported syntax in parameter default".into());
                    }
                    text.push(ch);
                }
                Some(text)
            } else {
                None
            };
            input
                .next_if_eq(&'}')
                .ok_or("unsupported or unclosed braced parameter")?;
            Part::Variable { name, default }
        }
        Some(ch) if ch.is_ascii_alphanumeric() || "_(*#?$!-".contains(*ch) => {
            let name = variable_name(input).ok_or("unsupported parameter")?;
            Part::Variable {
                name,
                default: None,
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
