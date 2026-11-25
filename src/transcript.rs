//! Relay a dedicated controlling terminal so command output can be captured
//! without piping a job's stdout/stderr or changing its terminal semantics.
use std::fs::File;
use std::io::{self, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::Duration;

use crate::terminal_modes::{TTY, attributes, restore_attributes, set_attributes};

const LIMIT: usize = 32_000;
const CHUNK: usize = 8192;
const SIGNALS: [libc::c_int; 3] = [libc::SIGHUP, libc::SIGINT, libc::SIGTERM];
static SIGNAL: AtomicI32 = AtomicI32::new(0);

pub(crate) struct Transcript(UnixStream);

impl Transcript {
    pub fn begin(&mut self, input_pending: bool) -> io::Result<()> {
        self.0.write_all(if input_pending { b"s" } else { b"b" })?;
        self.0.read_exact(&mut [0])
    }

    pub fn finish(&mut self) -> io::Result<Option<String>> {
        self.finish_with(b"f")
    }

    pub fn finish_agent(&mut self) -> io::Result<Option<String>> {
        self.finish_with(b"l")
    }

    fn finish_with(&mut self, request: &[u8; 1]) -> io::Result<Option<String>> {
        self.0.write_all(request)?;
        let mut len = [0; 4];
        self.0.read_exact(&mut len)?;
        let len = u32::from_be_bytes(len) as usize;
        if len == 0 || len > LIMIT {
            return Ok(None);
        }
        let mut bytes = vec![0; len];
        self.0.read_exact(&mut bytes)?;
        Ok(plain_text(&bytes))
    }
}

pub(crate) fn run(shell: impl FnOnce(Option<Transcript>) -> io::Result<u8>) -> io::Result<u8> {
    // Do not replace redirected descriptors or wrap an elevated/background shell.
    let Some(tty) = terminal_device(TTY) else {
        return shell(None);
    };
    if terminal_device(1) != Some(tty)
        || terminal_device(2) != Some(tty)
        || unsafe { libc::tcgetpgrp(TTY) != libc::getpgrp() }
    {
        return shell(None);
    }
    let mut original = attributes()?;
    let mut size = window_size()?;
    let (parent, child) = UnixStream::pair()?;
    // Leave the shell's public redirection range (0–9) alone.
    let child_fd = cvt(unsafe { libc::fcntl(child.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) })?;
    let child = {
        drop(child);
        unsafe { UnixStream::from_raw_fd(child_fd) }
    };
    let mut master = -1;
    // Launch occurs before kwsh starts threads. forkpty makes the slave the
    // child's controlling terminal; foreground handoff stays in that session.
    let pid =
        cvt(unsafe { libc::forkpty(&mut master, std::ptr::null_mut(), &mut original, &mut size) })?;
    if pid == 0 {
        drop(parent);
        return shell(Some(Transcript(child)));
    }
    drop(child);
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let result = relay(File::from(master), parent, original, size);
    let status = reap(pid)?;
    result.map(|signal| {
        if signal == 0 {
            status
        } else {
            128 + signal as u8
        }
    })
}

fn relay(
    mut master: File,
    mut channel: UnixStream,
    original: libc::termios,
    mut size: libc::winsize,
) -> io::Result<libc::c_int> {
    let fd = master.as_raw_fd();
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) })?;
    let _restore = OuterTerminal::raw(original)?;
    let mut output = io::stdout().lock();
    let mut control = channel.as_raw_fd();
    let mut capture = None;
    let mut at_line_start = true;
    let mut pending = Vec::new();
    let mut buf = [0; CHUNK];
    loop {
        let signal = SIGNAL.load(Ordering::Relaxed);
        if signal != 0 {
            return Ok(signal);
        }
        let stdin = if pending.is_empty() { TTY } else { -1 };
        let events = libc::POLLIN | if pending.is_empty() { 0 } else { libc::POLLOUT };
        let mut poll = [
            pollfd(fd, events),
            pollfd(control, libc::POLLIN),
            pollfd(stdin, libc::POLLIN),
        ];
        if unsafe { libc::poll(poll.as_mut_ptr(), poll.len() as _, 50) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if let Ok(next) = window_size() {
            if next.ws_row != size.ws_row || next.ws_col != size.ws_col {
                let _ = unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &next) };
                size = next;
            }
        }
        if poll[0].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0
            && !drain(&mut master, &mut output, &mut capture, &mut at_line_start)?
        {
            return Ok(0);
        }
        if poll[1].revents != 0 {
            let count = channel.read(&mut buf[..1])?;
            if count == 0 {
                control = -1;
            } else {
                // Drain before acknowledging either boundary: prompts and editor
                // echo stay outside the snapshot, even on very fast commands.
                drain(&mut master, &mut output, &mut capture, &mut at_line_start)?;
                match buf[0] {
                    b'b' | b's' => {
                        capture = (buf[0] == b'b' && pending.is_empty()).then(Vec::new);
                        channel.write_all(b"\0")?;
                    }
                    b'f' | b'l' => {
                        // Separate the next prompt or answer from command output
                        // without adding a newline to the captured bytes.
                        if buf[0] == b'l' && !at_line_start {
                            output.write_all(b"\r\n")?;
                            output.flush()?;
                            at_line_start = true;
                        }
                        let snapshot = capture.take().unwrap_or_default();
                        channel.write_all(&(snapshot.len() as u32).to_be_bytes())?;
                        channel.write_all(&snapshot)?;
                    }
                    _ => return Err(io::Error::other("invalid terminal relay message")),
                }
            }
        }
        if poll[2].revents != 0 {
            let count = io::stdin().read(&mut buf)?;
            if count == 0 {
                return Ok(0);
            }
            // Do not feed interactive programs' input (including terminal
            // echo) back to the provider, even if the program copies it out.
            capture = None;
            pending.extend_from_slice(&buf[..count]);
        }
        if !pending.is_empty() {
            match master.write(&pending) {
                Ok(count) => {
                    pending.drain(..count);
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(0),
                Err(error) => return Err(error),
            }
        }
    }
}

fn drain(
    master: &mut File,
    output: &mut impl Write,
    capture: &mut Option<Vec<u8>>,
    at_line_start: &mut bool,
) -> io::Result<bool> {
    let mut buf = [0; CHUNK];
    for _ in 0..64 {
        let count = match master.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(count) => count,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(true),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => return Ok(false),
            Err(error) => return Err(error),
        };
        let bytes = &buf[..count];
        output.write_all(bytes)?;
        output.flush()?;
        *at_line_start = bytes.ends_with(b"\n");
        if let Some(snapshot) = capture {
            if snapshot.len() + count > LIMIT {
                *capture = None;
            } else {
                snapshot.extend_from_slice(bytes);
            }
        }
    }
    // Output from a detached writer may never quiesce. Keep relaying it,
    // but do not attribute it to this command.
    *capture = None;
    Ok(true)
}

fn plain_text(bytes: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");
    let mut plain = String::new();
    let mut rest = text.as_str();
    while let Some((before, after)) = rest.split_once('\x1b') {
        plain.push_str(before);
        // Accept color/style changes, not cursor control, alternate screens,
        // clipboard sequences, or other terminal-view data.
        let after = after.strip_prefix('[')?;
        let end = after.find(|ch: char| !(ch.is_ascii_digit() || matches!(ch, ';' | ':')))?;
        rest = after[end..].strip_prefix('m')?;
    }
    plain.push_str(rest);
    (!plain
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t')))
    .then_some(plain)
}

pub(crate) fn input_pending() -> bool {
    let mut stdin = pollfd(TTY, libc::POLLIN);
    unsafe { libc::poll(&mut stdin, 1, 0) != 0 }
}

fn terminal_device(fd: RawFd) -> Option<libc::dev_t> {
    let mut stat = MaybeUninit::uninit();
    (unsafe { libc::isatty(fd) == 1 && libc::fstat(fd, stat.as_mut_ptr()) == 0 })
        .then(|| unsafe { stat.assume_init().st_rdev })
}

fn window_size() -> io::Result<libc::winsize> {
    let mut size = MaybeUninit::uninit();
    cvt(unsafe { libc::ioctl(TTY, libc::TIOCGWINSZ, size.as_mut_ptr()) })?;
    Ok(unsafe { size.assume_init() })
}

fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

fn cvt(value: libc::c_int) -> io::Result<libc::c_int> {
    if value == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(value)
    }
}

struct OuterTerminal {
    original: libc::termios,
    handlers: Vec<(libc::c_int, libc::sigaction)>,
}

extern "C" fn interrupted(signal: libc::c_int) {
    SIGNAL.store(signal, Ordering::Relaxed);
}

impl OuterTerminal {
    fn raw(original: libc::termios) -> io::Result<Self> {
        let mut terminal = Self {
            original,
            handlers: Vec::new(),
        };
        for signal in SIGNALS {
            let mut next = unsafe { std::mem::zeroed::<libc::sigaction>() };
            next.sa_sigaction = interrupted as *const () as usize;
            let mut old = MaybeUninit::uninit();
            cvt(unsafe { libc::sigaction(signal, &next, old.as_mut_ptr()) })?;
            terminal
                .handlers
                .push((signal, unsafe { old.assume_init() }));
        }
        let mut raw = original;
        unsafe { libc::cfmakeraw(&mut raw) };
        set_attributes(&raw)?;
        Ok(terminal)
    }
}

impl Drop for OuterTerminal {
    fn drop(&mut self) {
        restore_attributes(&self.original);
        for (signal, old) in &self.handlers {
            unsafe { libc::sigaction(*signal, old, std::ptr::null_mut()) };
        }
    }
}

fn reap(pid: libc::pid_t) -> io::Result<u8> {
    for _ in 0..10 {
        if let Some(status) = wait(pid, libc::WNOHANG)? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    // The master is closed. If the child didn't exit on terminal loss,
    // clean it up rather than leaving kwsh behind.
    unsafe { libc::kill(pid, libc::SIGKILL) };
    Ok(wait(pid, 0)?.unwrap_or(1))
}

fn wait(pid: libc::pid_t, flags: libc::c_int) -> io::Result<Option<u8>> {
    let mut status = 0;
    loop {
        match cvt(unsafe { libc::waitpid(pid, &mut status, flags) }) {
            Ok(0) => return Ok(None),
            Ok(_) if libc::WIFEXITED(status) => return Ok(Some(libc::WEXITSTATUS(status) as u8)),
            Ok(_) => return Ok(Some(128 + libc::WTERMSIG(status) as u8)),
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::plain_text;

    #[test]
    fn agent_output_with_or_without_a_final_newline_leaves_the_prompt_on_its_own_line() {
        use super::*;
        use std::process::{Command, Stdio};

        const CHILD: &str = "KWSH_TEST_AGENT_OUTPUT_CHILD";
        const TEST: &str = "transcript::tests::agent_output_with_or_without_a_final_newline_leaves_the_prompt_on_its_own_line";
        if std::env::var_os(CHILD).is_some() {
            let status = run(|transcript| {
                let mut transcript = transcript.unwrap();
                let mut stdout = io::stdout().lock();
                for text in ["bar", "baz\n"] {
                    transcript.begin(false)?;
                    stdout.write_all(text.as_bytes())?;
                    stdout.flush()?;
                    assert_eq!(transcript.finish_agent()?.as_deref(), Some(text));
                    writeln!(stdout, "next prompt")?;
                }
                stdout.flush()?;
                // The forked shell exits here; only the relay returns to the test harness.
                std::process::exit(0)
            })
            .unwrap();
            assert_eq!(status, 0);
            return;
        }

        let mut child = Command::new("/usr/bin/script")
            .args(["-q", "/dev/null"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env(CHILD, "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _keep_input_open = child.stdin.take().unwrap();
        let result = child.wait_with_output().unwrap();
        let stdout = String::from_utf8_lossy(&result.stdout);
        assert!(
            result.status.success() && stdout.contains("1 passed"),
            "{stdout}"
        );
        assert!(
            stdout.contains("bar\r\nnext prompt\r\nbaz\r\nnext prompt\r\n"),
            "{stdout}"
        );
    }

    #[test]
    fn retains_plain_and_styled_output_but_not_terminal_screen_updates() {
        assert_eq!(
            plain_text(b"\x1b[1;32msrc/\x1b[m\r\n").as_deref(),
            Some("src/\n")
        );
        assert_eq!(plain_text(b"\x1b[?1049halternate screen"), None);
        assert_eq!(plain_text(b"progress\rcomplete"), None);
        assert_eq!(plain_text(b"\x1b]52;data\x07"), None);
    }
}
