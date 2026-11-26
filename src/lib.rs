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
    fs::File,
    io::{self, BufRead, BufReader},
    process::ExitCode,
};

const SHELL_FDS: usize = 10;
const SUBSTITUTION: &str = "--zledsh-substitution";
const PIPELINE_BUILTIN: &str = "--zledsh-pipeline-builtin";

/// Launch the standalone `zledsh` shell.
pub fn launch() -> ExitCode {
    let mut args = env::args_os();
    let name = args.next().unwrap_or_else(|| "zledsh".into());
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
            eprintln!("zledsh: {error}");
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
    let [closed, name, ..] = args.as_slice() else {
        return 2.into();
    };
    let Ok(closed @ 0..=7) = closed.parse::<u8>() else {
        return 2.into();
    };
    if !runtime::is_builtin(name) {
        return 2.into();
    }
    if matches!(name.as_str(), "fg" | "bg" | "jobs" | "wait") {
        eprintln!("zledsh: {name}: not available in a pipeline stage");
        return 1.into();
    }
    let command = syntax::Command {
        words: args
            .into_iter()
            .skip(1)
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
    let mut invocation = runtime::Invocation::new("zledsh", &[]);
    invocation.run_pipeline_builtin(&command, &mut input, closed);
    invocation.status.into()
}

fn run_command(name: OsString, mut args: impl Iterator<Item = OsString>) -> Result<u8, String> {
    let mut force_interactive = false;
    let mut read_stdin = false;
    let mut option = args.next();
    loop {
        match option.as_deref().and_then(|arg| arg.to_str()) {
            Some("-i") => force_interactive = true,
            Some("-s") => read_stdin = true,
            Some("-is" | "-si") => {
                force_interactive = true;
                read_stdin = true;
            }
            _ => break,
        }
        option = args.next();
    }
    let end_options = option
        .as_deref()
        .is_some_and(|arg| arg == "--" || arg == "-");
    if end_options {
        option = args.next();
    }
    let flag = option
        .as_deref()
        .and_then(|arg| arg.to_str())
        .filter(|_| !end_options);
    let chatgpt = !read_stdin && !force_interactive && flag == Some("--chatgpt");
    // The next bytes may belong to a child reading fd 0 directly.
    let mut input = BufReader::with_capacity(1, process::Stdin);
    if option.is_none() || read_stdin || chatgpt {
        if chatgpt && args.next().is_some() {
            return Err("--chatgpt does not take arguments".into());
        }
        let args = if read_stdin {
            if !end_options {
                if let Some(arg) = option
                    .as_ref()
                    .filter(|arg| arg.as_encoded_bytes().starts_with(b"-"))
                {
                    return Err(format!("unsupported option: {arg:?}"));
                }
            }
            positional_args(option.into_iter().chain(args))?
        } else {
            Vec::new()
        };
        let mut invocation =
            runtime::Invocation::new(name.to_str().ok_or("command name must be UTF-8")?, &args);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            use std::io::IsTerminal;
            if io::stdin().is_terminal() && io::stderr().is_terminal() {
                return interactive::run(&mut invocation, &mut input, chatgpt)
                    .map_err(|error| error.to_string());
            }
            if force_interactive {
                return interactive::run_without_terminal(&mut invocation, &mut input)
                    .map_err(|error| error.to_string());
            }
        }
        if chatgpt {
            return Err("--chatgpt requires an interactive terminal on macOS or Linux".into());
        }
        return run_input(&mut invocation, &mut input, None, false);
    }
    let option = option.as_ref().unwrap();
    match flag {
        Some("--help") => {
            println!("Usage: zledsh [-i] [-c command [name [args...]]]");
            println!("       zledsh [-i] [--] script [args...]");
            println!("       zledsh [-i] -s [--] [args...]");
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            println!("       zledsh --chatgpt | --chatgpt-sign-out");
            Ok(0)
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        Some("--chatgpt-sign-out") if !force_interactive => {
            if args.next().is_some() {
                return Err("--chatgpt-sign-out does not take arguments".into());
            }
            if chatgpt::sign_out().map_err(|error| error.to_string())? {
                println!("Signed out of ChatGPT on this machine.");
            } else {
                println!("No saved ChatGPT session.");
            }
            Ok(0)
        }
        Some("-c" | "-ic" | "-ci") => {
            force_interactive |= flag != Some("-c");
            let source = args.next().ok_or("-c requires a command")?;
            let source = source.to_str().ok_or("-c command must be UTF-8")?;
            let name = args.next().unwrap_or(name);
            let name = name.to_str().ok_or("command name must be UTF-8")?;
            let args = positional_args(args)?;
            let mut invocation = runtime::Invocation::new(name, &args);
            load_forced_startup(force_interactive, &mut invocation, &mut input);
            if !invocation.exited {
                invocation.run_source(source, &mut input)?;
            }
            Ok(invocation.status)
        }
        _ if end_options || !option.as_encoded_bytes().starts_with(b"-") => {
            let path = option.to_str().ok_or("script name must be UTF-8")?;
            let args = positional_args(args)?;
            let script = File::open(path).map_err(|error| format!("{path}: {error}"))?;
            let mut invocation = runtime::Invocation::new(path, &args);
            load_forced_startup(force_interactive, &mut invocation, &mut input);
            run_input(
                &mut invocation,
                &mut input,
                Some(BufReader::new(script)),
                force_interactive,
            )
            .map_err(|error| format!("{path}: {error}"))
        }
        _ => Err(format!("unsupported option: {}", option.to_string_lossy())),
    }
}

fn load_forced_startup(
    force: bool,
    invocation: &mut runtime::Invocation<'_>,
    input: &mut impl BufRead,
) {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    if force {
        interactive::load_startup(invocation, input);
    }
}

fn positional_args(args: impl Iterator<Item = OsString>) -> Result<Vec<String>, String> {
    args.map(OsString::into_string)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "positional arguments must be UTF-8".into())
}

fn run_input(
    invocation: &mut runtime::Invocation<'_>,
    input: &mut impl BufRead,
    mut script: Option<BufReader<File>>,
    interactive: bool,
) -> Result<u8, String> {
    let mut source = String::new();
    while !invocation.exited {
        let count = match &mut script {
            Some(script) => script.read_line(&mut source),
            None => input.read_line(&mut source),
        }
        .map_err(|error| error.to_string())?;
        if count == 0 && source.is_empty() {
            break;
        }
        match invocation.parse(&source) {
            Ok(commands) => invocation.run(&commands, input),
            Err(syntax::ParseError::Incomplete(_)) if count != 0 => continue,
            Err(error) if interactive => {
                eprintln!("zledsh: {}", error.message());
                invocation.status = 2;
            }
            Err(error) => return Err(error.message().to_owned()),
        }
        source.clear();
    }
    Ok(invocation.status)
}

#[cfg(all(test, any(target_os = "macos", target_os = "linux")))]
mod test_terminal;
