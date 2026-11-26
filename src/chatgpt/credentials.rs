use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::{Error, Result, oauth};

#[derive(Serialize, Deserialize)]
pub(super) struct Credentials {
    pub client: String,
    pub subject: String,
    pub access: String,
    pub refresh: String,
    pub expires_at: u64,
}

impl Credentials {
    pub fn usable_at(&self, now: u64) -> bool {
        self.expires_at > now.saturating_add(60)
    }
}

pub(super) fn now() -> Result<u64> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| Error::Message("invalid system clock"))
}

pub(super) struct Store {
    path: PathBuf,
    _lock: File,
}

pub(super) const HOST_FILE: &str = "chatgpt-host";
const SESSION_FILE: &str = "chatgpt-session";

const STORE_ERROR: Error = Error::Message("could not access the private ChatGPT credentials file");

impl Store {
    pub fn lock(home: &Path, cancelled: &AtomicBool) -> Result<Self> {
        let directory = home.join(".zledsh");
        let store = Self::lock_current(&directory, cancelled)?;
        migrate(home, &directory, cancelled)?;
        Ok(store)
    }

    fn lock_current(directory: &Path, cancelled: &AtomicBool) -> Result<Self> {
        prepare_directory(directory)?;
        let lock = lock_file(&directory.join("chatgpt-session.lock"), cancelled)?;
        Ok(Self {
            path: directory.join(SESSION_FILE),
            _lock: lock,
        })
    }

    pub fn directory(&self) -> &Path {
        self.path.parent().expect("session path has a parent")
    }

    pub fn forget(home: &Path, cancelled: &AtomicBool) -> Result<bool> {
        let directory = home.join(".zledsh");
        let legacy = home.join(".zledsh-chatgpt-session");
        if !exists(&directory)? && !exists(&legacy)? {
            return Ok(false);
        }
        let store = Self::lock_current(&directory, cancelled)?;
        let legacy_removed = if exists(&legacy)? {
            let _legacy_lock = lock_file(&home.join(".zledsh-chatgpt-session.lock"), cancelled)?;
            remove_session(&legacy)?
        } else {
            false
        };
        Ok(remove_session(&store.path)? || legacy_removed)
    }

    pub fn load(&self) -> Result<Option<Credentials>> {
        let file = match options().open(&self.path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(STORE_ERROR),
        };
        validate(&file)?;
        let mut data = Vec::new();
        file.take(64 * 1024 + 1)
            .read_to_end(&mut data)
            .map_err(|_| STORE_ERROR)?;
        if data.len() > 64 * 1024 {
            return Err(STORE_ERROR);
        }
        let saved: Credentials = serde_json::from_slice(&data).map_err(|_| STORE_ERROR)?;
        if [&saved.access, &saved.refresh, &saved.client]
            .iter()
            .any(|value| {
                value.is_empty()
                    || value.len() > 16384
                    || !value.bytes().all(|b| b.is_ascii_graphic())
            })
            || saved.subject.is_empty()
        {
            return Err(STORE_ERROR);
        }
        Ok(Some(saved))
    }

    pub fn save(&self, credentials: &Credentials) -> Result<()> {
        let mut temp = tempfile::NamedTempFile::new_in(self.path.parent().ok_or(STORE_ERROR)?)
            .map_err(|_| STORE_ERROR)?;
        validate(temp.as_file())?;
        serde_json::to_writer(&mut temp, credentials).map_err(|_| STORE_ERROR)?;
        temp.as_file().sync_all().map_err(|_| STORE_ERROR)?;
        temp.persist(&self.path).map_err(|_| STORE_ERROR)?;
        Ok(())
    }
}

fn remove_session(path: &Path) -> Result<bool> {
    let file = match options().open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_) => return Err(STORE_ERROR),
    };
    validate(&file)?;
    fs::remove_file(path).map_err(|_| STORE_ERROR)?;
    Ok(true)
}

fn prepare_directory(path: &Path) -> Result<()> {
    const DIRECTORY_ERROR: Error =
        Error::Message("~/.zledsh must be a directory you own with mode 0700 (not a symlink)");
    match DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(DIRECTORY_ERROR),
    }
    let stat = fs::symlink_metadata(path).map_err(|_| DIRECTORY_ERROR)?;
    if !stat.is_dir() || stat.uid() != unsafe { libc::getuid() } || stat.mode() & 0o7777 != 0o700 {
        return Err(DIRECTORY_ERROR);
    }
    Ok(())
}

fn migrate(home: &Path, directory: &Path, cancelled: &AtomicBool) -> Result<()> {
    let paths = [
        (home.join(".zledsh-chatgpt-host"), directory.join(HOST_FILE)),
        (
            home.join(".zledsh-chatgpt-session"),
            directory.join(SESSION_FILE),
        ),
    ];
    let mut needed = false;
    for (old, new) in &paths {
        needed |= !exists(new)? && exists(old)?;
    }
    if !needed {
        return Ok(());
    }
    // Coordinate with a previous build that might still be rotating credentials.
    let _legacy_lock = lock_file(&home.join(".zledsh-chatgpt-session.lock"), cancelled)?;
    for (old, new) in paths {
        if !exists(&new)? && exists(&old)? {
            let file = options().open(&old).map_err(|_| STORE_ERROR)?;
            validate(&file)?;
            fs::rename(old, new).map_err(|_| STORE_ERROR)?;
        }
    }
    Ok(())
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(STORE_ERROR),
    }
}

fn lock_file(path: &Path, cancelled: &AtomicBool) -> Result<File> {
    let lock = options()
        .write(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| STORE_ERROR)?;
    validate(&lock)?;
    loop {
        oauth::check_cancelled(cancelled)?;
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            return Ok(lock);
        }
        let error = io::Error::last_os_error();
        if !matches!(
            error.kind(),
            io::ErrorKind::Interrupted | io::ErrorKind::WouldBlock
        ) {
            return Err(STORE_ERROR);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    options
}

fn validate(file: &File) -> Result<()> {
    let stat = file.metadata().map_err(|_| STORE_ERROR)?;
    if !stat.is_file()
        || stat.uid() != unsafe { libc::getuid() }
        || stat.nlink() != 1
        || stat.mode() & 0o7777 != 0o600
    {
        return Err(STORE_ERROR);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn moves_existing_chatgpt_state_without_overwriting_new_state() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        let running = AtomicBool::new(false);
        let legacy_host = home.join(".zledsh-chatgpt-host");
        let legacy_session = home.join(".zledsh-chatgpt-session");
        let write_private = |path: &Path, contents: &[u8]| {
            use std::io::Write;
            options()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(path)
                .unwrap()
                .write_all(contents)
                .unwrap();
        };
        write_private(
            &legacy_host,
            b"urn:uuid:00000000-0000-4000-8000-000000000000",
        );
        write_private(&legacy_session, br#"{"client":"fixture-client","subject":"fixture-subject","access":"fixture-access","refresh":"fixture-refresh","expires_at":1000}"#);
        {
            let store = Store::lock(home, &running).unwrap();
            assert_eq!(store.load().unwrap().unwrap().access, "fixture-access");
            assert_eq!(store.directory().metadata().unwrap().mode() & 0o777, 0o700);
            assert_eq!(
                fs::read(store.directory().join(HOST_FILE)).unwrap(),
                b"urn:uuid:00000000-0000-4000-8000-000000000000"
            );
            assert!(!legacy_host.exists());
            assert!(!legacy_session.exists());
        }
        write_private(&legacy_session, b"stale file");
        let store = Store::lock(home, &running).unwrap();
        assert_eq!(store.load().unwrap().unwrap().access, "fixture-access");
        assert_eq!(fs::read(&legacy_session).unwrap(), b"stale file");
    }

    #[test]
    fn rejects_insecure_or_symlinked_state_directories() {
        let home = tempfile::tempdir().unwrap();
        let directory = home.path().join(".zledsh");
        let running = AtomicBool::new(false);
        DirBuilder::new().mode(0o755).create(&directory).unwrap();
        assert!(Store::lock(home.path(), &running).is_err());
        let other = home.path().join("other");
        fs::rename(&directory, &other).unwrap();
        fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
        symlink(&other, &directory).unwrap();
        assert!(Store::lock(home.path(), &running).is_err());
        assert!(other.read_dir().unwrap().next().is_none());
    }

    #[test]
    fn reloads_the_latest_token_pair_and_rejects_unsafe_permissions() {
        let home = tempfile::tempdir().unwrap();
        let running = AtomicBool::new(false);
        let mut tokens = Credentials {
            client: "example-client".into(),
            subject: "example-account".into(),
            access: "first-access".into(),
            refresh: "first-refresh".into(),
            expires_at: 1000,
        };
        {
            let store = Store::lock(home.path(), &running).unwrap();
            assert!(store.load().unwrap().is_none());
            store.save(&tokens).unwrap();
            tokens.access = "next-access".into();
            tokens.refresh = "next-refresh".into();
            store.save(&tokens).unwrap();
        }
        let store = Store::lock(home.path(), &running).unwrap();
        let reloaded = store.load().unwrap().unwrap();
        assert_eq!(reloaded.access, "next-access");
        assert_eq!(reloaded.refresh, "next-refresh");
        assert!(reloaded.usable_at(939));
        assert!(!reloaded.usable_at(940));
        let stat = store.path.metadata().unwrap();
        assert_eq!(stat.mode() & 0o777, 0o600);
        std::fs::set_permissions(&store.path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(store.load().is_err());
        let moved = home.path().join("moved");
        std::fs::rename(&store.path, &moved).unwrap();
        std::fs::set_permissions(&moved, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&moved, &store.path).unwrap();
        assert!(store.load().is_err());
    }
}
