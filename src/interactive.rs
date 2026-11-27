mod editor;

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
use crate::terminal_modes::TTY;
use crate::transcript::{self, Transcript};
use crate::{chatgpt, foreground::Foreground};

use self::editor::{EditorTerminal, Line, read_key_byte, write_prompt};

pub(crate) fn run(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
) -> io::Result<u8> {
    if matching_identity() && invocation.variable("TERM").as_deref() != Some("dumb") {
        return transcript::run(|transcript| {
            run_shell(invocation, input, connect_chatgpt, transcript, None, true)
        });
    }
    run_shell(invocation, input, connect_chatgpt, None, None, true)
}

pub(crate) fn run_without_terminal(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
) -> io::Result<u8> {
    run_shell(invocation, input, false, None, None, false)
}

fn run_shell(
    invocation: &mut Invocation<'_>,
    input: &mut impl BufRead,
    connect_chatgpt: bool,
    mut transcript: Option<Transcript>,
    session: Option<Arc<chatgpt::Session>>,
    terminal: bool,
) -> io::Result<u8> {
    if terminal {
        invocation.foreground = Some(Foreground::new()?);
    }
    invocation.diagnostic_marker = transcript
        .as_ref()
        .map(|capture| std::path::absolute(capture.marker()))
        .transpose()?;
    load_startup(invocation, input);
    if invocation.exited {
        return Ok(invocation.finish(input, Ok(invocation.status)));
    }
    let mut editor = if terminal && invocation.variable("TERM").as_deref() != Some("dumb") {
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
        let model = invocation.variable("ZLEDSH_CHATGPT_MODEL");
        let effort = invocation.variable("ZLEDSH_CHATGPT_REASONING_EFFORT");
        eprintln!("Connecting to ChatGPT. If sign-in is needed, continue in your browser.");
        match run_chatgpt_task(editor, input, move |cancelled| {
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
        if let Some(foreground) = &mut invocation.foreground {
            foreground.notify()?;
        }
        let mut prompt = crate::prompt::render(invocation, !source.is_empty(), history.next);
        if history.paused {
            prompt.insert_str(0, "[private] ");
        }
        let start = source.len();
        let primary = source.is_empty();
        let mut input_pending = false;
        if let Some(editor) = &mut editor {
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
            if invocation.read_stdin_line(input, &mut source)? == 0 {
                break;
            }
        }
        invocation.verbose_input(&source[start..]);
        if let Some(instruction) = source.strip_prefix('?').filter(|_| primary) {
            if history.paused {
                eprintln!(
                    "Shell agent unavailable while history is paused. Use history resume first."
                );
            } else if instruction.trim().is_empty() {
                eprintln!("Shell prompt instruction required after ?");
            } else if let Some(editor) = &editor {
                history.record(&source);
                let context = ShellContext::current(last_command.as_ref());
                let program = invocation.variable("ZLEDSH_ASSISTANT_EXEC");
                if let Some(program) = program.filter(|_| chatgpt.is_none() && !connect_chatgpt) {
                    pending_draft = ask(
                        editor,
                        Path::new(&program),
                        (instruction.trim(), &context),
                        invocation,
                        input,
                        &mut history,
                    )?;
                } else if matching_identity() {
                    let mut shell = AgentShell {
                        invocation,
                        transcript: transcript.as_mut(),
                        last_command: &mut last_command,
                        draft: &mut pending_draft,
                        history: &mut history,
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
                        Some(Err(error)) => eprintln!("zledsh: ChatGPT: {error}"),
                        None => eprintln!("^C\nturn cancelled"),
                    }
                } else {
                    eprintln!("zledsh: ChatGPT is unavailable when real and effective IDs differ");
                }
            } else {
                eprintln!("zledsh: assistant requires the line editor");
            }
            source.clear();
            continue;
        }
        if history.control(&source) {
            if history.paused {
                last_command = None;
                conversation.clear();
                if let Some(editor) = &mut editor {
                    editor.clear_kill_ring();
                }
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
        invocation.listed_history = false;
        match parsed {
            Ok(commands) => {
                invocation.interactive_history = Some(history);
                invocation.run(&commands, input);
                history = invocation
                    .interactive_history
                    .take()
                    .expect("interactive history");
            }
            Err(error) => {
                eprintln!("zledsh: {}", error.message());
                invocation.status = 2;
            }
        }
        let output = match capture {
            Some(capture) => capture.finish()?.filter(|_| !invocation.listed_history),
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
        eprintln!("zledsh: incomplete command at end of input");
        return Ok(invocation.finish(input, Ok(2)));
    }
    Ok(invocation.finish(input, Ok(invocation.status)))
}

struct AgentShell<'a, 'invocation> {
    invocation: &'a mut Invocation<'invocation>,
    transcript: Option<&'a mut Transcript>,
    last_command: &'a mut Option<LastCommand>,
    draft: &'a mut String,
    history: &'a mut History,
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
                _ => return Ok(json!({"error": "expected one zledsh command or pipeline"})),
            },
            Err(error) => return Ok(json!({"error": error.message()})),
        };
        let prompt = crate::prompt::render(self.invocation, false, self.history.next);
        writeln!(terminal, "{prompt}{command}")?;
        terminal.flush()?;
        if let Some(capture) = &mut self.transcript {
            // This handshake flushes the command echo on the real terminal before execution.
            capture.begin(transcript::input_pending())?;
        }
        self.invocation.run_agent_command(pipeline);
        self.history.record(&command);
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
        let model = shell.invocation.variable("ZLEDSH_CHATGPT_MODEL");
        let effort = shell.invocation.variable("ZLEDSH_CHATGPT_REASONING_EFFORT");
        match run_chatgpt_task(editor, input, move |cancelled| {
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
        let Some(response) = run_chatgpt_task(editor, input, move |cancelled| {
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
    history: &mut History,
) -> io::Result<String> {
    use crate::assistant::Turn;
    let result =
        editor.with_editor_mode(|| crate::assistant::turn(program, instruction, context))?;
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
                        .map(|text| syntax::Word::literal(text.into()))
                        .collect(),
                    redirects: Vec::new(),
                    background: false,
                    compound: None,
                };
                invocation.run_pipeline(&[command], input);
                history.record(&format!("cd {}", crate::runtime::quote_value(&directory)));
            }
            show_answer(&answer);
            Ok(draft)
        }
        Ok(Turn::Cancelled) => {
            eprintln!("^C\nturn cancelled");
            Ok(String::new())
        }
        Err(error) => {
            eprintln!("zledsh: assistant: {error}");
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

pub(crate) fn load_startup(invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
    invocation.options.interactive = true;
    // Do not run user startup files if real and effective IDs differ.
    if !matching_identity() {
        return;
    }
    let startup = invocation.variable("ENV");
    load_zledshrc(invocation, input);
    if let Some(value) = startup.filter(|_| !invocation.exited) {
        match invocation.parameter_text(&value) {
            Ok(path) if Path::new(&path).is_file() => load_rc(Path::new(&path), invocation, input),
            Ok(_) => {}
            Err(error) => eprintln!("zledsh: ENV: {error}"),
        }
    }
}

fn load_zledshrc(invocation: &mut Invocation<'_>, input: &mut impl BufRead) {
    let home = env::var_os("HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let primary = env::var_os("KWARD_CONFIG_PATH")
        .filter(|path| !path.is_empty())
        .and_then(|path| Path::new(&path).parent().map(|dir| dir.join("zledshrc")))
        .or_else(|| home.as_ref().map(|path| path.join(".kward/zledshrc")));
    let personal = home.map(|path| path.join(".zledshrc"));
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
                eprintln!("zledsh: {}: {error}", path.display());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => eprintln!("zledsh: {}: {error}", path.display()),
    }
}

fn run_chatgpt_task<T: Send + 'static>(
    editor: &EditorTerminal,
    input: &mut impl BufRead,
    task: impl FnOnce(&AtomicBool) -> Result<T, chatgpt::Error> + Send + 'static,
) -> io::Result<Option<Result<T, chatgpt::Error>>> {
    editor.with_editor_mode(|| wait_for_chatgpt_task(input, task))?
}

// A cancelled blocking HTTP request may still be unwinding. Do not start
// another worker until it exits (HTTP requests have a global timeout).
fn wait_for_chatgpt_task<T: Send + 'static>(
    input: &mut impl BufRead,
    task: impl FnOnce(&AtomicBool) -> Result<T, chatgpt::Error> + Send + 'static,
) -> io::Result<Option<Result<T, chatgpt::Error>>> {
    static ACTIVE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let (tx, rx) = mpsc::sync_channel(1);
    let (ready, started) = mpsc::sync_channel(0);
    let cancelled = Arc::new(AtomicBool::new(false));
    let worker_flag = Arc::clone(&cancelled);
    std::thread::Builder::new()
        .name("zledsh-chatgpt".into())
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

#[cfg(test)]
mod tests {
    use super::*;

    mod hosted_pty;

    #[test]
    fn shell_tool_prints_before_execution_and_drafts_do_not_execute() {
        let work = tempfile::tempdir().unwrap();
        let path = work.path().join("foo.txt");
        let command = format!("/usr/bin/touch '{}'", path.display());
        let mut invocation = Invocation::new("zledsh", &[]);
        invocation
            .run_source("unset HISTFILE", &mut io::empty())
            .unwrap();
        let mut history = History::new(&invocation);
        let mut draft = String::new();
        let mut last_command = None;
        let mut shell = AgentShell {
            invocation: &mut invocation,
            transcript: None,
            last_command: &mut last_command,
            draft: &mut draft,
            history: &mut history,
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
        assert_eq!(history.entries().collect::<Vec<_>>(), [&command]);
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
