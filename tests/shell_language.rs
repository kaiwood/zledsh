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

#[test]
fn readonly_preserves_values_across_assignment_unset_and_export() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
value="two words'"
readonly fixed=$value missing
readonly -p
( fixed=changed; printf unexpected ) || printf 'assignment blocked\n'
true | unset fixed
printf 'worker status=%s\n' "$?"
command export fixed=changed || printf 'export assignment blocked\n'
export fixed
/usr/bin/printenv fixed
command unset fixed || printf 'unset blocked\n'
printf '<%s>\n' "$fixed" "${missing-unset}"
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "readonly fixed='two words'\\'''\nreadonly missing\nassignment blocked\nworker status=2\nexport assignment blocked\ntwo words'\nunset blocked\n<two words'>\n<unset>\n",
        "zledsh: fixed: read-only variable\nzledsh: unset: fixed: read-only variable\nzledsh: export: fixed: read-only variable\nzledsh: unset: fixed: read-only variable\n",
    );
}

#[test]
fn readonly_rejects_indirect_writes_and_does_not_leak_a_function_local() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
x=outer
f() { local x=inner; readonly x; printf '%s\n' "$x"; }
f
x=2
readonly x y
((x=3, changed=1)) || printf 'arithmetic blocked\n'
( printf '%s' "${y:=value}" ) || printf 'parameter blocked\n'
true | read x || printf 'read blocked\n'
printf '%s:%s\n' "$x" "${changed-unset}"
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "inner\narithmetic blocked\nparameter blocked\nread blocked\n2:unset\n",
        "zledsh: arithmetic command: assignment to a read-only variable\nzledsh: y: read-only variable\nzledsh: read: x: read-only variable\n",
    );
}

#[test]
fn assignment_prefixes_have_command_local_scope_except_for_special_builtins() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
x=outer
prefix='two words *'
x=$prefix y=$x /usr/bin/printenv y
x=$prefix /usr/bin/printenv x
printf '%s\n' "$x"
HOME=missing-prefix-directory cd || printf 'cd failed\n'
f() { printf 'function=%s\n' "$x"; x=changed; y=retained; }
x=inner f
printf 'after=%s:%s\n' "$x" "$y"
x=pipe /usr/bin/printenv x | /bin/cat
x=persistent :
x=temporary command :
printf 'final=%s\n' "$x"
"#,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"two words *\ntwo words *\nouter\ncd failed\nfunction=inner\nafter=outer:retained\npipe\nfinal=persistent\n");
    let error = String::from_utf8(output.stderr).unwrap();
    assert!(
        error.starts_with("zledsh: cd: missing-prefix-directory:"),
        "{error}"
    );
    assert_eq!(error.lines().count(), 1);
}

#[test]
fn assignment_prefixes_apply_after_arguments_and_redirections() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
name=before
name=after printf '%s\n' "$name" > "$name"
/bin/cat before
name=retained $missing
printf '%s\n' "$name"
readonly fixed=old
( fixed=new /usr/bin/printenv fixed; printf unexpected ) || printf 'blocked\n'
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "before\nretained\nblocked\n",
        "zledsh: fixed: read-only variable\n",
    );
    assert!(!home.path().join("after").exists());
}

#[test]
fn direct_special_builtin_errors_exit_noninteractive_but_command_errors_do_not() {
    let home = tempfile::tempdir().unwrap();
    let error = "zledsh: export: expected a variable name or assignment\n";
    let direct = shell(home.path())
        .args([
            "-c",
            "export bad-name || printf unexpected; printf also-unexpected",
        ])
        .output()
        .unwrap();
    assert_output(direct, 2, "", error);
    let via_command = shell(home.path())
        .args([
            "-c",
            "command export bad-name || printf recovered; eval false; printf ':%s' \"$?\"",
        ])
        .output()
        .unwrap();
    assert_output(via_command, 0, "recovered:1", error);
    let interactive = shell(home.path())
        .args(["-ic", "export bad-name || printf recovered"])
        .output()
        .unwrap();
    assert_output(interactive, 0, "recovered", error);
}

#[test]
fn special_builtins_take_precedence_and_failed_redirection_prevents_assignment() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
export() { printf unexpected; }
export x=exported
/usr/bin/printenv x
( x=changed : > missing/file; printf unexpected ) || printf 'blocked\n'
command : > missing/file || printf 'recovered\n'
printf '%s\n' "$x"
"#,
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stdout, b"exported\nblocked\nrecovered\nexported\n");
    let error = String::from_utf8(output.stderr).unwrap();
    assert_eq!(error.lines().count(), 2, "{error}");
    assert!(
        error
            .lines()
            .all(|line| line.starts_with("zledsh: missing/file:")),
        "{error}"
    );
}

#[test]
fn getopts_tracks_clusters_arguments_boundaries_and_reset() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .env("OPTIND", "77")
        .args([
            "-c",
            r#"
printf 'initial=%s\n' "$OPTIND"
while getopts ab: opt; do printf '%s:%s\n' "$opt" "${OPTARG-unset}"; done
printf 'end=%s:%s\n' "$opt" "$OPTIND"
shift "$((OPTIND - 1))"
printf 'remaining=%s\n' "$1"
OPTIND=1
getopts ab: opt -abnext
OPTIND=1
getopts ab: opt -b 'two words'
printf '%s:%s:%s\n' "$opt" "$OPTARG" "$OPTIND"
/usr/bin/printenv OPTIND || printf 'not exported\n'
"#,
            "demo",
            "-abvalue",
            "--",
            "operand",
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "initial=1\na:unset\nb:value\nend=?:3\nremaining=operand\nb:two words:3\nnot exported\n",
        "",
    );
}

#[test]
fn getopts_reports_missing_and_unknown_options_in_both_modes() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
getopts ':b:' opt -xb
printf '%s:%s:%s\n' "$?" "$opt" "$OPTARG"
getopts ':b:' opt -xb
printf '%s:%s:%s\n' "$?" "$opt" "$OPTARG"
OPTIND=1
getopts 'b:' opt -x -b
printf '%s:%s:%s\n' "$?" "$opt" "${OPTARG-unset}"
getopts 'b:' opt -x -b
printf '%s:%s:%s\n' "$?" "$opt" "${OPTARG-unset}"
readonly opt
getopts b opt -b
printf 'status=%s\n' "$?"
"#,
            "demo",
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "0:?:x\n0:::b\n0:?:unset\n0:?:unset\nstatus=2\n",
        "zledsh: demo: getopts: unknown option: -x\nzledsh: demo: getopts: option -b requires an argument\nzledsh: demo: getopts: opt: read-only variable\n",
    );
}

#[test]
fn umask_applies_to_builtins_and_children_without_leaking_from_subshells() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
umask 077
umask
umask -S
: > private
( umask u=rwx,g=rx,o=; /usr/bin/touch shared )
true | umask 000
umask 089 || printf 'invalid\n'
/usr/bin/touch still-private
umask a=rx,ug+w
umask
: > group-writable
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "0077\nu=rwx,g=,o=\ninvalid\n0002\n",
        "zledsh: umask: invalid mask\n",
    );
    for (path, mode) in [
        ("private", 0o600),
        ("shared", 0o640),
        ("still-private", 0o600),
        ("group-writable", 0o664),
    ] {
        assert_eq!(
            fs::metadata(home.path().join(path))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            mode,
            "{path}"
        );
    }
}

#[test]
fn exec_replacement_probe() {
    if std::env::var_os("ZLEDSH_EXEC_PROBE").is_some() {
        println!("exec-pid={}", std::process::id());
        println!("exec-value={}", std::env::var("probe_value").unwrap());
    }
}

#[test]
fn exec_replaces_the_same_process_with_redirected_descriptors() {
    let home = tempfile::tempdir().unwrap();
    let probe = std::env::current_exe().unwrap();
    let child = shell(home.path()).env("ZLEDSH_EXEC_PROBE", "yes")
        .args(["-c", r#"probe_value=inherited exec "$1" exec_replacement_probe --exact --nocapture > capture; printf unexpected"#, "name"])
        .arg(probe).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let pid = child.id();
    assert_output(child.wait_with_output().unwrap(), 0, "", "");
    let capture = fs::read_to_string(home.path().join("capture")).unwrap();
    assert!(
        capture
            .lines()
            .any(|line| line == format!("exec-pid={pid}")),
        "{capture}"
    );
    assert!(
        capture.lines().any(|line| line == "exec-value=inherited"),
        "{capture}"
    );
}

#[test]
fn exec_failure_terminates_only_its_noninteractive_environment() {
    let home = tempfile::tempdir().unwrap();
    let direct = shell(home.path())
        .args(["-c", "exec ./missing; printf unexpected"])
        .output()
        .unwrap();
    assert_output(
        direct,
        127,
        "",
        "zledsh: exec: ./missing: command not found\n",
    );
    let nested = shell(home.path())
        .args([
            "-c",
            "(exec ./missing; printf unexpected) || printf 'status=%s' \"$?\"",
        ])
        .output()
        .unwrap();
    assert_output(
        nested,
        0,
        "status=127",
        "zledsh: exec: ./missing: command not found\n",
    );
}

#[test]
fn cd_and_pwd_distinguish_logical_and_physical_directories() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("real/child")).unwrap();
    std::os::unix::fs::symlink("real/child", root.join("link")).unwrap();
    let output = shell(&root)
        .args([
            "-c",
            r#"
cd link
pwd
pwd -P
cd ..
pwd
cd -
cd -P ..
pwd -L
printf '%s\n' "$OLDPWD"
"#,
        ])
        .output()
        .unwrap();
    let expected = ["link", "real/child", "", "link", "real", "link"]
        .into_iter()
        .map(|path| {
            format!(
                "{}\n",
                if path.is_empty() {
                    root.clone()
                } else {
                    root.join(path)
                }
                .display()
            )
        })
        .collect::<String>();
    assert_output(output, 0, &expected, "");
}

#[test]
fn cdpath_searches_in_order_and_failed_changes_leave_the_directory_intact() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().canonicalize().unwrap();
    fs::create_dir_all(root.join("search/target")).unwrap();
    fs::create_dir(root.join("target")).unwrap();
    fs::write(root.join("not-dir"), "").unwrap();
    let output = shell(&root)
        .args([
            "-c",
            r#"
CDPATH=missing:search:
cd target
cd "$HOME"
cd ./target
pwd
cd "$HOME"
cd not-dir/.. || printf 'blocked\n'
cd '' || printf 'empty\n'
pwd
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{}\n{}\nblocked\nempty\n{}\n",
            root.join("search/target").display(),
            root.join("target").display(),
            root.display()
        )
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 2, "{stderr}");
    assert!(stderr.starts_with("zledsh: cd: not-dir/..:"), "{stderr}");
    assert!(
        stderr.ends_with("zledsh: cd: empty directory name\n"),
        "{stderr}"
    );
}

#[test]
fn exit_traps_run_in_the_final_environment_and_reset_in_workers() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
trap 'printf "outer:%s:%s\n" "$?" "$value"' EXIT
value=live
( exit 3 ); printf 'subshell:%s\n' "$?"
( trap 'printf "inner:%s\n" "$?"' 0; exit 4 ); printf 'subshell:%s\n' "$?"
printf 'capture=<%s>\n' "$(trap 'printf "capture:%s" "$?"' EXIT; exit 6)"
exit 7
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        7,
        "subshell:3\ninner:4\nsubshell:4\ncapture=<capture:6>\nouter:7:live\n",
        "",
    );
}

#[test]
fn trap_listing_is_reusable_and_reset_restores_default() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
trap 'printf "%s" "a'\''b"' EXIT
saved=$(trap)
trap 0
trap -p EXIT
eval "$saved"
"#,
        ])
        .output()
        .unwrap();
    assert_output(output, 0, "trap -- '-' EXIT\na'b", "");
}

#[test]
fn trap_signal_probe() {
    if let Ok(mode) = std::env::var("ZLEDSH_TRAP_TEST") {
        if mode == "ignored" {
            unsafe { libc::raise(libc::SIGUSR1) };
            return;
        }
        if mode == "wait" {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
        assert_eq!(unsafe { libc::kill(libc::getppid(), libc::SIGUSR1) }, 0);
        if mode == "wait" {
            std::thread::sleep(std::time::Duration::from_millis(500));
        } else {
            println!("foreground finished");
        }
    }
}

#[test]
fn trapped_signal_runs_after_foreground_completion_and_interrupts_wait() {
    let home = tempfile::tempdir().unwrap();
    let probe = std::env::current_exe().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
trap 'printf "signal:%s\n" "$?"; false' USR1
export ZLEDSH_TRAP_TEST=foreground
"$1" trap_signal_probe --exact --nocapture
printf 'after:%s\n' "$?"
export ZLEDSH_TRAP_TEST=wait
"$1" trap_signal_probe --exact > probe-output &
pid=$!
wait "$pid"
printf 'wait:%s\n' "$?"
wait "$pid"
printf 'reaped:%s\n' "$?"
"#,
            "name",
        ])
        .arg(probe)
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let interrupted = 128 + libc::SIGUSR1;
    assert!(
        stdout.ends_with(&format!(
            "signal:0\nafter:0\nsignal:{interrupted}\nwait:{interrupted}\nreaped:0\n"
        )),
        "{stdout}"
    );
    assert!(
        stdout.find("foreground finished").unwrap() < stdout.find("signal:0").unwrap(),
        "{stdout}"
    );
}

#[test]
fn ignored_signals_survive_subshells_and_cannot_be_reset_on_noninteractive_entry() {
    use std::os::unix::process::CommandExt;

    let home = tempfile::tempdir().unwrap();
    let mut command = shell(home.path());
    // Set an inherited disposition in the child only.
    unsafe {
        command.pre_exec(|| {
            if libc::signal(libc::SIGUSR1, libc::SIG_IGN) == libc::SIG_ERR {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command
        .env("ZLEDSH_TRAP_TEST", "ignored")
        .args([
            "-c",
            r#"
trap 'printf unexpected' USR1
trap - USR1
trap -p USR1
( "$1" trap_signal_probe --exact > probe-output )
printf 'probe:%s\n' "$?"
"#,
            "name",
        ])
        .arg(std::env::current_exe().unwrap())
        .output()
        .unwrap();
    assert_output(output, 0, "trap -- '' USR1\nprobe:0\n", "");
}

#[test]
fn exit_trap_runs_on_stdin_eof_and_named_script_errors() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("input");
    fs::write(
        &script,
        b"trap 'printf \"exit:%s\\n\" \"$?\"' EXIT\nfalse\n",
    )
    .unwrap();
    let output = shell(home.path())
        .stdin(fs::File::open(&script).unwrap())
        .output()
        .unwrap();
    assert_output(output, 1, "exit:1\n", "");
    fs::write(
        &script,
        b"trap 'printf \"exit:%s\\n\" \"$?\"' EXIT\nif true\n",
    )
    .unwrap();
    let output = shell(home.path()).arg(&script).output().unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert_eq!(output.stdout, b"exit:2\n");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("input:"),
        "{output:?}"
    );
}

#[test]
fn wait_collects_each_operand_and_returns_the_last_status() {
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
exit 17 & first=$!
exit 23 & second=$!
wait "$first" 0 "$second"
printf 'last=%s\n' "$?"
wait "$first" "$second"
printf 'consumed=%s\n' "$?"
exit 31 & third=$!
wait
printf 'all=%s\n' "$?"
wait "$third"
printf 'discarded=%s\n' "$?"
"#,
        ])
        .output()
        .unwrap();
    assert_output(
        output,
        0,
        "last=23\nconsumed=127\nall=0\ndiscarded=127\n",
        "",
    );
}

#[test]
fn wait_for_stopped_job_collects_exit_after_external_continue() {
    let home = tempfile::tempdir().unwrap();
    let worker = home.path().join("stop");
    fs::write(&worker, "kill -STOP \"$$\"\nexit 19\n").unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
/bin/sh ./stop & pid=$!
# Wait until the foreground job table has observed the stop before resuming.
i=0
while [ "$i" -lt 50 ]; do
    jobs %1 > status
    /usr/bin/grep -q Stopped status && break
    i=$((i + 1))
    /bin/sleep 0.02
done
if [ "$i" -eq 50 ]; then /bin/kill -KILL "$pid"; exit 2; fi
(/bin/sleep 0.2; /bin/kill -CONT "$pid") &
wait "$pid"
printf 'exit=%s\n' "$?"
wait
"#,
        ])
        .output()
        .unwrap();
    assert_output(output, 0, "exit=19\n", "");
}
