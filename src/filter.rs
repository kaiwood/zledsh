use std::env;
use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use crate::{assistant, chatgpt};

const MAX_INPUT: usize = 64 * 1024;

pub(crate) fn run(
    mut args: impl Iterator<Item = OsString>,
    force_chatgpt: bool,
) -> Result<u8, String> {
    let instruction = args.next().ok_or("--filter requires one instruction")?;
    if args.next().is_some() {
        return Err("--filter takes exactly one instruction; quote it as one argument".into());
    }
    let instruction = instruction
        .to_str()
        .ok_or("--filter instruction must be UTF-8")?;
    if instruction.trim().is_empty() || instruction.len() > assistant::MAX_INSTRUCTION {
        return Err("--filter instruction must be nonempty and at most 8192 bytes".into());
    }
    if unsafe { libc::getuid() != libc::geteuid() || libc::getgid() != libc::getegid() } {
        return Err("--filter requires matching real and effective IDs".into());
    }

    let mut bytes = Vec::new();
    io::stdin()
        .take(MAX_INPUT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("could not read filter input: {error}"))?;
    if bytes.len() > MAX_INPUT {
        return Err("filter input exceeded 64 KiB".into());
    }
    let input = std::str::from_utf8(&bytes).map_err(|_| "filter input must be UTF-8")?;

    let provider = env::var_os("ZLEDSH_ASSISTANT_EXEC");
    let output = match provider {
        Some(program) if !force_chatgpt => {
            assistant::filter(Path::new(&program), instruction, input)
                .map_err(|error| format!("filter: {error}"))?
        }
        _ => hosted_filter(instruction, input).map_err(|error| error.to_string())?,
    };
    let mut stdout = io::stdout().lock();
    stdout
        .write_all(output.as_bytes())
        .and_then(|()| stdout.flush())
        .map_err(|error| format!("could not write filter output: {error}"))?;
    Ok(0)
}

fn hosted_filter(instruction: &str, input: &str) -> Result<String, chatgpt::Error> {
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or(chatgpt::Error::Message("ChatGPT requires an absolute HOME"))?;
    let model = env::var("ZLEDSH_CHATGPT_MODEL").ok();
    let effort = env::var("ZLEDSH_CHATGPT_REASONING_EFFORT").ok();
    let cancelled = AtomicBool::new(false);
    chatgpt::Session::connect(
        &home,
        model.as_deref(),
        effort.as_deref(),
        false,
        &cancelled,
    )?
    .filter(instruction, input, &cancelled)
}
