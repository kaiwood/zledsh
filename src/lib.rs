#[cfg(any(target_os = "macos", target_os = "linux"))]
mod assistant;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod chatgpt;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod completion;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod foreground;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod history;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod interactive;
mod process;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod prompt;
mod runtime;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod shell_context;
mod syntax;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod terminal_modes;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod transcript;

use std::{
    env,
    ffi::OsString,
    io::{self, BufRead, BufReader},
    process::ExitCode,
};

const SUBSTITUTION: &str = "--kwsh-substitution";
const PIPELINE_BUILTIN: &str = "--kwsh-pipeline-builtin";

/// Launch the standalone `kwsh` shell.
pub fn launch() -> ExitCode {
    let mut args = env::args_os();
    let name = args.next().unwrap_or_else(|| "kwsh".into());
    let mut args = args.peekable();
    if args.next_if(|arg| arg == PIPELINE_BUILTIN).is_some() {
        return run_pipeline_builtin(args);
    }
    let result = if args.next_if(|arg| arg == SUBSTITUTION).is_some() {
        runtime::Invocation::run_substitution()
    } else {
        run_command(name, args)
    };
    match result {
        Ok(status) => status.into(),
        Err(error) => {
            eprintln!("kwsh: {error}");
            2.into()
        }
    }
}

fn run_pipeline_builtin(args: impl Iterator<Item = OsString>) -> ExitCode {
    let Ok(args) = args
        .map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
    else {
        return 2.into();
    };
    let Some(name) = args.first() else {
        return 2.into();
    };
    if !runtime::is_builtin(name) {
        return 2.into();
    }
    if matches!(name.as_str(), "fg" | "bg" | "jobs" | "wait") {
        eprintln!("kwsh: {name}: not available in a pipeline stage");
        return 1.into();
    }
    let command = syntax::Command {
        words: args
            .into_iter()
            .map(|arg| syntax::Word {
                parts: vec![syntax::Part::Literal(arg)],
                assignment: false,
                bare: false,
            })
            .collect(),
        redirects: Vec::new(),
        background: false,
    };
    let mut input = BufReader::with_capacity(1, process::Stdin);
    let mut invocation = runtime::Invocation::new("kwsh", &[]);
    invocation.run_pipeline(&[command], &mut input);
    invocation.status.into()
}

fn run_command(name: OsString, mut args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let option = args.next();
    // The next bytes may belong to a child reading fd 0 directly.
    let mut input = BufReader::with_capacity(1, process::Stdin);
    if option.is_none() || option.as_deref().and_then(|option| option.to_str()) == Some("--chatgpt")
    {
        let chatgpt = option.is_some();
        if chatgpt && args.next().is_some() {
            return Err("--chatgpt does not take arguments".into());
        }
        let mut invocation =
            runtime::Invocation::new(name.to_str().ok_or("command name must be UTF-8")?, &[]);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::io::IsTerminal;
            if io::stdin().is_terminal() && io::stderr().is_terminal() {
                return interactive::run(&mut invocation, &mut input, chatgpt)
                    .map_err(|error| error.to_string());
            }
        }
        if chatgpt {
            return Err("--chatgpt requires an interactive terminal on macOS or Linux".into());
        }
        return run_input(&mut invocation, &mut input);
    }
    let option = option.unwrap();
    match option.to_str() {
        Some("--help") => {
            println!("Usage: kwsh [-c command [name [args...]]]");
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            println!("       kwsh --chatgpt | --chatgpt-sign-out");
            return Ok(0);
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Some("--chatgpt-sign-out") => {
            if args.next().is_some() {
                return Err("--chatgpt-sign-out does not take arguments".into());
            }
            if chatgpt::sign_out().map_err(|error| error.to_string())? {
                println!("Signed out of ChatGPT on this machine.");
            } else {
                println!("No saved ChatGPT session.");
            }
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
    let mut invocation = runtime::Invocation::new(name, &args);
    invocation.run_source(source, &mut input)?;
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
        match invocation.parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(syntax::ParseError::Incomplete(_)) if count != 0 => continue,
            Err(error) => return Err(error.message().to_owned()),
        }
        source.clear();
    }
    Ok(invocation.status)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod test_terminal;
