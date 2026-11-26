use std::{
    collections::BTreeMap,
    io,
    mem::MaybeUninit,
    sync::atomic::{AtomicBool, Ordering},
};

// Standard signals on the supported platforms fit in this array. EXIT is
// synthetic and never installed as an OS signal handler.
const LIMIT: usize = 64;
static PENDING: [AtomicBool; LIMIT] = [const { AtomicBool::new(false) }; LIMIT];
static IGNORE_PIPE: AtomicBool = AtomicBool::new(false);

extern "C" fn received(signal: libc::c_int) {
    if let Some(pending) = PENDING.get(signal as usize) {
        pending.store(true, Ordering::Relaxed);
    }
}

pub(crate) fn pending() -> Option<libc::c_int> {
    PENDING
        .iter()
        .position(|flag| flag.load(Ordering::Relaxed))
        .map(|signal| signal as libc::c_int)
}

pub(crate) fn take_pending() -> Vec<libc::c_int> {
    PENDING
        .iter()
        .enumerate()
        .filter_map(|(signal, flag)| {
            flag.swap(false, Ordering::Relaxed)
                .then_some(signal as libc::c_int)
        })
        .collect()
}

const SIGNALS: &[(&str, libc::c_int)] = &[
    ("EXIT", 0),
    ("ABRT", libc::SIGABRT),
    ("ALRM", libc::SIGALRM),
    ("BUS", libc::SIGBUS),
    ("CHLD", libc::SIGCHLD),
    ("CONT", libc::SIGCONT),
    ("FPE", libc::SIGFPE),
    ("HUP", libc::SIGHUP),
    ("ILL", libc::SIGILL),
    ("INT", libc::SIGINT),
    ("PIPE", libc::SIGPIPE),
    ("POLL", libc::SIGIO),
    ("PROF", libc::SIGPROF),
    ("QUIT", libc::SIGQUIT),
    ("SEGV", libc::SIGSEGV),
    ("SYS", libc::SIGSYS),
    ("TERM", libc::SIGTERM),
    ("TRAP", libc::SIGTRAP),
    ("TSTP", libc::SIGTSTP),
    ("TTIN", libc::SIGTTIN),
    ("TTOU", libc::SIGTTOU),
    ("URG", libc::SIGURG),
    ("USR1", libc::SIGUSR1),
    ("USR2", libc::SIGUSR2),
    ("VTALRM", libc::SIGVTALRM),
    ("XCPU", libc::SIGXCPU),
    ("XFSZ", libc::SIGXFSZ),
    ("WINCH", libc::SIGWINCH),
];

pub(crate) fn condition(value: &str) -> Result<libc::c_int, String> {
    let number = value.parse().ok();
    let name = value.strip_prefix("SIG").unwrap_or(value);
    SIGNALS
        .iter()
        .find_map(|&(symbol, signal)| (symbol == name || number == Some(signal)).then_some(signal))
        .ok_or_else(|| format!("unknown or unsupported condition: {value}"))
}

fn action(signal: libc::c_int) -> io::Result<libc::sigaction> {
    let mut old = MaybeUninit::uninit();
    if unsafe { libc::sigaction(signal, std::ptr::null(), old.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { old.assume_init() })
}

// Used in the fork child as well: only sigaction and a lock-free atomic load.
pub(crate) fn child_ignores(signal: libc::c_int) -> bool {
    if signal == libc::SIGPIPE {
        // Rust ignores SIGPIPE on startup even when the parent did not.
        return IGNORE_PIPE.load(Ordering::Relaxed);
    }
    action(signal).is_ok_and(|old| old.sa_sigaction == libc::SIG_IGN)
}

pub(crate) fn inherit_pipe_ignore(ignored: bool) {
    IGNORE_PIPE.store(ignored, Ordering::Relaxed);
}

#[derive(Default)]
pub(crate) struct Traps {
    actions: BTreeMap<libc::c_int, String>,
    original: BTreeMap<libc::c_int, libc::sigaction>,
    pub inherited_pipe_ignore: bool,
    pub inherited: Option<BTreeMap<libc::c_int, String>>,
}

impl Traps {
    pub fn get(&self, signal: libc::c_int) -> Option<&str> {
        self.actions
            .get(&signal)
            .map(String::as_str)
            .filter(|value| !value.is_empty())
    }

    pub fn take_exit(&mut self) -> Option<String> {
        self.actions.remove(&0).filter(|value| !value.is_empty())
    }

    pub fn set(&mut self, signal: libc::c_int, value: &str, interactive: bool) -> io::Result<()> {
        if signal != 0 {
            let original = match self.original.get(&signal) {
                Some(old) => *old,
                None => action(signal)?,
            };
            if !interactive
                && original.sa_sigaction == libc::SIG_IGN
                && (signal != libc::SIGPIPE || self.inherited_pipe_ignore)
            {
                return Ok(());
            }
            let mut next = original;
            // Use the shell's original disposition on reset, including its
            // internal SIGPIPE handling; external programs get their own.
            if value != "-" {
                next = unsafe { std::mem::zeroed() };
                next.sa_flags = libc::SA_RESTART;
                next.sa_sigaction = if value.is_empty() {
                    libc::SIG_IGN
                } else {
                    received as *const () as usize
                };
                unsafe { libc::sigemptyset(&mut next.sa_mask) };
            }
            if unsafe { libc::sigaction(signal, &next, std::ptr::null_mut()) } < 0 {
                return Err(io::Error::last_os_error());
            }
            self.original.entry(signal).or_insert(original);
            if signal == libc::SIGPIPE {
                inherit_pipe_ignore(value.is_empty() || value == "-" && self.inherited_pipe_ignore);
            }
        }
        if value == "-" {
            self.actions.remove(&signal);
        } else {
            self.actions.insert(signal, value.into());
        }
        Ok(())
    }

    pub fn snapshot(&self) -> BTreeMap<libc::c_int, String> {
        let mut result = self.actions.clone();
        for &(_, signal) in SIGNALS {
            if signal != 0 && child_ignores(signal) {
                result.entry(signal).or_default();
            }
        }
        result
    }

    pub fn listing(
        &self,
        defaults: bool,
        requested: &[libc::c_int],
    ) -> Vec<(&'static str, String)> {
        let snapshot = self.inherited.clone().unwrap_or_else(|| self.snapshot());
        SIGNALS
            .iter()
            .filter_map(|&(name, signal)| {
                if !requested.is_empty() && !requested.contains(&signal) {
                    return None;
                }
                snapshot
                    .get(&signal)
                    .cloned()
                    .or_else(|| defaults.then(|| "-".into()))
                    .map(|value| (name, value))
            })
            .collect()
    }
}

impl Drop for Traps {
    fn drop(&mut self) {
        for (&signal, original) in &self.original {
            unsafe { libc::sigaction(signal, original, std::ptr::null_mut()) };
            PENDING[signal as usize].store(false, Ordering::Relaxed);
            if signal == libc::SIGPIPE {
                inherit_pipe_ignore(false);
            }
        }
    }
}
