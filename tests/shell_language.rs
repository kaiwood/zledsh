use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn shell(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zledsh"));
    command
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .current_dir(home);
    command
}

fn assert_output(output: Output, status: i32, stdout: &str, stderr: &str) {
    assert_eq!(output.status.code(), Some(status), "{output:?}");
    assert_eq!(output.stdout, stdout.as_bytes());
    assert_eq!(output.stderr, stderr.as_bytes());
}

// sh OPTIONS (-c), STDIN, and Shell Command Language 2.7.
#[test]
fn command_status_streams_arguments_and_side_effects() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(".zledshrc"), "PARITY_RC=loaded\n").unwrap();
    let source = r#"printf '<%s>\n' "$0" "$@"
printf 'diagnostic\n' >&2
printf '%s\n' "${PARITY_RC-unset}" > artifact
exit 17
"#;
    assert_output(
        shell(home.path())
            .args(["-c", source])
            .args(["named", "a b", "--"])
            .output()
            .unwrap(),
        17,
        "<named>\n<a b>\n<-->\n",
        "diagnostic\n",
    );
    assert_eq!(fs::read(home.path().join("artifact")).unwrap(), b"unset\n");
}

// Shell Command Language 2.6.2: null and unset tests differ.
#[test]
fn nested_parameter_defaults_distinguish_unset_and_empty() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"unset RBENV_ROOT
printf '%s\n' "${RBENV_ROOT:-$HOME/.rbenv}/shims"
RBENV_ROOT=
printf '<%s> <%s>\n' "${RBENV_ROOT-fallback}" "${RBENV_ROOT:-fallback}"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        &format!("{}/.rbenv/shims\n<> <fallback>\n", home.path().display()),
        "",
    );
}

// zledsh extension: alias replacement uses the state at the start of the input unit.
#[test]
fn simple_aliases_use_the_next_input_unit() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"alias say='printf old'
alias say='printf new'; say
say
alias printf='false'
"printf" ':literal'
unalias printf
printf ':removed'
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "oldnew:literal:removed",
        "",
    );
}

// Shell Command Language 2.9.2, 2.9.3.1 and wait.
#[test]
fn background_pipeline_waits_for_its_last_status() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"/usr/bin/true | /usr/bin/false &
saved="$!"
printf '%s\n' launched
wait "$saved"
printf '%s\n' "$?"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "launched\n1\n",
        "",
    );
}

// Shell Command Language 2.9.4.4: branch selection and exit status.
#[test]
fn conditionals_in_command_and_redirected_input() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"if /usr/bin/false; then
  printf wrong > skipped
elif /usr/bin/true; then
  if /usr/bin/true; then selected=right; else selected=wrong; fi
fi
printf '%s:%s\n' "$selected" "$?"
if /usr/bin/false; then printf wrong; fi
printf '%s\n' "$?"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "right:0\n0\n",
        "",
    );
    let mut child = shell(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(source.as_bytes())
        .unwrap();
    assert_output(child.wait_with_output().unwrap(), 0, "right:0\n0\n", "");
    assert!(!home.path().join("skipped").exists());
}

// Shell Command Language 2.9.2 and 2.9.3: status, precedence, and laziness.
// The last stage drains the pipe before returning 1, so the producer cannot race a close.
#[test]
fn boolean_lists_short_circuit_with_pipeline_status() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"false && printf wrong > skipped || printf fallback
true || printf wrong > skipped && printf ':left'
if ! printf hidden | /usr/bin/grep -q '^never$' &&
  true; then printf ':negated'; fi
! true
printf ':%s' "$?"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "fallback:left:negated:1",
        "",
    );
    assert!(!home.path().join("skipped").exists());
}

// Shell Command Language 2.6.2 and 2.6.3: lazy defaults and nested substitution.
#[test]
fn substitution_and_lazy_parameter_defaults() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"word=parent
result=$(word=child; printf '%s\n\n' "$word"; exit 7)
printf '%s:%s:%s\n' "$?" "$result" "$word"
printf '<%s>' $(printf 'a b') "$(printf '%s' "$(printf '%s' ')')")"
printf '%s' "${word:-$(printf wrong > skipped)}"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "7:child:parent\n<a><b><)>parent",
        "",
    );
    assert!(!home.path().join("skipped").exists());
}

// Shell Command Language 2.9.4: loop conditions and control flow.
#[test]
fn loops_keep_state_and_control_nested_iterations() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"for item; do
  if [ "$item" = skip ]; then continue; fi
  if [ "$item" = stop ]; then break; fi
  printf '%s' "$item"
done
for outer in 1 2; do
  for inner in x y; do
    printf ':%s%s' "$outer" "$inner"
    continue 2
  done
  printf wrong
done
value=start
while [ "$value" = start ]; do value=next; done
until [ "$value" = done ]; do value=done; done
for item in; do printf wrong; done
printf ':%s:%s' "$value" "$?"
"#;
    assert_output(
        shell(home.path())
            .args(["-c", source])
            .args(["fixture", "a b", "skip", "c", "stop", "wrong"])
            .output()
            .unwrap(),
        0,
        "a bc:1x:2x:done:0",
        "",
    );
}

// zledsh extension: local scope. Positional parameters and return: 2.9.5.
#[test]
fn functions_restore_locals_and_arguments() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"value=outside
show() {
  local value="${1:-fallback}"
  if [ "$#" -gt 0 ]; then shift; fi
  printf '%s:%s' "$value" "$#"
  return 7
  printf wrong
}
show direct extra
printf ':%s:%s:%s\n' "$?" "$value" "$1"
reply=$(show 'two words' extra)
printf '%s:%s:%s:%s' "$?" "$reply" "$value" "$1"
"#;
    assert_output(
        shell(home.path())
            .args(["-c", source])
            .args(["fixture", "caller"])
            .output()
            .unwrap(),
        0,
        "direct:1:7:outside:caller\n7:two words:1:outside:caller",
        "",
    );
}

// Shell Command Language 2.9.4.3: first match and lazy arms.
#[test]
fn case_selects_the_first_match_and_expands_only_the_chosen_body() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"value=rehash
case "$value" in
  shell|rehash) printf 'first';;
  *) printf wrong > skipped;;
esac
printf ':%s' "$(case x in (x) printf yes;; esac)"
unset value
printf ':%s' "${value+x}"
value=
printf ':%s:%s' "${value+x}" "${value:+x}"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "first:yes::x:",
        "",
    );
    assert!(!home.path().join("skipped").exists());
}

#[test]
fn case_patterns_match_text_without_pathname_rules() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"
pattern='.*[0-9]'
case '/.app/7' in
  '/.*[0-9]') printf wrong;;
  /$pattern) printf 'matched\n';;
  *) printf wrong;;
esac
case 'a*b' in
  a\*b|x?) printf 'literal\n';;
  *) printf wrong;;
esac
case '.hidden' in [!x]hidden) printf 'leading dot\n';; esac
case 'a/b' in a?b) printf 'slash\n';; esac
case 'a*b' in
  a*b) printf 'first\n';;
  "$(printf wrong > skipped)") printf wrong;;
esac
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "matched\nliteral\nleading dot\nslash\nfirst\n",
        "",
    );
    assert!(!home.path().join("skipped").exists());
}

// POSIX.1-2024 2.9.4.1: brace grouping uses the current shell environment.
#[test]
fn brace_group_runs_in_the_live_shell() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("child")).unwrap();
    let source = r#"
value=outside
{ value=inside; cd child; { printf '%s:' "$value"; false; }; } || printf 'failed:'
printf '%s:' "$value"
pwd
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        &format!(
            "inside:failed:inside:{}\n",
            home.path().join("child").canonicalize().unwrap().display()
        ),
        "",
    );
}

// POSIX.1-2024 2.9.4.1 and 2.13: a subshell inherits the live environment
// and open descriptors, but its changes and exit do not escape the group.
#[test]
fn parenthesized_group_isolates_state_and_exit() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("child")).unwrap();
    let source = r#"
value=outer
send() { printf '%s\n' "$value" >&3; }
run() {
    ( value=inner; cd child; send; printf 'pwd:%s\n' "$(pwd)"; printf 'redirected\n' 3>local-file >&3; exit 17; printf wrong )
    printf 'status:%s value:%s\n' "$?" "$value"
    send
}
run 3>shared
pwd
printf 'captured:%s\n' "$( (printf nested) )"
"#;
    let output = shell(home.path()).args(["-c", source]).output().unwrap();
    let root = home.path().canonicalize().unwrap();
    assert_output(
        output,
        0,
        &format!(
            "pwd:{}/child\nstatus:17 value:outer\n{}\ncaptured:nested\n",
            root.display(),
            root.display()
        ),
        "",
    );
    assert_eq!(fs::read(root.join("shared")).unwrap(), b"inner\nouter\n");
    assert_eq!(
        fs::read(root.join("child/local-file")).unwrap(),
        b"redirected\n"
    );
}

#[test]
fn asynchronous_and_or_lists_short_circuit_and_keep_wait_status() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"
value=parent
false && printf wrong > skipped || { value=child; printf '%s' "$value" > result; (exit 17); } &
first=$!
true || printf wrong > skipped && (exit 9) &
second=$!
wait "$second"
printf '%s:' "$?"
wait "$first"
printf '%s:%s\n' "$?" "$value"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "9:17:parent\n",
        "",
    );
    assert!(!home.path().join("skipped").exists());
    assert_eq!(fs::read(home.path().join("result")).unwrap(), b"child");
}

#[test]
fn case_fallthrough_runs_bodies_without_expanding_their_patterns() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"
case yes in
  no) printf wrong ;&
  yes) printf first ;&
  "$(printf wrong > skipped)") printf ':next'; false ;&
  unused) ;;
  *) printf wrong ;;
esac
printf ':%s\n' "$?"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "first:next:1\n",
        "",
    );
    assert!(!home.path().join("skipped").exists());
}

#[test]
fn arithmetic_command_sets_status_and_updates_live_variables() {
    let home = tempfile::tempdir().unwrap();
    let source = r#"
count=1
(( count -= 1 )) || printf 'zero:%s\n' "$count"
if (( count += 4 )); then printf 'nonzero:%s\n' "$count"; fi
(( 0 && (count = 99) ))
printf 'short:%s value:%s\n' "$?" "$count"
(( 1 / 0 ))
printf 'error:%s value:%s\n' "$?" "$count"
"#;
    assert_output(
        shell(home.path()).args(["-c", source]).output().unwrap(),
        0,
        "zero:0\nnonzero:4\nshort:1 value:4\nerror:1 value:4\n",
        "zledsh: arithmetic command: invalid arithmetic division\n",
    );
}

#[test]
fn errexit_uses_pipeline_status_and_respects_tested_lists() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-ec",
            r#"
f() { false; printf 'function\n'; }
if false; printf 'condition\n'; then :; fi
while false; do printf 'unexpected\n'; done
until true; do printf 'unexpected\n'; done
! true
false && printf 'unexpected\n'
{ false && :; }
f && printf 'tested\n'
( false; printf 'subshell\n' ) && :
( false; printf 'unexpected\n' ) | /bin/cat
printf '<%s>\n' "$(false; printf unexpected)"
set -o pipefail
exit 7 | true
printf 'unexpected\n'
"#,
        ])
        .output()
        .unwrap();
    assert_output(output, 7, "condition\nfunction\ntested\nsubshell\n<>\n", "");
}

#[test]
fn errexit_checks_the_final_and_or_command_and_can_be_disabled() {
    let home = tempfile::tempdir().unwrap();
    for (source, status, stdout) in [
        ("set -e; true && false; printf unexpected", 1, ""),
        ("set -e; (false && :); printf unexpected", 1, ""),
        ("set -e; value=$(exit 9); printf unexpected", 9, ""),
        (
            "set -e; set +o errexit; false; printf continued",
            0,
            "continued",
        ),
    ] {
        let output = shell(home.path()).args(["-c", source]).output().unwrap();
        assert_output(output, status, stdout, "");
    }
}
