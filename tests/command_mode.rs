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
        r#"printf '%s' "$((2 + 2))""#,
        "printf wrong || ; printf wrong",
        "printf 'unclosed",
        "if /usr/bin/false; then printf wrong",
        "if /usr/bin/true; then ; fi",
        "fi",
    ] {
        let script = format!("printf earlier; {tail}");
        let output = run(&script, &[]);
        assert_eq!(output.status.code(), Some(2), "{script}");
        assert!(output.stdout.is_empty(), "{script}");
        assert!(!output.stderr.is_empty(), "{script}");
    }
}

#[test]
fn command_substitution_isolates_state_and_captures_status_and_stdout() {
    let output = run(
        r#"
        value=parent
        false
        result=$(printf '%s:%s:%s\n\n' "$value" "$0" "$?"; value=child; cd /; exit 7)
        printf '%s|%s|%s\n' "$?" "$result" "$value"
        printf '<%s>' $(printf ' a b\n') "$(printf '%s' "$(printf '%s' ')')")"
        test=$(printf '%s' '$(printf not-code)')
        printf '%s' "$test"
    "#,
        &["fixture"],
    );
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        output.stdout,
        b"7|parent:fixture:1|parent\n<a><b><)>$(printf not-code)"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn substitution_drains_pipes_and_rejects_binary_output() {
    let output = run(
        r#"printf '%s' "$(/usr/bin/yes a | /usr/bin/head -n 40000)""#,
        &[],
    );
    assert!(output.status.success());
    assert_eq!(output.stdout.len(), 79999);
    let output = run(r#"value=$(/usr/bin/printf '\000')"#, &[]);
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("NUL byte"));
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
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an interactive terminal"));
}
