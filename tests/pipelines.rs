use std::process::{Command, Output};

fn run(source: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args(["-c", source])
        .output()
        .unwrap()
}

#[test]
fn passes_more_than_a_pipe_buffer_and_closes_unused_ends() {
    let output = run("/usr/bin/yes | /usr/bin/head -n 100000 | /usr/bin/wc -l");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "100000");
    assert!(output.stderr.is_empty());
}

#[test]
fn builtins_pipe_output_without_changing_parent_state() {
    let output = run(
        "printf '%s\\n' first second | /usr/bin/grep second; exit 17 | /bin/cat; printf 'still here\\n'",
    );
    assert!(output.status.success());
    assert_eq!(output.stdout, b"second\nstill here\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn pipeline_builtins_keep_unexported_variables_out_of_child_environments() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env("HOME", dir.path())
        .env_remove("ZLEDSH_TEST_PRIVATE")
        .args([
            "-c",
            r#"ZLEDSH_TEST_PRIVATE=local
export ZLEDSH_TEST_PUBLIC=exported
printf '%s\n' piped | eval 'read -r line
printf "%s:%s\n" "$line" "$ZLEDSH_TEST_PRIVATE"
/usr/bin/printenv ZLEDSH_TEST_PRIVATE
printf "status=%s\n" "$?"
/usr/bin/printenv ZLEDSH_TEST_PUBLIC'
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"piped:local\nstatus=1\nexported\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn pipeline_status_is_the_last_stage_status() {
    assert_eq!(run("exit 19 | exit 7").status.code(), Some(7));
    assert_eq!(run("exit 19 | /bin/cat").status.code(), Some(0));
}

#[test]
fn rejects_missing_commands_and_does_not_confuse_quoted_pipes_with_operators() {
    assert_eq!(run("printf '%s' '|' | /bin/cat").stdout, b"|");
    let invalid = run("printf earlier; /bin/cat |");
    assert_eq!(invalid.status.code(), Some(2));
    assert!(invalid.stdout.is_empty());
}
