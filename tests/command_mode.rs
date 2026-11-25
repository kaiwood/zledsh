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

#[test]
fn status_expands_and_unset_removes_inherited_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .env("KWSH_TEST_UNSET", "inherited")
        .args(["-c", r#"/usr/bin/false; printf '%s\n' "$?"; unset -v KWSH_TEST_UNSET; printf '%s\n' "${KWSH_TEST_UNSET-missing}"; KWSH_TEST_UNSET=local; /usr/bin/printenv KWSH_TEST_UNSET; printf '%s\n' "$?"; export KWSH_TEST_UNSET; /usr/bin/printenv KWSH_TEST_UNSET"#])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"1\nmissing\n1\nlocal\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn chatgpt_login_requires_a_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .arg("--chatgpt")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("requires an interactive macOS terminal")
    );
}
