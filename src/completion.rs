use std::{
    io::{self, Read, Seek},
    os::unix::process::CommandExt,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

use crate::runtime::{BUILTINS, Invocation};

pub(crate) fn matches(line: &str, at_end: bool, shell: &Invocation<'_>) -> Option<Matches> {
    let Draft {
        mut words,
        token,
        quote,
        expand_home,
    } = Draft::parse(line)?;
    // A whitespace byte to the right of the cursor may be part of the
    // quoted word, not an argument boundary. Do not insert a closing quote there.
    if !at_end && quote != Quote::Bare {
        return None;
    }
    let command = words.is_empty();
    let programmed = if command {
        None
    } else {
        words.push(token.clone());
        shell.programmed_completion(words)
    };
    let mut matches = programmed.unwrap_or_default();
    matches.retain(|candidate| {
        !candidate.is_empty()
            && candidate.starts_with(&token)
            && !candidate.chars().any(char::is_control)
    });
    if token == "~" && expand_home && matches.is_empty() {
        if shell
            .variable("HOME")
            .is_some_and(|home| Path::new(&home).is_dir())
        {
            matches.push("~/".into());
        }
    } else if command && !token.contains('/') {
        if token.is_empty() {
            return None;
        }
        matches.extend(
            BUILTINS
                .iter()
                .filter(|word| word.starts_with(&token))
                .map(ToString::to_string),
        );
        matches.extend(
            shell
                .aliases()
                .filter(|name| name.starts_with(&token))
                .map(str::to_owned),
        );
        let path = shell
            .variable("PATH")
            .unwrap_or_else(|| "/usr/bin:/bin".into());
        for directory in std::env::split_paths(&path) {
            let directory = if directory.as_os_str().is_empty() {
                Path::new(".")
            } else {
                &directory
            };
            collect(directory, "", &token, true, &mut matches);
        }
    } else if matches.is_empty() {
        let (parent, prefix) = token
            .rsplit_once('/')
            .map_or(("", token.as_str()), |(parent, prefix)| (parent, prefix));
        let original = &token[..token.len() - prefix.len()];
        let directory = if original.is_empty() {
            "."
        } else if parent.is_empty() {
            "/"
        } else {
            parent
        };
        // Expand only the lookup path; candidates must keep the typed `~/`.
        let home_directory = expand_home
            .then(|| original.strip_prefix("~/"))
            .flatten()
            .and_then(|rest| shell.variable("HOME").map(|home| format!("{home}/{rest}")));
        collect(
            Path::new(home_directory.as_deref().unwrap_or(directory)),
            original,
            prefix,
            command,
            &mut matches,
        );
    }
    matches.sort_unstable();
    matches.dedup();
    (!matches.is_empty()).then_some(Matches {
        token_len: token.len(),
        candidates: matches,
        quote,
    })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Quote {
    Bare,
    Single,
    Double,
}

struct Draft {
    words: Vec<String>,
    token: String,
    quote: Quote,
    expand_home: bool,
}

impl Draft {
    // Decode only literal words, never shell syntax or expansions. Quotes may
    // be open at the cursor; the suffix is encoded for the current quote mode.
    fn parse(line: &str) -> Option<Self> {
        let mut words = Vec::new();
        let mut token = String::new();
        let mut quote = Quote::Bare;
        let mut started = false;
        let mut expand_home = false;
        let mut chars = line.chars().peekable();
        while let Some(ch) = chars.next() {
            match (quote, ch) {
                (_, '\n' | '\r') => return None,
                (Quote::Bare, ' ' | '\t') => {
                    if started {
                        words.push(std::mem::take(&mut token));
                    }
                    started = false;
                    expand_home = false;
                    continue;
                }
                (Quote::Bare, '\'') => quote = Quote::Single,
                (Quote::Bare, '"') => quote = Quote::Double,
                (Quote::Single, '\'') | (Quote::Double, '"') => quote = Quote::Bare,
                (Quote::Bare, '\\') => token.push(chars.next().filter(|&ch| ch != '\n')?),
                (Quote::Double, '\\') => {
                    if chars
                        .peek()
                        .is_some_and(|ch| matches!(ch, '$' | '`' | '"' | '\\'))
                    {
                        token.push(chars.next()?);
                    } else {
                        token.push('\\');
                    }
                }
                (
                    Quote::Bare,
                    '$' | '`' | ';' | '|' | '&' | '<' | '>' | '(' | ')' | '*' | '?' | '[' | '{'
                    | '}',
                )
                | (Quote::Double, '$' | '`') => return None,
                (Quote::Bare, '#') if !started => return None,
                (Quote::Bare, '~') if !started => {
                    expand_home = chars.peek().is_none_or(|ch| *ch == '/');
                    token.push('~');
                }
                (_, ch) => token.push(ch),
            }
            started = true;
        }
        Some(Self {
            words,
            token,
            quote,
            expand_home,
        })
    }
}

pub(crate) struct Matches {
    token_len: usize,
    candidates: Vec<String>,
    quote: Quote,
}

impl Matches {
    pub(crate) fn len(&self) -> usize {
        self.candidates.len()
    }

    pub(crate) fn labels(&self) -> impl Iterator<Item = String> + '_ {
        let parent_len = self.candidates[0][..self.token_len]
            .rfind('/')
            .map_or(0, |slash| slash + 1);
        self.candidates
            .iter()
            .map(move |word| escape(&word[parent_len..]))
    }

    pub(crate) fn extension(&self, selection: Option<usize>) -> String {
        let word = &self.candidates[selection.unwrap_or(0)];
        let mut end = word.len();
        if selection.is_none() {
            for candidate in &self.candidates[1..] {
                end = word[..end]
                    .char_indices()
                    .zip(candidate.chars())
                    .take_while(|((_, a), b)| a == b)
                    .map(|((i, ch), _)| i + ch.len_utf8())
                    .last()
                    .unwrap_or(0);
            }
        }
        let text = &word[self.token_len..end];
        let mut suffix = match self.quote {
            Quote::Bare => escape(text),
            Quote::Single => text.replace('\'', "'\\''"),
            Quote::Double => {
                let mut suffix = String::new();
                for ch in text.chars() {
                    if "\\\"$`".contains(ch) {
                        suffix.push('\\');
                    }
                    suffix.push(ch);
                }
                suffix
            }
        };
        if self.len() == 1 && !word.ends_with('/') {
            match self.quote {
                Quote::Single => suffix.push('\''),
                Quote::Double => suffix.push('"'),
                Quote::Bare => {}
            }
            suffix.push(' ');
        }
        suffix
    }
}

fn escape(text: &str) -> String {
    let mut escaped = String::new();
    for ch in text.chars() {
        if ch.is_whitespace() || "\\'\"$`;|&<>()*?[]#!{}~".contains(ch) {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

pub(crate) fn run_callback(mut command: Command) -> io::Result<Vec<String>> {
    const LIMIT: u64 = 128 * 1024;
    let mut reply = tempfile::tempfile()?;
    // Use a fresh session so even an explicit open of /dev/tty cannot
    // interfere with the line editor. The worker leads its own process group.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .stdout(reply.try_clone()?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut guard = CallbackChild {
        child,
        reaped: false,
    };
    if !guard.wait(deadline)? {
        return Ok(Vec::new());
    }
    reply.rewind()?;
    let mut bytes = Vec::new();
    reply.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Ok(Vec::new());
    }
    serde_json::from_slice(&bytes).map_err(io::Error::other)
}

struct CallbackChild {
    child: std::process::Child,
    reaped: bool,
}

impl CallbackChild {
    fn wait(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(status.success());
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl Drop for CallbackChild {
    fn drop(&mut self) {
        // Kill children of a timed-out callback along with the worker.
        if !self.reaped {
            unsafe { libc::kill(-(self.child.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.child.wait();
        }
    }
}

fn collect(directory: &Path, base: &str, prefix: &str, command: bool, matches: &mut Vec<String>) {
    use std::os::unix::fs::PermissionsExt;
    let Ok(entries) = directory.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name.starts_with(prefix)
            || (name.starts_with('.') && !prefix.starts_with('.'))
            || name.chars().any(char::is_control)
        {
            continue;
        }
        let Ok(metadata) = entry.path().metadata() else {
            continue;
        };
        if metadata.is_dir() && (!command || !base.is_empty()) {
            matches.push(format!("{base}{name}/"));
        } else if metadata.is_file() && (!command || metadata.permissions().mode() & 0o111 != 0) {
            matches.push(format!("{base}{name}"));
        }
    }
}
