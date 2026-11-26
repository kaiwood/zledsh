use std::process::{Command, Output};

fn run(source: &str) -> Output {
    let dir = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
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
fn eval_in_a_pipeline_inherits_functions_aliases_and_arrays() {
    let output = run(r#"
items=(first second)
show() { printf '%s\n' "${items[1]}"; }
alias zledsh_pipeline_show=show
true | command eval zledsh_pipeline_show
"#);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"second\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn pipeline_expansion_and_redirection_do_not_change_parent_variables() {
    let output = run(r#"
value=0
printf '%s\n' "$((value=1))" 3>&"$((value=1))" | /bin/cat
printf 'parent:%s\n' "$value"
: 3>&"$((value=1))" | /bin/cat
printf 'parent:%s\n' "$value"
"#);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"1\nparent:0\nparent:0\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn fatal_expansion_only_exits_its_stage() {
    let output = run(r#"
unset value
printf '%s\n' "${value:?missing}" | /bin/cat
printf 'default:%s\n' "$?"
set -o pipefail
printf '%s\n' "${value:?missing}" | /bin/cat
printf 'pipefail:%s\n' "$?"
"#);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"default:0\npipefail:2\n");
    assert_eq!(
        output.stderr,
        b"zledsh: value: missing\nzledsh: value: missing\n"
    );
}

#[test]
fn background_external_exec_keeps_the_stage_pid() {
    let output = run(r#"
/bin/sh -c 'printf "%s\n" "$$"' &
last=$!
wait "$last"
printf '%s\n' "$!" | /bin/cat
"#);
    assert!(output.status.success(), "{output:?}");
    let stdout = std::str::from_utf8(&output.stdout).unwrap();
    let pids: Vec<_> = stdout.lines().collect();
    assert_eq!(pids.len(), 2, "{stdout:?}");
    assert!(pids[0].parse::<u32>().is_ok(), "{stdout:?}");
    assert_eq!(pids[0], pids[1]);
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

#[test]
fn compound_and_function_stages_inherit_pipes_but_isolate_state() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "-c",
            r#"
value=outer
transform() { read -r line; value=inner; printf '%s:%s\n' "$line" "$value"; return 7; }
{ value=left; printf '%s\n' "$value"; } | transform
printf 'status:%s value:%s\n' "$?" "$value"
( printf 'subshell\n' ) | if read -r line; then printf '<%s>\n' "$line"; fi
printf '%s\n' "${line-unset}"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"left:inner\nstatus:7 value:outer\n<subshell>\nunset\n"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn pipefail_uses_the_rightmost_failure_and_negates_after_selection() {
    let output = run(r#"
set -o pipefail
exit 19 | exit 7 | true
printf '%s\n' "$?"
! exit 19 | true
printf '%s\n' "$?"
! true | true
printf '%s\n' "$?"
( exit 13 | true )
printf '%s\n' "$?"
set +o pipefail
exit 19 | set -o pipefail
printf '%s\n' "$?"
exit 19 | true
printf '%s\n' "$?"
"#);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"7\n0\n1\n13\n0\n0\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn background_pipeline_remembers_pipefail_from_invocation() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args([
            "-o",
            "pipefail",
            "-c",
            r#"
exit 19 | true &
pid=$!
set +o pipefail
wait "$pid"
printf '%s\n' "$?"
exit 19 | true &
pid=$!
set -o pipefail
wait "$pid"
printf '%s\n' "$?"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"19\n0\n");
    assert!(output.stderr.is_empty(), "{output:?}");
}
