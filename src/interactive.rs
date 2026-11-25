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
use crate::shell_context::{LastCommand, ShellContext};
use crate::syntax::{self, ParseError};
use crate::terminal_modes::{
    TTY, attributes, editor_attributes, restore_attributes, set_attributes,
};
use crate::transcript::{self, Transcript};
use crate::{chatgpt, foreground::Foreground};

pub(crate) fn run(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
) -> io::Result<u8> {
    if matching_identity() && invocation.variable("TERM").as_deref() != Some("dumb") {
        return transcript::run(|transcript| {
            run_shell(invocation, input, connect_chatgpt, transcript, None)
        });
    }
    run_shell(invocation, input, connect_chatgpt, None, None)
}

fn run_shell(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
    mut transcript: Option<Transcript>,
    session: Option<Arc<chatgpt::Session>>,
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
        let effort = invocation.variable("KWSH_CHATGPT_REASONING_EFFORT");
        eprintln!("Connecting to ChatGPT. If sign-in is needed, continue in your browser.");
        match editor.chatgpt_task(input, move |cancelled| {
            chatgpt::Session::connect(&home, model.as_deref(), effort.as_deref(), true, cancelled)
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
        session
    };
    let mut source = String::new();
    let mut history = History::new(invocation);
    let mut pending_draft = String::new();
    let mut last_command = None;
    let mut conversation = chatgpt::Conversation::default();
    while !invocation.exited {
        invocation.foreground.as_mut().unwrap().refresh()?;
        let mut prompt = crate::prompt::render(invocation, !source.is_empty(), history.next);
        if history.paused {
            prompt.insert_str(0, "[private] ");
        }
        let primary = source.is_empty();
        let mut input_pending = false;
        if let Some(editor) = &editor {
            let (line, pending) = editor.read_line(
                input,
                &prompt,
                &mut history,
                primary,
                invocation,
                std::mem::take(&mut pending_draft),
            )?;
            input_pending = pending;
            match line {
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
                let context = ShellContext::current(last_command.as_ref());
                let program = invocation.variable("KWSH_ASSISTANT_EXEC");
                if let Some(program) = program.filter(|_| chatgpt.is_none() && !connect_chatgpt) {
                    pending_draft = ask(
                        editor,
                        Path::new(&program),
                        (instruction.trim(), &context),
                        invocation,
                        input,
                    )?;
                } else if matching_identity() {
                    let number = history.next;
                    let mut shell = AgentShell {
                        invocation,
                        transcript: transcript.as_mut(),
                        last_command: &mut last_command,
                        draft: &mut pending_draft,
                        command_number: number,
                    };
                    match ask_chatgpt(
                        editor,
                        input,
                        &mut chatgpt,
                        &mut shell,
                        conversation.begin(instruction.trim(), &context),
                    )? {
                        Some(Ok(answer)) => {
                            conversation.remember(instruction.trim(), &answer);
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
            if history.paused {
                last_command = None;
                conversation.clear();
            }
            source.clear();
            continue;
        }
        let parsed = invocation.parse(&source);
        if matches!(parsed, Err(ParseError::Incomplete(_))) {
            continue;
        }
        let record_context =
            !history.paused && !matches!(&parsed, Ok(commands) if commands.is_empty());
        let mut capture = transcript
            .as_mut()
            .filter(|_| record_context && editor.is_some());
        if let Some(capture) = &mut capture {
            capture.begin(input_pending)?;
        }
        match parsed {
            Ok(commands) => invocation.run(&commands, input),
            Err(error) => {
                eprintln!("kwsh: {}", error.message());
                invocation.status = 2;
            }
        }
        let output = match capture {
            Some(capture) => capture.finish()?,
            None => None,
        };
        history.record(&source);
        if record_context {
            last_command = Some(LastCommand {
                source: source.trim_end_matches('\n').to_owned(),
                output,
                status: invocation.status,
            });
        }
        source.clear();
    }
    if !source.is_empty() {
        eprintln!("kwsh: incomplete command at end of input");
        return Ok(2);
    }
    Ok(invocation.status)
}

struct AgentShell<'a, 'invocation> {
    invocation: &'a mut Invocation<'invocation>,
    transcript: Option<&'a mut Transcript>,
    last_command: &'a mut Option<LastCommand>,
    draft: &'a mut String,
    command_number: usize,
}

impl AgentShell<'_, '_> {
    fn act(
        &mut self,
        action: chatgpt::Action,
        terminal: &mut impl Write,
    ) -> io::Result<serde_json::Value> {
        use serde_json::json;
        let command = match action {
            chatgpt::Action::Prepare(command) => {
                *self.draft = command;
                return Ok(json!({"prepared": true, "executed": false}));
            }
            chatgpt::Action::Run(_) | chatgpt::Action::Display(_) if !self.draft.is_empty() => {
                return Ok(json!({"error": "the prepared draft must be submitted by the user"}));
            }
            chatgpt::Action::Run(command) | chatgpt::Action::Display(command) => command,
        };
        let pipeline = match self.invocation.parse(&command) {
            Ok(script) => match <[syntax::Statement; 1]>::try_from(script) {
                Ok([syntax::Statement::Pipeline(pipeline)]) => pipeline,
                _ => return Ok(json!({"error": "expected one kwsh command or pipeline"})),
            },
            Err(error) => return Ok(json!({"error": error.message()})),
        };
        let prompt = crate::prompt::render(self.invocation, false, self.command_number);
        writeln!(terminal, "{prompt}{command}")?;
        terminal.flush()?;
        if let Some(capture) = &mut self.transcript {
            // This handshake flushes the command echo on the real terminal before execution.
            capture.begin(transcript::input_pending())?;
        }
        self.invocation.run_agent_command(pipeline);
        let output = match &mut self.transcript {
            Some(capture) => capture.finish_agent()?,
            None => None,
        };
        let status = self.invocation.status;
        let result = json!({
            "exit_status": status,
            "cwd": env::current_dir().ok().map(|path| path.to_string_lossy().into_owned()),
            "output": output,
        });
        *self.last_command = Some(LastCommand {
            source: command,
            output,
            status,
        });
        Ok(result)
    }
}

fn ask_chatgpt(
    editor: &EditorTerminal,
    input: &mut impl BufRead,
    session: &mut Option<Arc<chatgpt::Session>>,
    shell: &mut AgentShell<'_, '_>,
    turn: Result<chatgpt::Turn, chatgpt::Error>,
) -> io::Result<Option<Result<String, chatgpt::Error>>> {
    let mut turn = match turn {
        Ok(turn) => turn,
        Err(error) => return Ok(Some(Err(error))),
    };
    if session.is_none() {
        let home = env::var_os("HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute());
        let model = shell.invocation.variable("KWSH_CHATGPT_MODEL");
        let effort = shell.invocation.variable("KWSH_CHATGPT_REASONING_EFFORT");
        match editor.chatgpt_task(input, move |cancelled| {
            chatgpt::Session::connect(
                home.as_deref()
                    .ok_or(chatgpt::Error::Message("ChatGPT requires an absolute HOME"))?,
                model.as_deref(),
                effort.as_deref(),
                false,
                cancelled,
            )
        })? {
            Some(Ok(connected)) => *session = Some(Arc::new(connected)),
            Some(Err(error)) => return Ok(Some(Err(error))),
            None => return Ok(None),
        }
    }
    let session = session.as_ref().unwrap();
    loop {
        let session = Arc::clone(session);
        // Move the stateless request history to the worker and back; only
        // the interactive thread can act on a completed tool call.
        let Some(response) = editor.chatgpt_task(input, move |cancelled| {
            let reply = session.respond(&turn, cancelled)?;
            Ok((turn, reply))
        })?
        else {
            return Ok(None);
        };
        let (mut previous, reply) = match response {
            Ok(result) => result,
            Err(error) => return Ok(Some(Err(error))),
        };
        let mut io_error = None;
        let mut interrupted = false;
        let result = previous.advance(reply, |action| {
            match shell.act(action, &mut io::stderr().lock()) {
                Ok(result) => {
                    interrupted = result["exit_status"] == 130;
                    result
                }
                Err(error) => {
                    io_error = Some(error);
                    serde_json::Value::Null
                }
            }
        });
        if let Some(error) = io_error {
            return Err(error);
        }
        if interrupted {
            return Ok(None);
        }
        match result {
            Ok(None) => turn = previous,
            Ok(Some(answer)) => return Ok(Some(Ok(answer))),
            Err(error) => return Ok(Some(Err(error))),
        }
    }
}

fn ask(
    editor: &EditorTerminal,
    program: &Path,
    (instruction, context): (&str, &ShellContext),
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
) -> io::Result<String> {
    use crate::assistant::Turn;
    set_attributes(&editor.editor)?;
    let result = crate::assistant::turn(program, instruction, context);
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
                            bare: false,
                        })
                        .collect(),
                    redirects: Vec::new(),
                    background: false,
                };
                invocation.run_pipeline(&[command], input);
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
        Ok(source) => {
            if let Err(error) = invocation.run_source(&source, input) {
                eprintln!("kwsh: {}: {error}", path.display());
            }
        }
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
        shell: &mut Invocation<'_>,
        draft: String,
    ) -> io::Result<(Line, bool)> {
        set_attributes(&self.editor)?;
        let mut stderr = io::stderr().lock();
        let line = stderr
            .write_all(b"\x1b[?2004h")
            .and_then(|()| write_prompt(&mut stderr, prompt))
            .and_then(|()| read_keys(input, &mut stderr, prompt, history, primary, shell, draft));
        stderr.write_all(b"\x1b[?2004l")?;
        match &line {
            Ok(Line::Submitted(_)) => stderr.write_all(b"\n")?,
            Ok(Line::Cancelled) => stderr.write_all(b"^C\n")?,
            _ => {}
        }
        stderr.flush()?;
        let pending = transcript::input_pending();
        // Builtins such as `read` run with the user's cooked settings.
        set_attributes(&self.original)?;
        Ok((line?, pending))
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
            let result = task(&worker_flag);
            drop(guard);
            let _ = tx.send(result);
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
    shell: &mut Invocation<'_>,
    draft: String,
) -> io::Result<Line> {
    let mut history_index = None;
    let mut saved_draft = (Vec::new(), 0);
    echo.write_all(draft.as_bytes())?;
    echo.flush()?;
    let mut line = draft.into_bytes();
    let mut cursor = line.len();
    while let Some(byte) = read_editor_byte(input, shell)? {
        let key = if byte == 27 {
            read_escape(input)?
        } else {
            Key::Byte(byte)
        };
        match key {
            Key::Byte(b'\n' | b'\r') => {
                line.push(b'\n');
                return String::from_utf8(line)
                    .map(Line::Submitted)
                    .map_err(io::Error::other);
            }
            Key::Byte(3) => return Ok(Line::Cancelled),
            Key::Byte(4) if line.is_empty() => return Ok(Line::End),
            Key::Byte(4) if cursor < line.len() => {
                line.drain(cursor..next_char(&line, cursor));
            }
            Key::Byte(8 | 127) if cursor > 0 => {
                let start = previous_char(&line, cursor);
                line.drain(start..cursor);
                cursor = start;
            }
            Key::Byte(11) if cursor < line.len() => {
                // At an embedded newline, delete it to join the two lines.
                let end = line[cursor..]
                    .iter()
                    .position(|&byte| byte == b'\n')
                    .map_or(line.len(), |offset| cursor + offset.max(1));
                line.drain(cursor..end);
            }
            Key::Byte(21) => {
                line.drain(..cursor);
                cursor = 0;
            }
            Key::Byte(23) => {
                let end = cursor;
                while cursor > 0 && line[cursor - 1].is_ascii_whitespace() {
                    cursor -= 1;
                }
                while cursor > 0 && !line[cursor - 1].is_ascii_whitespace() {
                    cursor -= 1;
                }
                line.drain(cursor..end);
            }
            Key::Byte(1) | Key::Home => cursor = 0,
            Key::Byte(5) | Key::End => cursor = line.len(),
            Key::Byte(2) | Key::Left => cursor = previous_char(&line, cursor),
            Key::Byte(6) | Key::Right => cursor = next_char(&line, cursor),
            Key::Paste => match read_paste(input)? {
                Paste::Text(bytes) => {
                    let len = bytes.len();
                    line.splice(cursor..cursor, bytes);
                    cursor += len;
                }
                Paste::Rejected => echo.write_all(b"\x07")?,
                Paste::End => return Ok(Line::End),
            },
            Key::Byte(b'\t')
                if primary && (cursor == line.len() || line[cursor].is_ascii_whitespace()) =>
            {
                let Some(mut suffix) = std::str::from_utf8(&line[..cursor])
                    .ok()
                    .and_then(|prefix| crate::completion::suffix(prefix, shell))
                else {
                    continue;
                };
                if cursor == line.len() {
                    line.extend_from_slice(suffix.as_bytes());
                    cursor = line.len();
                    echo.write_all(suffix.as_bytes())?;
                    echo.flush()?;
                    continue;
                }
                if suffix.ends_with(' ') {
                    suffix.pop();
                }
                line.splice(cursor..cursor, suffix.bytes());
                cursor += suffix.len();
            }
            Key::Up | Key::Down if primary && !history.paused => {
                if history_index.is_none() {
                    if key == Key::Down {
                        continue;
                    }
                    history.refresh();
                    saved_draft = (line.clone(), cursor);
                }
                let matches_prefix =
                    |text: &str| text.as_bytes().starts_with(&saved_draft.0[..saved_draft.1]);
                if key == Key::Up {
                    let bound = history_index.unwrap_or_else(|| history.entries().len());
                    history_index = history
                        .entries()
                        .take(bound)
                        .rposition(matches_prefix)
                        .or(history_index);
                    if history_index.is_none() {
                        continue;
                    }
                } else if let Some(index) = history_index {
                    history_index = history
                        .entries()
                        .enumerate()
                        .skip(index + 1)
                        .find_map(|(index, text)| matches_prefix(text).then_some(index));
                }
                (line, cursor) = history_index
                    .and_then(|index| history.entries().nth(index))
                    .map_or_else(
                        || saved_draft.clone(),
                        |text| (text.as_bytes().to_vec(), text.len()),
                    );
            }
            Key::Byte(byte @ (b' '..=b'~' | 128..=255 | b'\t')) => {
                if cursor == line.len() {
                    line.push(byte);
                    cursor += 1;
                    echo.write_all(&[byte])?;
                    echo.flush()?;
                    continue;
                }
                line.insert(cursor, byte);
                cursor += 1;
            }
            _ => continue,
        }
        redraw(echo, prompt, &line, cursor)?;
        echo.flush()?;
    }
    Ok(Line::End)
}

#[derive(PartialEq, Eq)]
enum Key {
    Byte(u8),
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Paste,
    Ignored,
}

fn read_escape(input: &mut impl BufRead) -> io::Result<Key> {
    if !matches!(read_key_byte(input)?, Some(b'[' | b'O')) {
        return Ok(Key::Ignored);
    }
    let mut sequence = [0; 16];
    for index in 0..sequence.len() {
        let Some(byte) = read_key_byte(input)? else {
            return Ok(Key::Ignored);
        };
        sequence[index] = byte;
        if (0x40..=0x7e).contains(&byte) {
            return Ok(match &sequence[..=index] {
                b"A" => Key::Up,
                b"B" => Key::Down,
                b"C" => Key::Right,
                b"D" => Key::Left,
                b"H" | b"1~" => Key::Home,
                b"F" | b"4~" => Key::End,
                b"200~" => Key::Paste,
                _ => Key::Ignored,
            });
        }
    }
    Ok(Key::Ignored)
}

enum Paste {
    Text(Vec<u8>),
    Rejected,
    End,
}

fn read_paste(input: &mut impl BufRead) -> io::Result<Paste> {
    const END: &[u8] = b"\x1b[201~";
    const LIMIT: usize = 64 * 1024;
    let mut bytes = Vec::new();
    let mut matched = 0;
    let mut rejected = false;
    while let Some(byte) = read_key_byte(input)? {
        if !rejected {
            if bytes.len() == LIMIT + END.len() {
                rejected = true;
            } else {
                bytes.push(byte);
            }
        }
        if byte == END[matched] {
            matched += 1;
        } else {
            matched = usize::from(byte == END[0]);
        }
        if matched == END.len() {
            if rejected {
                return Ok(Paste::Rejected);
            }
            bytes.truncate(bytes.len() - END.len());
            let Ok(text) = String::from_utf8(bytes) else {
                return Ok(Paste::Rejected);
            };
            if text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\n' | '\t'))
            {
                return Ok(Paste::Rejected);
            }
            return Ok(Paste::Text(
                text.replace("\r\n", "\n").replace('\r', "\n").into_bytes(),
            ));
        }
    }
    Ok(Paste::End)
}

fn previous_char(line: &[u8], cursor: usize) -> usize {
    let mut index = cursor.saturating_sub(1);
    while index > 0 && line[index] & 0xc0 == 0x80 {
        index -= 1;
    }
    index
}

fn next_char(line: &[u8], cursor: usize) -> usize {
    let mut index = (cursor + 1).min(line.len());
    while index < line.len() && line[index] & 0xc0 == 0x80 {
        index += 1;
    }
    index
}

fn redraw(echo: &mut impl Write, prompt: &str, line: &[u8], cursor: usize) -> io::Result<()> {
    fn display(bytes: &[u8]) -> String {
        let mut visible = String::new();
        for ch in String::from_utf8_lossy(bytes).chars() {
            if ch.is_control() {
                visible.extend(ch.escape_default());
            } else {
                visible.push(ch);
            }
        }
        visible
    }
    let before = display(&line[..cursor]);
    let after = display(&line[cursor..]);
    write!(echo, "\r\x1b[2K{prompt}{before}{after}")?;
    if !after.is_empty() {
        write!(echo, "\r{prompt}{before}")?;
    }
    Ok(())
}

fn read_editor_byte(
    input: &mut impl BufRead,
    shell: &mut Invocation<'_>,
) -> io::Result<Option<u8>> {
    if let Some(jobs) = &mut shell.foreground {
        while jobs.has_live_jobs() {
            let mut fd = libc::pollfd {
                fd: TTY,
                events: libc::POLLIN,
                revents: 0,
            };
            match unsafe { libc::poll(&mut fd, 1, 100) } {
                0 => jobs.refresh()?,
                -1 if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => continue,
                -1 => return Err(io::Error::last_os_error()),
                _ => break,
            }
        }
    }
    read_key_byte(input)
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

#[cfg(test)]
mod tests {
    use super::*;

    mod hosted_pty;

    #[test]
    fn shell_tool_prints_before_execution_and_drafts_do_not_execute() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("foo.txt");
        let command = format!("/usr/bin/touch '{}'", path.display());
        let mut invocation = Invocation::new("kwsh", &[]);
        let mut draft = String::new();
        let mut last_command = None;
        let mut shell = AgentShell {
            invocation: &mut invocation,
            transcript: None,
            last_command: &mut last_command,
            draft: &mut draft,
            command_number: 1,
        };
        let mut display = BeforeExecution {
            path: &path,
            text: Vec::new(),
            flushed: false,
        };
        let prepared = shell
            .act(chatgpt::Action::Prepare(command.clone()), &mut display)
            .unwrap();
        assert_eq!(prepared["executed"], false);
        assert!(!path.exists());
        assert!(display.text.is_empty());
        assert_eq!(*shell.draft, command);
        let blocked = shell
            .act(chatgpt::Action::Run(command.clone()), &mut display)
            .unwrap();
        assert!(blocked["error"].is_string());
        assert!(!path.exists());
        shell.draft.clear();
        let run = shell
            .act(chatgpt::Action::Run(command.clone()), &mut display)
            .unwrap();
        assert_eq!(run["exit_status"], 0);
        assert!(display.flushed);
        assert!(
            String::from_utf8(display.text)
                .unwrap()
                .ends_with(&format!("{command}\n"))
        );
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        assert_eq!(last_command.unwrap().source, command);
    }

    struct BeforeExecution<'a> {
        path: &'a Path,
        text: Vec<u8>,
        flushed: bool,
    }

    impl Write for BeforeExecution<'_> {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.text.write(bytes)
        }

        fn flush(&mut self) -> io::Result<()> {
            assert!(
                !self.path.exists(),
                "command ran before its echo was flushed"
            );
            self.flushed = true;
            Ok(())
        }
    }
}
