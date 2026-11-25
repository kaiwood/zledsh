mod runtime;
mod syntax;

use std::{env, ffi::OsString, process::ExitCode};

/// Launch the plain `kwsh` profile.
pub fn plain() -> ExitCode {
    let mut args = env::args_os();
    let name = args.next().unwrap_or_else(|| "kwsh".into());
    run("kwsh", name, args)
}

/// Launch the standalone `kward shell` profile.
pub fn kward_shell() -> ExitCode {
    let mut args = env::args_os();
    let mut name = args.next().unwrap_or_else(|| "kward".into());
    if args.next().as_deref() != Some("shell".as_ref()) {
        eprintln!("Usage: kward shell -c command [name [args...]]");
        return 2.into();
    }
    name.push(" shell");
    run("kward shell", name, args)
}

fn run(program: &str, name: OsString, args: impl Iterator<Item = OsString>) -> ExitCode {
    match run_command(program, name, args) {
        Ok(status) => status.into(),
        Err(error) => {
            eprintln!("kwsh: {error}");
            2.into()
        }
    }
}

fn run_command(
    program: &str,
    name: OsString,
    mut args: impl Iterator<Item = OsString>,
) -> Result<u8, String> {
    match args.next().as_deref().and_then(|arg| arg.to_str()) {
        Some("--help") => {
            println!("Usage: {program} -c command [name [args...]]");
            return Ok(0);
        }
        Some("-c") => {}
        _ => return Err("only -c command mode is implemented".into()),
    }
    let source = args.next().ok_or("-c requires a command")?;
    let source = source.to_str().ok_or("-c command must be UTF-8")?;
    let name = args.next().unwrap_or(name);
    let name = name.to_str().ok_or("command name must be UTF-8")?;
    let args = args
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "positional arguments must be UTF-8")?;
    let commands = syntax::parse(source)?;
    Ok(runtime::Invocation { name, args: &args }.run(&commands))
}
