#[cfg(target_os = "macos")]
mod provider;
#[cfg(target_os = "macos")]
mod terminal;
#[cfg(target_os = "macos")]
#[path = "../../src/terminal_modes.rs"]
mod terminal_modes;

#[cfg(target_os = "macos")]
fn main() -> std::process::ExitCode {
    use std::io::{self, Write};

    fn run() -> io::Result<()> {
        match std::env::args().nth(1).as_deref() {
            Some("--foreground-probe") => return terminal::foreground_probe(),
            Some("--fd-probe") => return terminal::fd_probe(),
            Some("--fake-provider") => return provider::fake(false),
            Some("--waiting-provider") => return provider::fake(true),
            Some(_) => return Err(io::Error::other("unknown spike mode")),
            None => {}
        }

        let executable = std::env::current_exe()?;
        let tty = terminal::Terminal::enter()?;
        let mut stopped = None;
        let mut draft = String::new();
        loop {
            print!("spike> {draft}");
            io::stdout().flush()?;
            loop {
                match terminal::read_key()? {
                    None | Some(4) => return Ok(()),
                    Some(3) => {
                        println!("^C");
                        draft.clear();
                        break;
                    }
                    Some(b'\n' | b'\r') => {
                        println!();
                        let line = std::mem::take(&mut draft);
                        match line.as_str() {
                            "" => {}
                            "exit" => return Ok(()),
                            "pwd" => println!("cwd: {}", std::env::current_dir()?.display()),
                            "job" if stopped.is_none() => {
                                stopped = tty.start(&executable, "--foreground-probe")?;
                            }
                            "fd" if stopped.is_none() => {
                                stopped = tty.start(&executable, "--fd-probe")?;
                            }
                            "fg" if stopped.is_some() => {
                                stopped = tty.resume(stopped.take().unwrap())?
                            }
                            "?" | "? wait" => {
                                draft = provider::turn(&executable, line == "? wait", |path| {
                                    std::env::set_current_dir(path)?;
                                    std::env::current_dir()
                                })?
                                .unwrap_or_default();
                            }
                            _ if line.starts_with("mark ") => {
                                std::fs::write("marker", &line[5..])?;
                                println!("marked");
                            }
                            _ => println!("unsupported spike command"),
                        }
                        break;
                    }
                    Some(8 | 127) if draft.pop().is_some() => print!("\x08 \x08"),
                    Some(byte @ b' '..=b'~') => {
                        draft.push(char::from(byte));
                        print!("{}", char::from(byte));
                    }
                    _ => {}
                }
                io::stdout().flush()?;
            }
        }
    }

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("terminal_spike: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() -> std::process::ExitCode {
    eprintln!("terminal_spike is only supported on macOS");
    std::process::ExitCode::FAILURE
}
