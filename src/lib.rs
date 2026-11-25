mod runtime;
mod syntax;

use std::{
    env,
    ffi::OsString,
    io::{self, BufRead},
    process::ExitCode,
};

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
    let option = args.next();
    let mut input = io::stdin().lock();
    let Some(option) = option else {
        let mut invocation =
            runtime::Invocation::new(name.to_str().ok_or("command name must be UTF-8")?, &[]);
        return run_input(&mut invocation, &mut input);
    };
    match option.to_str() {
        Some("--help") => {
            println!("Usage: {program} [-c command [name [args...]]]");
            return Ok(0);
        }
        Some("-c") => {}
        _ => return Err("unsupported invocation; use -c or read commands from stdin".into()),
    }
    let source = args.next().ok_or("-c requires a command")?;
    let source = source.to_str().ok_or("-c command must be UTF-8")?;
    let name = args.next().unwrap_or(name);
    let name = name.to_str().ok_or("command name must be UTF-8")?;
    let args = args
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "positional arguments must be UTF-8")?;
    let commands = syntax::parse(source).map_err(|error| error.message().to_owned())?;
    let mut invocation = runtime::Invocation::new(name, &args);
    invocation.run(&commands, &mut input);
    Ok(invocation.status)
}

fn run_input(
    invocation: &mut runtime::Invocation<'_>,
    input: &mut impl BufRead,
) -> Result<u8, String> {
    let mut source = String::new();
    while !invocation.exited {
        let count = input
            .read_line(&mut source)
            .map_err(|error| error.to_string())?;
        if count == 0 && source.is_empty() {
            break;
        }
        match syntax::parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(syntax::ParseError::Incomplete(_)) if count != 0 => continue,
            Err(error) => return Err(error.message().to_owned()),
        }
        source.clear();
    }
    Ok(invocation.status)
}
