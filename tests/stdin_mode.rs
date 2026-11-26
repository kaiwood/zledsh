use std::io::{Seek, Write};
use std::process::{Command, Output, Stdio};

fn from_pipe(args: &[&str], input: &[u8]) -> Output {
    run_with_pipe(Command::new(env!("CARGO_BIN_EXE_zledsh")).args(args), input)
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

// POSIX sh: OPTIONS (-s), OPERANDS (argument), and STDIN.
#[test]
fn s_flag_reads_commands_from_stdin_and_treats_operands_as_arguments() {
    let output = from_pipe(
        &["-s", "--", "-first", "two words"],
        b"printf '<%s>\\n' \"$0\" \"$#\" \"$1\" \"$2\"\nread -r data\nstdin data\nprintf '%s\\n' \"$data\"\n",
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "<{}>\n<2>\n<-first>\n<two words>\nstdin data\n",
            env!("CARGO_BIN_EXE_zledsh")
        )
    );
    assert!(output.stderr.is_empty());
}

// POSIX.1-2024 sh: OPERANDS (command_file, argument) and STDIN.
// https://pubs.opengroup.org/onlinepubs/9799919799/utilities/sh.html
#[test]
fn named_script_preserves_arguments_and_separates_commands_from_stdin() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("test script.zledsh"),
        r#"printf '<%s>\n' "$0" "$#" "$1" "$2"
read -r first
printf '<%s>\n' "$first"
/bin/cat
exit 17
printf wrong
"#,
    )
    .unwrap();
    std::fs::write(dir.path().join(".zledshrc"), "exit 99\n").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .env_remove("KWARD_CONFIG_PATH")
            .env_remove("ENV")
            .args(["test script.zledsh", "two words", "-x"]),
        b"first input line\nexternal command input\n",
    );
    assert_eq!(output.status.code(), Some(17));
    assert_eq!(
        output.stdout,
        b"<test script.zledsh>\n<2>\n<two words>\n<-x>\n<first input line>\nexternal command input\n"
    );
    assert!(output.stderr.is_empty());
}

// sh OPERANDS: a pathname with a slash need not be executable; $0 is the operand.
#[test]
fn named_script_with_slash_preserves_operand_spelling() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("script"),
        r#"printf '<%s>\n' "$0" "$#" "$1""#,
    )
    .unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .args(["./script", ""]),
        b"",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"<./script>\n<1>\n<>\n");
    assert!(output.stderr.is_empty());
}

// sh OPTIONS refers to XBD 12.2 Utility Syntax Guidelines (guideline 10).
#[test]
fn end_of_options_allows_a_script_name_starting_with_a_dash() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("--help"), "printf '%s' \"$0\"").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(dir.path())
            .args(["--", "--help"]),
        b"",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"--help");
    assert!(output.stderr.is_empty());
}

// sh STDIN and EXIT STATUS: a missing command file is an error, not stdin mode.
#[test]
fn missing_named_script_fails_without_reading_stdin_as_commands() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("stdin");
    std::fs::write(&input, "printf wrong\n").unwrap();
    let mut input = std::fs::File::open(input).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .arg("missing.zledsh")
        .stdin(input.try_clone().unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(127));
    assert_eq!(input.stream_position().unwrap(), 0);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing.zledsh"));
}

// POSIX sh: OPTIONS (-i) and Shell Language 2.8.1 (interactive syntax errors).
#[test]
fn forced_interactive_pipe_loads_startup_and_recovers_without_terminal_editing() {
    let dir = tempfile::tempdir().unwrap();
    let startup = dir.path().join("env.zledsh");
    std::fs::write(&startup, "startup=from-env\n").unwrap();
    std::fs::write(dir.path().join(".zledshrc"), "local_rc=from-rc\n").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .env("HOME", dir.path())
            .env("ENV", &startup)
            .env("PROMPT", "ready> ")
            .env("TERM", "xterm")
            .env_remove("KWARD_CONFIG_PATH")
            .env_remove("HISTFILE")
            .args(["-i", "-s", "arg"]),
        b"printf '%s:%s:%s\\n' \"$local_rc\" \"$startup\" \"$1\"\n)\nprintf 'after:%s\\n' \"$?\"\n",
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stdout, b"from-rc:from-env:arg\nafter:2\n");
    assert!(output.stderr.starts_with(b"ready> ready> zledsh:"));
    assert!(!output.stderr.contains(&27), "unexpected terminal controls");
}

#[test]
fn forced_interactive_command_string_loads_startup_without_prompts() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(".zledshrc"), "startup=loaded\n").unwrap();
    let output = run_with_pipe(
        Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .env("HOME", dir.path())
            .env_remove("ENV")
            .env_remove("KWARD_CONFIG_PATH")
            .args(["-ic", "printf '%s' \"$startup\""]),
        b"",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"loaded");
    assert!(output.stderr.is_empty());
}

#[test]
fn read_assigns_fields_and_preserves_the_unsplit_remainder() {
    let home = tempfile::tempdir().unwrap();
    let input = home.path().join("data");
    std::fs::write(&input, b"  one : :two : three  \nleft\\:side:right\nbegin\\\nend last\n  raw\\:line  \npartial tail").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .env_clear()
        .env("HOME", home.path())
        .current_dir(home.path())
        .stdin(std::fs::File::open(input).unwrap())
        .args([
            "-c",
            r#"
IFS=' :'
read -r a b c; printf '<%s>\n' "$a" "$b" "$c"
read a b; printf '<%s>\n' "$a" "$b"
unset IFS
read a b; printf '<%s>\n' "$a" "$b"
IFS=
read -r a b; printf '<%s>\n' "$a" "$b"
unset IFS
read a b c; printf '%s:<%s>:<%s>:<%s>\n' "$?" "$a" "$b" "$c"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"<one>\n<>\n<two : three>\n<left:side>\n<right>\n<beginend>\n<last>\n<  raw\\:line  >\n<>\n1:<partial>:<tail>:<>\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}
