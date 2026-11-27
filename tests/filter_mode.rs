#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::fs;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};

fn provider(home: &Path, body: &str) {
    let path = home.join("provider");
    fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn run(home: &Path, args: &[&str], input: &[u8], local: bool) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zledsh"));
    command
        .args(args)
        .current_dir(home)
        .env("HOME", home)
        .env_remove("ZLEDSH_CHATGPT_MODEL")
        .env_remove("ZLEDSH_CHATGPT_REASONING_EFFORT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if local {
        command.env("ZLEDSH_ASSISTANT_EXEC", home.join("provider"));
    } else {
        command.env_remove("ZLEDSH_ASSISTANT_EXEC");
    }
    let mut child = command.spawn().unwrap();
    if let Err(error) = child.stdin.take().unwrap().write_all(input) {
        assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    }
    child.wait_with_output().unwrap()
}

#[test]
fn filters_the_whole_stream_without_interpreting_input_or_output() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(".zledshrc"), "touch startup-ran\n").unwrap();
    provider(
        home.path(),
        r#"/bin/cat > request.json
printf '%s\n' '{"answer":"  3 2 1\n\nlast line without newline","draft":"touch draft-ran","directory":"."}'"#,
    );
    let input = "1 2 3\n$(touch input-ran) 💫\n";
    let output = run(
        home.path(),
        &["--filter", "reverse the list"],
        input.as_bytes(),
        true,
    );
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(output.stdout, b"  3 2 1\n\nlast line without newline");
    let request: Value =
        serde_json::from_slice(&fs::read(home.path().join("request.json")).unwrap()).unwrap();
    assert_eq!(
        request,
        json!({"mode": "filter", "instruction": "reverse the list", "context": {"input": input}})
    );
    for marker in ["startup-ran", "draft-ran", "input-ran"] {
        assert!(!home.path().join(marker).exists());
    }
}

fn assert_failure(output: Output, message: &str) {
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.contains(message),
        "expected stderr to contain {message:?}, got {stderr:?}"
    );
}

#[test]
fn validates_arguments_and_input_before_starting_provider() {
    let home = tempfile::tempdir().unwrap();
    provider(home.path(), "printf ran > marker");
    assert_failure(
        run(home.path(), &["--filter"], b"", true),
        "requires one instruction",
    );
    assert_failure(
        run(home.path(), &["--filter", "a", "b"], b"", true),
        "exactly one instruction",
    );
    assert_failure(
        run(home.path(), &["--filter", "a"], b"\xff", true),
        "input must be UTF-8",
    );
    assert_failure(
        run(home.path(), &["--filter", "a"], &vec![b'a'; 65537], true),
        "input exceeded 64 KiB",
    );
    assert!(!home.path().join("marker").exists());
}

#[test]
fn preserves_an_empty_provider_answer() {
    let home = tempfile::tempdir().unwrap();
    provider(home.path(), r#"printf '%s' '{"answer":""}'"#);
    let output = run(home.path(), &["--filter", "delete everything"], b"", true);
    assert!(output.status.success(), "{output:?}");
    assert!(
        output.stdout.is_empty() && output.stderr.is_empty(),
        "{output:?}"
    );
}

#[test]
fn reports_provider_failures() {
    for (body, message) in [
        (
            r#"printf '%s' '{"answer":"partial"}'; exit 1"#,
            "provider failed",
        ),
        (r#"printf '%s' '{"draft":"only a draft"}'"#, "answer string"),
    ] {
        let home = tempfile::tempdir().unwrap();
        provider(home.path(), body);
        assert_failure(run(home.path(), &["--filter", "a"], b"", true), message);
    }
}

#[test]
fn missing_chatgpt_credentials_fail_without_running_local_override() {
    let home = tempfile::tempdir().unwrap();
    provider(home.path(), "printf ran > marker");
    for (args, local) in [
        (&["--filter", "a"][..], false),
        (&["--chatgpt", "--filter", "a"][..], true),
    ] {
        assert_failure(
            run(home.path(), args, b"input", local),
            "ChatGPT sign-in required; run zledsh --chatgpt",
        );
    }
    assert!(!home.path().join("marker").exists());
}
