use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::terminal::{read_key, retry};

pub fn turn(
    executable: &Path,
    waiting: bool,
    apply_cd: impl FnOnce(&Path) -> io::Result<PathBuf>,
) -> io::Result<Option<String>> {
    // Keep the toy protocol off the terminal, like standalone_link.rb. The
    // provider asks; the interpreter alone applies the live state change.
    let mode = if waiting {
        "--waiting-provider"
    } else {
        "--fake-provider"
    };
    let mut provider = Provider(
        Command::new(executable)
            .arg(mode)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()?,
    );
    let mut input = provider.0.stdin.take().unwrap();
    let mut output = BufReader::new(provider.0.stdout.take().unwrap());
    writeln!(input, "ask")?;
    let mut apply_cd = Some(apply_cd);
    loop {
        let mut fds = [
            libc::pollfd {
                fd: libc::STDIN_FILENO,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: output.get_ref().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        if output.buffer().is_empty() {
            retry(|| unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, -1) })?;
        }
        if fds[0].revents != 0 && matches!(read_key()?, None | Some(3)) {
            provider.0.kill()?;
            provider.0.wait()?;
            println!("^C\nturn cancelled");
            return Ok(None);
        }
        if fds[1].revents == 0 && output.buffer().is_empty() {
            continue;
        }
        let mut message = String::new();
        output.read_line(&mut message)?;
        match message.trim_end() {
            "waiting" => println!("provider waiting: {}", provider.0.id()),
            "run cd moved" => {
                let cd = apply_cd
                    .take()
                    .ok_or_else(|| io::Error::other("duplicate run request"))?;
                let cwd = cd(Path::new("moved"))?;
                writeln!(input, "result {}", cwd.display())?;
            }
            "answer mark draft" if provider.0.wait()?.success() => {
                return Ok(Some("mark draft".to_owned()));
            }
            _ => return Err(io::Error::other("unexpected fake-provider message or exit")),
        }
    }
}

struct Provider(Child);

impl Drop for Provider {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub fn fake(waiting: bool) -> io::Result<()> {
    let mut input = io::stdin().lock();
    let mut message = String::new();
    input.read_line(&mut message)?;
    if message != "ask\n" {
        return Err(io::Error::other("expected ask"));
    }
    if waiting {
        println!("waiting");
        // This pipe stays open until the shell cancels; never read the tty.
        input.read_line(&mut message)?;
    } else {
        println!("run cd moved");
        message.clear();
        input.read_line(&mut message)?;
        if !message.trim_end().ends_with("/moved") || !message.starts_with("result ") {
            return Err(io::Error::other("expected live cwd in run result"));
        }
        println!("answer mark draft");
    }
    Ok(())
}
