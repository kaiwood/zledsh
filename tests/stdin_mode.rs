use std::io::Write;
use std::process::{Command, Output, Stdio};

fn from_pipe(args: &[&str], input: &[u8]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .args(args)
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
