use std::io::{self, Read, Seek, Write};
use std::os::{fd::AsRawFd, unix::process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::shell_context::ShellContext;

const MAX_REPLY: usize = 64 * 1024;

pub enum Turn {
    Cancelled,
    Reply {
        answer: String,
        draft: String,
        directory: String,
    },
}

pub fn turn(program: &Path, instruction: &str, context: &ShellContext) -> io::Result<Turn> {
    if !program.is_absolute() || instruction.len() > 8192 {
        return Err(io::Error::other(
            "provider must be an absolute path and instructions at most 8192 bytes",
        ));
    }
    let mut request = tempfile::tempfile()?;
    serde_json::to_writer(
        &mut request,
        &json!({ "instruction": instruction, "context": context }),
    )
    .map_err(io::Error::other)?;
    request.write_all(b"\n")?;
    request.rewind()?;
    let mut child = Provider(
        Command::new(program)
            .process_group(0)
            .stdin(Stdio::from(request))
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let mut reply = child.0.stdout.take().unwrap();
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut bytes = Vec::new();
    let mut buffer = [0; 4096];
    let mut eof = false;
    loop {
        if Instant::now() >= deadline {
            return Err(io::Error::other("provider timed out"));
        }
        if let Some(status) = if eof { child.0.try_wait()? } else { None } {
            if !status.success() {
                return Err(io::Error::other("provider failed"));
            }
            break;
        }
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if eof { -1 } else { reply.as_raw_fd() },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, 100) } < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if fds[0].revents != 0 {
            let mut key = 0u8;
            let count = unsafe { libc::read(libc::STDIN_FILENO, (&mut key as *mut u8).cast(), 1) };
            if count == 0 || key == 3 {
                return Ok(Turn::Cancelled);
            }
        }
        if fds[1].revents != 0 {
            let count = reply.read(&mut buffer)?;
            eof = count == 0;
            bytes.extend_from_slice(&buffer[..count]);
            if bytes.len() > MAX_REPLY {
                return Err(io::Error::other("provider reply exceeded 64 KiB"));
            }
        }
    }
    let reply: Value =
        serde_json::from_slice(&bytes).map_err(|_| io::Error::other("invalid provider reply"))?;
    if !reply.is_object() {
        return Err(io::Error::other("expected a provider reply object"));
    }
    let answer = field(&reply, "answer")?;
    let directory = field(&reply, "directory")?;
    let draft = field(&reply, "draft")?;
    if draft.chars().any(char::is_control) {
        return Err(io::Error::other(
            "provider draft must be a single printable line",
        ));
    }
    Ok(Turn::Reply {
        answer,
        draft,
        directory,
    })
}

fn field(reply: &Value, name: &str) -> io::Result<String> {
    reply.get(name).map_or(Ok(String::new()), |value| {
        value
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or_else(|| io::Error::other("invalid provider reply field"))
    })
}

struct Provider(Child);

impl Drop for Provider {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            unsafe { libc::kill(-(self.0.id() as libc::pid_t), libc::SIGKILL) };
            let _ = self.0.wait();
        }
    }
}
