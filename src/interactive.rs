use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc,
};
use std::{env, fs};

use crate::history::History;
use crate::runtime::Invocation;
use crate::syntax::{self, ParseError};
use crate::terminal_modes::{
    TTY, attributes, editor_attributes, restore_attributes, set_attributes,
};
use crate::{chatgpt, foreground::Foreground};

pub(crate) fn run(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
) -> io::Result<u8> {
    invocation.foreground = Some(Foreground::new()?);
    load_startup(invocation, input);
    if invocation.exited {
        return Ok(invocation.status);
    }
    let editor = if invocation.variable("TERM").as_deref() != Some("dumb") {
        Some(EditorTerminal::new()?)
    } else {
        None
    };
    let mut chatgpt = if connect_chatgpt {
        if !matching_identity() {
            return Err(io::Error::other(
                "ChatGPT sign-in is unavailable when real and effective IDs differ",
            ));
        }
        let editor = editor.as_ref().ok_or_else(|| {
            io::Error::other("--chatgpt requires the line editor (TERM must not be dumb)")
        })?;
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .ok_or_else(|| io::Error::other("ChatGPT sign-in requires an absolute HOME"))?;
        let model = invocation.variable("KWSH_CHATGPT_MODEL");
        eprintln!("Connecting to ChatGPT. If sign-in is needed, continue in your browser.");
        match editor.chatgpt_task(input, move |cancelled| {
            chatgpt::Session::connect(&home, model.as_deref(), true, cancelled)
        })? {
            Some(Ok(session)) => {
                eprintln!("Connected to ChatGPT (model: {}).", session.model);
                Some(Arc::new(session))
            }
            Some(Err(error)) => return Err(io::Error::other(error.to_string())),
            None => {
                eprintln!("^C\nChatGPT sign-in cancelled");
                return Ok(130);
            }
        }
    } else {
        None
    };
    let mut source = String::new();
    let mut history = History::new(invocation);
    let mut pending_draft = String::new();
    while !invocation.exited {
        let mut prompt = crate::prompt::render(invocation, !source.is_empty(), history.next);
        if history.paused {
            prompt.insert_str(0, "[private] ");
        }
        let primary = source.is_empty();
        if let Some(editor) = &editor {
            match editor.read_line(
                input,
                &prompt,
                &mut history,
                primary,
                invocation,
                std::mem::take(&mut pending_draft),
            )? {
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
        if let Some(instruction) = source.strip_prefix('?').filter(|_| primary) {
            if history.paused {
                eprintln!(
                    "Shell agent unavailable while history is paused. Use history resume first."
                );
            } else if instruction.trim().is_empty() {
                eprintln!("Shell prompt instruction required after ?");
            } else if let Some(editor) = &editor {
                let program = invocation.variable("KWSH_ASSISTANT_EXEC");
                if let Some(program) = program.filter(|_| chatgpt.is_none() && !connect_chatgpt) {
                    pending_draft = ask(
                        editor,
                        Path::new(&program),
                        instruction.trim(),
                        invocation,
                        input,
                    )?;
                } else if matching_identity() {
                    let session = chatgpt.clone();
                    let home = env::var_os("HOME")
                        .map(PathBuf::from)
                        .filter(|path| path.is_absolute());
                    let model = invocation.variable("KWSH_CHATGPT_MODEL");
                    let question = instruction.trim().to_owned();
                    match editor.chatgpt_task(input, move |cancelled| {
                        let session = match session {
                            Some(session) => session,
                            None => Arc::new(chatgpt::Session::connect(
                                home.as_deref().ok_or(chatgpt::Error::Message(
                                    "ChatGPT requires an absolute HOME",
                                ))?,
                                model.as_deref(),
                                false,
                                cancelled,
                            )?),
                        };
                        let answer = session.answer(&question, cancelled)?;
                        Ok((session, answer))
                    })? {
                        Some(Ok((session, answer))) => {
                            chatgpt = Some(session);
                            show_answer(&answer);
                        }
                        Some(Err(error)) => eprintln!("kwsh: ChatGPT: {error}"),
                        None => eprintln!("^C\nturn cancelled"),
                    }
                } else {
                    eprintln!("kwsh: ChatGPT is unavailable when real and effective IDs differ");
                }
            } else {
                eprintln!("kwsh: assistant requires the line editor");
            }
            source.clear();
            continue;
        }
        if history.control(&source) {
            source.clear();
            continue;
        }
        match syntax::parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(ParseError::Incomplete(_)) => continue,
            Err(error) => {
                eprintln!("kwsh: {}", error.message());
                invocation.status = 2;
            }
        }
        history.record(&source);
        source.clear();
    }
    if !source.is_empty() {
        eprintln!("kwsh: incomplete command at end of input");
        return Ok(2);
    }
    Ok(invocation.status)
}

fn ask(
    editor: &EditorTerminal,
    program: &Path,
    instruction: &str,
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
) -> io::Result<String> {
    use crate::assistant::Turn;
    set_attributes(&editor.editor)?;
    let result = crate::assistant::turn(program, instruction);
    set_attributes(&editor.original)?;
    match result {
        Ok(Turn::Reply {
            answer,
            draft,
            directory,
        }) => {
            if !directory.is_empty() {
                let command = syntax::Command {
                    words: ["cd", &directory]
                        .into_iter()
                        .map(|text| syntax::Word {
                            parts: vec![syntax::Part::Literal(text.into())],
                            assignment: false,
                        })
                        .collect(),
                    redirects: Vec::new(),
                };
                invocation.run(&[vec![command]], input);
            }
            show_answer(&answer);
            Ok(draft)
        }
        Ok(Turn::Cancelled) => {
            eprintln!("^C\nturn cancelled");
            Ok(String::new())
        }
        Err(error) => {
            eprintln!("kwsh: assistant: {error}");
            Ok(String::new())
        }
    }
}

fn show_answer(answer: &str) {
    if !answer.is_empty() {
        let safe: String = answer
            .chars()
            .filter(|ch| !ch.is_control() || *ch == '\n')
            .collect();
        println!("{safe}");
    }
}

fn matching_identity() -> bool {
    unsafe { libc::getuid() == libc::geteuid() && libc::getgid() == libc::getegid() }
}

fn load_startup(invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
    // Do not run user startup files if real and effective IDs differ.
    if !matching_identity() {
        return;
    }
    let startup = invocation.variable("ENV");
    load_kwshrc(invocation, input);
    if let Some(value) = startup.filter(|_| !invocation.exited) {
        match invocation.parameter_text(&value) {
            Ok(path) if Path::new(&path).is_file() => load_rc(Path::new(&path), invocation, input),
            Ok(_) => {}
            Err(error) => eprintln!("kwsh: ENV: {error}"),
        }
    }
}

fn load_kwshrc(invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
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
            Err(error) => eprintln!("kwsh: {}: {}", path.display(), error.message()),
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

    fn chatgpt_task<T: Send + 'static>(
        &self,
        input: &mut impl BufRead,
        task: impl FnOnce(&AtomicBool) -> Result<T, chatgpt::Error> + Send + 'static,
    ) -> io::Result<Option<Result<T, chatgpt::Error>>> {
        set_attributes(&self.editor)?;
        let result = run_chatgpt_task(input, task);
        set_attributes(&self.original)?;
        result
    }

    fn read_line(
        &self,
        input: &mut impl BufRead,
        prompt: &str,
        history: &mut History,
        primary: bool,
        shell: &Invocation<'_>,
        draft: String,
    ) -> io::Result<Line> {
        set_attributes(&self.editor)?;
        let mut stderr = io::stderr().lock();
        let line = write_prompt(&mut stderr, prompt)
            .and_then(|()| read_keys(input, &mut stderr, prompt, history, primary, shell, draft));
        // Builtins such as `read` run with the user's cooked settings.
        set_attributes(&self.original)?;
        line
    }
}

// A cancelled blocking HTTP request may still be unwinding. Do not start
// another worker until it exits (HTTP requests have a global timeout).
fn run_chatgpt_task<T: Send + 'static>(
    input: &mut impl BufRead,
    task: impl FnOnce(&AtomicBool) -> Result<T, chatgpt::Error> + Send + 'static,
) -> io::Result<Option<Result<T, chatgpt::Error>>> {
    static ACTIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let (tx, rx) = mpsc::sync_channel(1);
    let (ready, started) = mpsc::sync_channel(0);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&cancelled);
    std::thread::Builder::new()
        .name("kwsh-chatgpt".into())
        .spawn(move || {
            let guard = ACTIVE.try_lock();
            if ready.send(guard.is_ok()).is_err() || guard.is_err() {
                return;
            }
            let _ = tx.send(task(&worker_flag));
        })?;
    if !started.recv().unwrap_or(false) {
        return Ok(Some(Err(chatgpt::Error::Message(
            "previous request is still ending; try again shortly",
        ))));
    }
    let result = (|| loop {
        let mut fd = libc::pollfd {
            fd: TTY,
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut fd, 1, 50) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fd.revents != 0 && matches!(read_key_byte(input)?, None | Some(3 | 4)) {
            return Ok(None);
        }
        match rx.try_recv() {
            Ok(value) => return Ok(Some(value)),
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err(io::Error::other("ChatGPT worker stopped"));
            }
        }
    })();
    cancelled.store(true, Ordering::Relaxed);
    result
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

fn read_keys(
    input: &mut impl BufRead,
    echo: &mut impl Write,
    prompt: &str,
    history: &mut History,
    primary: bool,
    shell: &Invocation<'_>,
    draft: String,
) -> io::Result<Line> {
    let mut cursor = None;
    let mut saved_draft = Vec::new();
    echo.write_all(draft.as_bytes())?;
    echo.flush()?;
    let mut line = draft.into_bytes();
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
                redraw(echo, prompt, &line)?;
            }
            21 => {
                line.clear();
                redraw(echo, prompt, &line)?;
            }
            23 => {
                while line.last().is_some_and(u8::is_ascii_whitespace) {
                    line.pop();
                }
                while line.last().is_some_and(|ch| !ch.is_ascii_whitespace()) {
                    line.pop();
                }
                redraw(echo, prompt, &line)?;
            }
            b'\t' if primary => {
                if let Some(suffix) = std::str::from_utf8(&line)
                    .ok()
                    .and_then(|line| crate::completion::suffix(line, shell))
                {
                    line.extend_from_slice(suffix.as_bytes());
                    echo.write_all(suffix.as_bytes())?;
                }
            }
            27 => {
                if !read_key_byte(input)?.is_some_and(|byte| byte == b'[' || byte == b'O') {
                    continue;
                }
                let Some(direction) = read_key_byte(input)? else {
                    continue;
                };
                if !matches!(direction, b'A' | b'B') || !primary || history.paused {
                    continue;
                }
                if direction == b'A' && cursor.is_none() {
                    history.refresh();
                    saved_draft.clone_from(&line);
                }
                let size = history.entries().len();
                if direction == b'A' {
                    cursor = cursor.unwrap_or(size).checked_sub(1).or(cursor);
                } else if let Some(index) = cursor {
                    cursor = (index + 1 < size).then_some(index + 1);
                } else {
                    continue;
                }
                line = cursor
                    .and_then(|index| history.entries().nth(index))
                    .map_or_else(|| saved_draft.clone(), |text| text.as_bytes().to_vec());
                redraw(echo, prompt, &line)?;
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

fn redraw(echo: &mut impl Write, prompt: &str, line: &[u8]) -> io::Result<()> {
    echo.write_all(b"\r\x1b[2K")?;
    echo.write_all(prompt.as_bytes())?;
    for ch in String::from_utf8_lossy(line).chars() {
        if ch.is_control() {
            write!(echo, "{}", ch.escape_default())?;
        } else {
            write!(echo, "{ch}")?;
        }
    }
    Ok(())
}

fn read_key_byte(input: &mut impl BufRead) -> io::Result<Option<u8>> {
    let mut byte = [0];
    loop {
        match input.read(&mut byte) {
            Ok(0) => return Ok(None),
            Ok(_) => return Ok(Some(byte[0])),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}
