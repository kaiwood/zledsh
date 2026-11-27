use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TIMEOUT: Duration = Duration::from_secs(8);
static OPEN_PTY: Mutex<()> = Mutex::new(());
const ROLE: &str = "ZLEDSH_TEST_TERMINAL_ROLE";
const TEST_BINARY: &str = "ZLEDSH_TEST_BINARY";

pub struct Terminal {
    child: Child,
    master: File,
    pending: Vec<u8>,
    home: Option<tempfile::TempDir>,
}

fn cvt(result: libc::c_int) -> io::Result<libc::c_int> {
    if result == -1 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

impl Terminal {
    pub fn start(prompt: &str, env: &[(&str, &str)], setup: impl FnOnce(&Path)) -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::create_dir(home.path().join("moved")).unwrap();
        setup(home.path());
        // Serialize openpty + CLOEXEC + spawn so another fixture cannot inherit an unmarked fd.
        let _open = OPEN_PTY.lock().unwrap_or_else(|error| error.into_inner());
        let (mut master, mut slave) = (-1, -1);
        let mut size = libc::winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: openpty initializes two new descriptors; no termios override.
        cvt(unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        })
        .unwrap();
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // Neither end should be inherited except where explicitly assigned to stdio.
        for fd in [&master, &slave] {
            cvt(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) }).unwrap();
        }
        let mut command = test_process("supervise");
        command
            .env("HOME", home.path())
            .env("PROMPT", prompt)
            .envs(env.iter().copied())
            .envs(std::env::var_os(TEST_BINARY).map(|path| (TEST_BINARY, path)))
            .current_dir(home.path())
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave);
        // SAFETY: only async-signal-safe calls before exec. stdin is already the slave.
        unsafe {
            command.pre_exec(|| {
                // An asynchronous launcher can ignore these signals. Start the
                // PTY session with normal dispositions, like a terminal login.
                for signal in [libc::SIGINT, libc::SIGQUIT] {
                    if libc::signal(signal, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(io::Error::last_os_error());
                    }
                }
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            pending: Vec::new(),
            home: Some(home),
        }
    }

    pub fn home(&self) -> &Path {
        self.home.as_ref().unwrap().path()
    }

    pub fn send(&mut self, input: &str) {
        self.master.write_all(input.as_bytes()).unwrap();
    }

    pub fn expect(&mut self, expected: &str) -> String {
        assert!(!expected.is_empty());
        let deadline = Instant::now() + TIMEOUT;
        loop {
            if let Some(start) = self
                .pending
                .windows(expected.len())
                .position(|w| w == expected.as_bytes())
            {
                return String::from_utf8_lossy(
                    &self
                        .pending
                        .drain(..start + expected.len())
                        .collect::<Vec<_>>(),
                )
                .into_owned();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let ready = cvt(unsafe { libc::poll(&mut poll, 1, left.as_millis() as i32) }).unwrap();
            assert!(
                ready > 0,
                "timed out waiting for {expected:?}; pending: {:?}",
                String::from_utf8_lossy(&self.pending)
            );
            let mut buf = [0; 4096];
            let n = self.master.read(&mut buf).unwrap_or_else(|error| {
                panic!(
                    "{error} waiting for {expected:?}; pending: {:?}",
                    String::from_utf8_lossy(&self.pending)
                )
            });
            assert!(
                n > 0,
                "PTY closed waiting for {expected:?}; pending: {:?}",
                String::from_utf8_lossy(&self.pending)
            );
            self.pending.extend_from_slice(&buf[..n]);
        }
    }

    pub fn finish(mut self) -> tempfile::TempDir {
        self.send("exit 0\n");
        self.expect("tty restored\r\n");
        assert!(until(|| self.child.try_wait().unwrap()).success());
        self.home.take().unwrap()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let group = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
            if group > 0 && group != unsafe { libc::getpgrp() } {
                unsafe {
                    libc::kill(-group, libc::SIGKILL);
                }
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

pub fn until<T>(mut probe: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(value) = probe() {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for fixture");
        std::thread::sleep(Duration::from_millis(10));
    }
}

pub fn executable(path: &Path, source: &str) {
    fs::write(path, source).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn test_process(role: &str) -> Command {
    let mut child = Command::new(std::env::current_exe().unwrap());
    child
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .env("TERM", "xterm")
        .env(ROLE, role)
        .args(["--exact", "pty::terminal_child", "--nocapture"]);
    child
}

fn modes() -> Vec<u8> {
    let result = Command::new("/bin/stty")
        .arg("-g")
        .stdin(Stdio::inherit())
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    result.stdout
}

// A session leader must outlive zledsh so macOS does not revoke the slave
// before the restoration check. Re-exec the test binary instead of using Ruby.
#[test]
fn terminal_child() {
    if std::env::var_os(ROLE).as_deref() != Some("supervise".as_ref()) {
        return;
    }
    let before = modes();
    let fd9 = File::create("fd9").unwrap();
    let binary =
        std::env::var_os(TEST_BINARY).unwrap_or_else(|| env!("CARGO_BIN_EXE_zledsh").into());
    assert!(
        Path::new(&binary).is_absolute(),
        "{TEST_BINARY} must be an absolute path"
    );
    let mut shell = Command::new(binary);
    shell.env_remove(TEST_BINARY);
    // SAFETY: dup2 is async-signal-safe and fd9 is owned until the child exits.
    unsafe {
        shell.pre_exec(move || {
            cvt(libc::dup2(fd9.as_raw_fd(), 9))?;
            cvt(libc::fcntl(9, libc::F_SETFD, 0))?;
            Ok(())
        });
    }
    assert!(shell.status().unwrap().success());
    assert_eq!(
        modes(),
        before,
        "zledsh did not restore its initial terminal modes"
    );
    println!("tty restored");
}

#[test]
fn terminal_probe() {
    if std::env::var_os(ROLE).is_none() {
        return;
    }
    fn attrs() -> libc::termios {
        let mut termios = std::mem::MaybeUninit::uninit();
        // SAFETY: tcgetattr initializes the struct on success.
        cvt(unsafe { libc::tcgetattr(0, termios.as_mut_ptr()) }).unwrap();
        unsafe { termios.assume_init() }
    }
    assert_eq!(cvt(unsafe { libc::tcgetpgrp(0) }).unwrap(), unsafe {
        libc::getpgrp()
    });
    let mut termios = attrs();
    assert_eq!(
        termios.c_lflag & (libc::ICANON | libc::ISIG),
        libc::ICANON | libc::ISIG
    );
    termios.c_lflag &= !libc::ECHO;
    cvt(unsafe { libc::tcsetattr(0, libc::TCSANOW, &termios) }).unwrap();
    println!("foreground ready");
    io::stdout().flush().unwrap();
    let mut input = String::new();
    io::stdin().read_line(&mut input).unwrap();
    assert_eq!(
        attrs().c_lflag & libc::ECHO,
        0,
        "stopped job tty modes were not restored"
    );
    println!("foreground read: {}", input.trim_end());
}

#[test]
fn job_control_probe() {
    if std::env::var_os(ROLE).is_none() {
        return;
    }
    // Never fork while the test is about to stop this group. On Linux a
    // stopped vfork child can pin its parent before waitpid sees a stop.
    let group = unsafe { libc::getpgrp() };
    assert_eq!(cvt(unsafe { libc::tcgetpgrp(0) }).unwrap(), group);
    let mut resumed = false;
    // Observe a real SIGCONT, not just the shell's bookkeeping in jobs.
    static CONTINUED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    use std::sync::atomic::Ordering;
    extern "C" fn continued(_: libc::c_int) {
        CONTINUED.store(true, Ordering::Relaxed);
    }
    assert_ne!(
        unsafe { libc::signal(libc::SIGCONT, continued as *const () as usize) },
        libc::SIG_ERR
    );
    println!("worker ready");
    io::stdout().flush().unwrap();
    until(|| {
        if CONTINUED.load(Ordering::Relaxed) && !resumed {
            resumed = true;
            println!("worker resumed");
            io::stdout().flush().unwrap();
        }
        (resumed && unsafe { libc::tcgetpgrp(0) } == group).then_some(())
    });
    println!("worker foreground");
    io::stdout().flush().unwrap();
    loop {
        unsafe { libc::pause() };
    }
}
