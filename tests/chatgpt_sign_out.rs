#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink};
use std::path::Path;
use std::process::{Command, Output};

fn sign_out(home: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .arg("--chatgpt-sign-out")
        .env("HOME", home)
        .output()
        .unwrap()
}

fn private_file(path: &Path) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .unwrap()
        .write_all(b"fixture; no credentials")
        .unwrap();
}

#[test]
fn sign_out_removes_current_and_legacy_sessions_but_not_preferences() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".kwsh");
    assert!(sign_out(home.path()).status.success());
    assert!(!directory.exists());
    DirBuilder::new().mode(0o700).create(&directory).unwrap();
    let session = directory.join("chatgpt-session");
    let legacy = home.path().join(".kwsh-chatgpt-session");
    let preferences = directory.join("chatgpt.json");
    let host = directory.join("chatgpt-host");
    for path in [&session, &legacy, &preferences, &host] {
        private_file(path);
    }
    assert!(sign_out(home.path()).status.success());
    assert!(!session.exists());
    assert!(!legacy.exists());
    assert!(preferences.exists());
    assert!(host.exists());
    assert!(sign_out(home.path()).status.success());
}

#[test]
fn sign_out_does_not_follow_a_session_symlink() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join(".kwsh");
    DirBuilder::new().mode(0o700).create(&directory).unwrap();
    let target = home.path().join("fixture");
    private_file(&target);
    symlink(&target, directory.join("chatgpt-session")).unwrap();
    assert!(!sign_out(home.path()).status.success());
    assert_eq!(fs::read(target).unwrap(), b"fixture; no credentials");
}
