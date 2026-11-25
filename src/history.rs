use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use crate::runtime::Invocation;

type Entry = (usize, String);

pub struct History {
    entries: Vec<Entry>,
    pub next: usize,
    pub paused: bool,
    limit: usize,
    path: Option<PathBuf>,
}

impl History {
    pub fn new(shell: &Invocation<'_>) -> Self {
        let limit = shell
            .variable("HISTSIZE")
            .and_then(|size| size.parse().ok())
            .unwrap_or(512);
        let path = shell
            .variable("HISTFILE")
            .filter(|path| !path.is_empty())
            .and_then(|path| std::path::absolute(path).ok());
        let mut history = Self {
            entries: Vec::new(),
            next: 1,
            paused: false,
            limit,
            path,
        };
        history.refresh();
        history
    }

    pub fn entries(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
        self.entries.iter().map(|(_, command)| command.as_str())
    }

    pub fn control(&mut self, source: &str) -> bool {
        match source.trim() {
            "history pause" => self.paused = true,
            "history resume" => self.paused = false,
            "history status" => println!(
                "Shell history is {}.",
                if self.paused { "paused" } else { "recording" }
            ),
            _ => return false,
        }
        true
    }

    pub fn refresh(&mut self) {
        if self.paused {
            return;
        }
        if let Some(entries) = self.disk(read_entries) {
            self.replace(entries);
        }
    }

    pub fn record(&mut self, source: &str) {
        if self.paused || source.trim().is_empty() {
            return;
        }
        let text = source.strip_suffix('\n').unwrap_or(source).to_owned();
        let next = self.next;
        let limit = self.limit;
        if let Some(entries) = self.disk(|path| {
            let mut entries = read_entries(path)?;
            entries.push((next.max(next_number(&entries)), text.clone()));
            trim(&mut entries, limit);
            persist(path, &entries)?;
            Ok(entries)
        }) {
            self.next = self.next.saturating_add(1);
            self.replace(entries);
        } else {
            self.entries.push((next, text));
            trim(&mut self.entries, limit);
            self.next = next.saturating_add(1);
        }
    }

    fn replace(&mut self, mut entries: Vec<Entry>) {
        self.next = self.next.max(next_number(&entries));
        trim(&mut entries, self.limit);
        self.entries = entries;
    }

    fn disk<T>(&mut self, action: impl FnOnce(&Path) -> io::Result<T>) -> Option<T> {
        let path = self.path.as_deref()?;
        match with_lock(path, action) {
            Ok(value) => Some(value),
            Err(_) => {
                self.path = None;
                None
            }
        }
    }
}

fn trim(entries: &mut Vec<Entry>, limit: usize) {
    entries.drain(..entries.len().saturating_sub(limit));
}

fn next_number(entries: &[Entry]) -> usize {
    entries
        .last()
        .map_or(1, |(number, _)| number.saturating_add(1))
}

fn with_lock<T>(path: &Path, action: impl FnOnce(&Path) -> io::Result<T>) -> io::Result<T> {
    let mut lock_path = OsString::from(path);
    lock_path.push(".lock");
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(lock_path)?;
    validate(&lock)?;
    loop {
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    action(path)
}

fn validate(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if metadata.is_file()
        && metadata.uid() == unsafe { libc::getuid() }
        && metadata.nlink() == 1
        && metadata.mode() & 0o7777 == 0o600
    {
        Ok(())
    } else {
        Err(io::Error::other("unsafe history file"))
    }
}

fn read_entries(path: &Path) -> io::Result<Vec<Entry>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    validate(&file)?;
    let mut entries = Vec::new();
    for line in BufReader::new(file).lines() {
        let entry: Entry = serde_json::from_str(&line?).map_err(io::Error::other)?;
        if entry.0 < next_number(&entries) || entry.0 == usize::MAX {
            return Err(io::Error::other("invalid history number"));
        }
        entries.push(entry);
    }
    Ok(entries)
}

fn persist(path: &Path, entries: &[Entry]) -> io::Result<()> {
    let mut bytes = Vec::new();
    for entry in entries {
        serde_json::to_writer(&mut bytes, entry).map_err(io::Error::other)?;
        bytes.push(b'\n');
    }
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    if unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) } < 0 {
        return Err(io::Error::last_os_error());
    }
    if bytes.len() as u64 > limit.rlim_cur {
        return Err(io::Error::other("history exceeds file size limit"));
    }
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("missing history directory"))?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    validate(file.as_file())?;
    file.write_all(&bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}
