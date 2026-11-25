use std::io::Write;
use std::process::{Command, Output, Stdio};

fn from_pipe(args: &[&str], input: &[u8]) -> Output {
    run_with_pipe(Command::new(env!("CARGO_BIN_EXE_kwsh")).args(args), input)
}

fn run_with_pipe(command: &mut Command, input: &[u8]) -> Output {
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(input).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn parser_and_read_take_turns_consuming_the_same_pipe() {
    let output = from_pipe(
        &[],
        b"read first\n'not a command\nread -r second\n  \\value  \nprintf '<%s>\\n' \"$first\" \"$second\"\nexit 17\nprintf wrong\n",
    );
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(output.stdout, b"<'not a command>\n<\\value>\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn read_in_command_mode_keeps_partial_input_but_returns_failure_at_eof() {
    let output = from_pipe(&["-c", "read word; exit"], b"partial");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stderr.is_empty());

    let output = from_pipe(&["-c", "read word; printf '%s' \"$word\""], b"partial");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"partial");
}

#[test]
fn quoted_script_words_can_span_physical_lines() {
    let output = from_pipe(&[], b"printf '%s' 'line one\nline two'\n");
    assert!(output.status.success());
    assert_eq!(output.stdout, b"line one\nline two");
}

// POSIX sh: OPERANDS (command_file, argument) and STDIN.
#[test]
fn named_script_preserves_arguments_and_separates_commands_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("test script.kwsh"),
        r#"printf '<%s>\n' "$0" "$#" "$1" "$2"
read -r first
printf '<%s>\n' "$first"
/bin/cat
exit 17
printf wrong
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join(".kwshrc"), "exit 99\n").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_kwsh"))
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env_remove("KWARD_CONFIG_PATH")
            .env_remove("ENV")
            .args(["test script.kwsh", "two words", "-x"]),
        b"first input line\nexternal command input\n",
    );
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(
        output.stdout,
        b"<test script.kwsh>\n<2>\n<two words>\n<-x>\n<first input line>\nexternal command input\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn end_of_options_allows_a_script_name_starting_with_a_dash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("--help"), "printf '%s' \"$0\"").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_kwsh"))
            .current_dir(dir.path())
            .args(["--", "--help"]),
        b"",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"--help");
    assert!(output.stderr.is_empty());
}

#[test]
fn missing_named_script_fails_without_reading_stdin_as_commands() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("stdin");
    std::fs::write(&input, "printf wrong\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .current_dir(dir.path())
        .arg("missing.kwsh")
        .stdin(std::fs::File::open(input).unwrap())
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing.kwsh"));
}
