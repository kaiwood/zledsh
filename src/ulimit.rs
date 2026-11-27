use std::{io, mem::MaybeUninit};

const RESOURCES: &[(char, &str, libc::c_int, libc::rlim_t)] = &[
    ('c', "core file size", libc::RLIMIT_CORE as _, 512),
    ('d', "data segment size", libc::RLIMIT_DATA as _, 1024),
    ('f', "file size", libc::RLIMIT_FSIZE as _, 512),
    ('n', "open files", libc::RLIMIT_NOFILE as _, 1),
    ('s', "stack size", libc::RLIMIT_STACK as _, 1024),
    ('v', "address space", libc::RLIMIT_AS as _, 1024),
];

pub(crate) fn run(args: &[String]) -> Result<String, String> {
    let (mut hard, mut soft, mut all, mut selected) = (false, false, false, None);
    let mut args = args.iter().peekable();
    while let Some(flag) = args.next_if(|arg| arg.starts_with('-')) {
        match flag.as_str() {
            "--" => break,
            "-H" => hard = true,
            "-S" => soft = true,
            "-a" => all = true,
            _ => {
                let resource = RESOURCES
                    .iter()
                    .find(|&&(key, ..)| flag.as_bytes() == [b'-', key as u8])
                    .ok_or_else(|| format!("unsupported option: {flag}"))?;
                selected = Some(resource);
            }
        }
    }
    let operand = args.next();
    if (all && (operand.is_some() || selected.is_some())) || args.next().is_some() {
        return Err("expected one resource limit or -a".into());
    }
    if all {
        let mut output = String::new();
        for &(key, label, resource, unit) in RESOURCES {
            let value = current(resource, hard && !soft, unit)?;
            output.push_str(&format!("{label} ({unit}-unit, -{key}) {value}\n"));
        }
        return Ok(output);
    }
    let &(_, _, resource, unit) = selected.unwrap_or(&RESOURCES[2]);
    let Some(operand) = operand else {
        return current(resource, hard && !soft, unit).map(|value| format!("{value}\n"));
    };
    let value = if operand == "unlimited" {
        libc::RLIM_INFINITY
    } else {
        operand
            .parse::<libc::rlim_t>()
            .ok()
            .and_then(|n| n.checked_mul(unit))
            .filter(|&n| n != libc::RLIM_INFINITY)
            .ok_or("invalid limit")?
    };
    let mut limits = get(resource)?;
    if hard || !soft {
        limits.rlim_max = value;
    }
    if soft || !hard {
        limits.rlim_cur = value;
    }
    // Only the calling shell process is affected, and its children inherit the result.
    if unsafe { libc::setrlimit(resource as _, &limits) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(String::new())
}

fn get(resource: libc::c_int) -> Result<libc::rlimit, String> {
    let mut limits = MaybeUninit::uninit();
    // getrlimit initializes the structure on success.
    if unsafe { libc::getrlimit(resource as _, limits.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(unsafe { limits.assume_init() })
}

fn current(resource: libc::c_int, hard: bool, unit: libc::rlim_t) -> Result<String, String> {
    let limits = get(resource)?;
    let value = if hard {
        limits.rlim_max
    } else {
        limits.rlim_cur
    };
    Ok(if value == libc::RLIM_INFINITY {
        "unlimited".into()
    } else {
        (value / unit).to_string()
    })
}
