use std::process::{Command, Output};

fn run(script: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .args(["-c", script])
        .args(args)
        .env_remove("KWSH_TEST_DEFAULT")
        .output()
        .unwrap()
}

#[test]
fn quoted_arguments_are_fields_not_shell_source() {
    let output = run(
        r#"printf '<%s>\n' "$0" "pre$@/post" "${KWSH_TEST_DEFAULT-fallback}" '$0'"#,
        &["name", "", "a b; exit 9", "*"],
    );
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        b"<name>\n<pre>\n<a b; exit 9>\n<*/post>\n<fallback>\n<$0>\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn empty_quoted_at_does_not_add_an_argument() {
    let output = run(r#"printf '<%s>\n' "$@" '' end"#, &[]);
    assert!(output.status.success());
    assert_eq!(output.stdout, b"<>\n<end>\n");
}

#[test]
fn redirections_are_resolved_left_to_right_and_do_not_leak() {
    let output = run(
        r#"printf first 1>&2 2>&1; printf second 2>&1 1>&2; printf third"#,
        &[],
    );
    assert!(output.status.success());
    assert_eq!(output.stderr, b"first");
    assert_eq!(output.stdout, b"secondthird");
}

#[test]
fn unsupported_syntax_is_rejected_before_execution() {
    for tail in [
        r#"printf '%s' "$(printf wrong)""#,
        "printf wrong || printf wrong",
        "printf 'unclosed",
        "if /usr/bin/false; then printf wrong; fi",
    ] {
        let script = format!("printf earlier; {tail}");
        let output = run(&script, &[]);
        assert_eq!(output.status.code(), Some(2), "{script}");
        assert!(output.stdout.is_empty(), "{script}");
        assert!(!output.stderr.is_empty(), "{script}");
    }
}
