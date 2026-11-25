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
            run_shell(invocation, input, connect_chatgpt, transcript)
        });
    }
    run_shell(invocation, input, connect_chatgpt, None)
}

fn run_shell(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
    mut transcript: Option<Transcript>,
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
        None
    };
    let mut source = String::new();
    let mut history = History::new(invocation);
    let mut pending_draft = String::new();
    let mut last_command = None;
    while !invocation.exited {
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
                        instruction.trim(),
                        &context,
                    )? {
                        Some(Ok(answer)) => show_answer(&answer),
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
            }
            source.clear();
            continue;
        }
        let parsed = syntax::parse(&source);
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
            chatgpt::Action::Run(_) if !self.draft.is_empty() => {
                return Ok(json!({"error": "the prepared draft must be submitted by the user"}));
            }
            chatgpt::Action::Run(command) => command,
        };
        let pipeline = match syntax::parse(&command) {
            Ok(mut commands) if commands.len() == 1 => commands.pop().unwrap(),
            Ok(_) => return Ok(json!({"error": "expected one kwsh command or pipeline"})),
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
            Some(capture) => capture.finish()?,
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
    instruction: &str,
    context: &ShellContext,
) -> io::Result<Option<Result<String, chatgpt::Error>>> {
    let mut turn = match chatgpt::Turn::new(instruction, context) {
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
    ) -> io::Result<(Line, bool)> {
        set_attributes(&self.editor)?;
        let mut stderr = io::stderr().lock();
        let line = write_prompt(&mut stderr, prompt)
            .and_then(|()| read_keys(input, &mut stderr, prompt, history, primary, shell, draft));
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

#[cfg(test)]
mod tests {
    use super::*;

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
