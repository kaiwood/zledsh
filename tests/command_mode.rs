use std::process::{Command, Output};

fn run(script: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args(["-c", script])
        .args(args)
        .env_remove("ZLEDSH_TEST_DEFAULT")
        .output()
        .unwrap()
}

#[test]
fn quoted_arguments_are_fields_not_shell_source() {
    let output = run(
        r#"printf '<%s>\n' "$0" "pre$@/post" "${ZLEDSH_TEST_DEFAULT-fallback}" '$0'"#,
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
        r#"printf '%s' "$((2 + 2)""#,
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
fn eval_runs_native_code_with_current_state_and_redirects() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args(["-c", r#"
            eval "$(printf '%s' 'name=loaded; if true; then printf "%s" "$name" | /bin/cat; fi')" > fixture
            printf '%s:' "$name"; /bin/cat fixture
            eval 'printf wrong; printf %s $((1 + 2)'; printf ':%s' "$?"
        "#])
        .output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"loaded:loaded:2");
    assert!(String::from_utf8_lossy(&output.stderr).contains("eval: expected ))"));
}

#[test]
fn source_uses_native_state_and_its_own_arguments() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("startup"),
        "value=loaded; printf '<%s>' \"$@\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#". ./startup 'from source'; printf ':%s:' "$value"; printf '<%s>' "$@""#,
            "fixture",
            "from caller",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"<from source>:loaded:<from caller>");
    assert!(output.stderr.is_empty());
}

#[test]
fn status_expands_and_unset_removes_inherited_environment() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .env("ZLEDSH_TEST_UNSET", "inherited")
        .args(["-c", r#"/usr/bin/false; printf '%s\n' "$?"; unset -v ZLEDSH_TEST_UNSET; printf '%s\n' "${ZLEDSH_TEST_UNSET-missing}"; ZLEDSH_TEST_UNSET=local; /usr/bin/printenv ZLEDSH_TEST_UNSET; printf '%s\n' "$?"; export ZLEDSH_TEST_UNSET; /usr/bin/printenv ZLEDSH_TEST_UNSET"#])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"1\nmissing\n1\nlocal\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn chatgpt_login_requires_a_terminal() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .arg("--chatgpt")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires an interactive terminal"));
}

#[test]
fn indexed_arrays_keep_sparse_indices_and_local_scope() {
    let output = run(
        r#"
        words=(first 'two words' "" $(printf 'last extra'))
        f() {
            local words=("${words[@]}")
            index=1
            printf '[%s]' "${words[index]}"
            unset "words[$index]"
            printf '<%s>' "${words[@]}"
            printf ':%s:' "${words[3]}"
        }
        f
        printf '<%s>' "${words[@]}"
        words=()
        printf '%s' "${words[@]}" done
        "#,
        &[],
    );
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        b"[two words]<first><><last><extra>:last:<first><two words><><last><extra>done"
    );
}

#[test]
fn unquoted_home_prefix_expands_as_one_field() {
    let scratch = tempfile::tempdir().unwrap();
    let home = scratch.path().join("home with spaces");
    std::fs::create_dir(&home).unwrap();
    let output = run(
        r#"
        HOME="$1"
        printf 'contents\n' > ~/.zledshrc
        /bin/cat ~/.zledshrc
        printf '<%s>\n' ~ "~/.zledshrc" \~/.zledshrc
        "#,
        &["fixture", home.to_str().unwrap()],
    );
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    assert_eq!(
        output.stdout,
        format!(
            "contents\n<{}>\n<~/.zledshrc>\n<~/.zledshrc>\n",
            home.display()
        )
        .as_bytes()
    );
    assert_eq!(
        std::fs::read(home.join(".zledshrc")).unwrap(),
        b"contents\n"
    );
}

#[test]
fn set_replaces_and_clears_positionals_without_changing_the_shell_name() {
    let output = run(
        r#"
printf '%s:%s:%s\n' "$0" "$#" "$1"
set -- -flag '' 'two words'
printf '<%s>\n' "$@"
f() { set inner; printf '%s:%s\n' "$#" "$1"; }
f argument
( set --; printf '%s\n' "$#" )
printf '%s:%s:%s\n' "$0" "$#" "$1"
printf '%s\n' "$(set -- child; printf '%s' "$1")"
true | eval 'printf "%s:%s:%s\n" "$0" "$#" "$1"; set worker'
set --
printf '%s:%s:%s\n' "$0" "$#" "${1-unset}"
"#,
        &["label", "original"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"label:1:original\n<-flag>\n<>\n<two words>\n1:inner\n0\nlabel:3:-flag\nchild\nlabel:3:-flag\nlabel:0:unset\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn option_parameter_reflects_interactive_invocation_and_is_inherited() {
    let home = tempfile::tempdir().unwrap();
    let script = r#"printf '<%s:%s>\n' "$-" "${-}"; (printf '<%s>\n' "$-"); true | eval 'printf "<%s>\n" "$-"'"#;
    for (flag, expected) in [("-c", "<:>\n<>\n<>\n"), ("-ic", "<i:i>\n<i>\n<i>\n")] {
        let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(home.path())
            .env_clear()
            .env("HOME", home.path())
            .args([flag, script])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected.as_bytes());
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn bare_set_lists_quoted_variables_without_resetting_positionals() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(home.path())
        .env_clear()
        .env("HOME", home.path())
        .env("REMOVE_ME", "inherited")
        .args([
            "-c",
            r#"unset REMOVE_ME; sample="one'two"; set > vars; printf '%s\n' "$1""#,
            "name",
            "keep",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"keep\n");
    assert!(output.stderr.is_empty(), "{output:?}");
    let vars = std::fs::read_to_string(home.path().join("vars")).unwrap();
    assert!(vars.lines().any(|line| line == "sample='one'\\''two'"));
    assert!(!vars.contains("REMOVE_ME"));
}
