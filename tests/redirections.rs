use std::{fs, process::Command};

// POSIX.1-2024 Shell Command Language 2.7.1 (descriptor 0), 2.9.1 and 2.9.5.
// https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html#tag_19_07_01
#[test]
fn input_redirect_is_local_to_the_command_and_works_for_builtins_and_children() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input file"), "file line\nfile tail\n").unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "original stdin\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .stdin(fs::File::open(stdin).unwrap())
        .args([
            "-c",
            r#"name='input file'
consume() { read -r line; /bin/cat; }
consume 0<"$name"
printf '<%s>\n' "$line"
/bin/cat
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"file tail\n<file line>\noriginal stdin\n");
    assert!(output.stderr.is_empty());
}

// Sections 2.7 and 2.9.2: no field splitting on the operand; redirects override pipes.
#[test]
fn pipeline_input_can_be_overridden_by_an_explicit_redirect() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file with spaces"), "from file\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            "printf 'from pipe\\n' 2> producer-errors | /bin/cat <$(printf 'file with spaces') | /bin/cat",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"from file\n");
    assert!(output.stderr.is_empty());
}

// Sections 2.7 and 2.8.1: an open failure prevents the utility from running.
#[test]
fn failed_input_redirect_skips_the_command_and_leaves_input_intact() {
    let dir = tempfile::tempdir().unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "still here\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .stdin(fs::File::open(stdin).unwrap())
        .args([
            "-c",
            "printf wrong < missing; printf 'status=%s\\n' \"$?\"; /bin/cat",
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"status=1\nstill here\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing"));
}

// POSIX Shell Command Language 2.9.1: expand ordinary arguments before redirection.
#[test]
fn argument_substitution_reads_output_file_before_redirection_truncates_it() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("data");
    for suffix in ["", " | /bin/cat"] {
        fs::write(&file, b"original\n").unwrap();
        let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
            .current_dir(dir.path())
            .env("HOME", dir.path())
            .args([
                "-c",
                &format!(r#"printf '%s\n' "$(/bin/cat data)" >data{suffix}"#),
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert!(output.stdout.is_empty(), "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
        assert_eq!(fs::read(&file).unwrap(), b"original\n", "suffix {suffix:?}");
    }
}

// POSIX Shell Command Language 2.7.2 and 2.7.3.
#[test]
fn append_preserves_existing_output_and_force_write_replaces_it() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("log"), "first\n").unwrap();
    fs::write(dir.path().join("replace"), "old longer contents\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#"printf 'second\n' >>log
printf 'third\n' 2>>log 1>&2
printf 'created\n' >>new-log
printf 'replacement\n' >|replace
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(dir.path().join("log")).unwrap(),
        b"first\nsecond\nthird\n"
    );
    assert_eq!(fs::read(dir.path().join("new-log")).unwrap(), b"created\n");
    assert_eq!(
        fs::read(dir.path().join("replace")).unwrap(),
        b"replacement\n"
    );
}

// POSIX Shell Command Language 2.7.7.
#[test]
fn read_write_redirection_creates_files_without_truncating_existing_files() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("data"), "head\ntail\n").unwrap();
    fs::write(dir.path().join("output"), "old suffix\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#"edit() { read -r head; /bin/sh -c 'printf "new!" >&0'; }
edit <>data
printf '<%s>\n' "$head"
printf '%s' new 1<>output
: <>created
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"<head>\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(dir.path().join("data")).unwrap(), b"head\nnew!\n");
    assert_eq!(
        fs::read(dir.path().join("output")).unwrap(),
        b"new suffix\n"
    );
    assert!(fs::read(dir.path().join("created")).unwrap().is_empty());
}

// POSIX Shell Command Language 2.7.4.
#[test]
fn here_document_quotes_and_tab_stripping_control_expansion() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args([
            "-c",
            concat!(
                "name=world\n",
                "/bin/cat <<-END\n",
                "\thello \"$name\" $(printf '%s' 'two words') \\$name \\\\\n",
                "\tjoined \\\n",
                "line\n",
                "\tEND\n",
                "/bin/cat <<'LIT'\n",
                "$name $(printf ignored) \\\n",
                "LIT\n",
                "/bin/cat <<$marker\n",
                "unexpanded delimiter\n",
                "$marker",
            ),
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        output.stdout,
        b"hello \"world\" two words $name \\\njoined line\n$name $(printf ignored) \\\nunexpanded delimiter\n"
    );
    assert!(output.stderr.is_empty());
}

#[test]
fn here_document_bodies_are_read_in_order_but_only_expanded_if_executed() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#"false && /bin/cat <<SKIP
$(printf wrong > unexpected)
SKIP
read -r value <<FIRST <<SECOND; /bin/cat <<THIRD
first
FIRST
second
SECOND
third
THIRD
printf '<%s>\n' "$value"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"third\n<second>\n");
    assert!(output.stderr.is_empty());
    assert!(!dir.path().join("unexpected").exists());
}

#[test]
fn nested_substitution_reads_its_own_here_documents() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args([
            "-c",
            r#"name=inside
/bin/cat <<OUTER | /bin/cat
$(/bin/cat <<INNER
$name
INNER
)
OUTER
<<FIRST /bin/cat"$(printf '' <<INNER
ignored
INNER
)" <<SECOND
first
FIRST
second
SECOND
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"inside\nsecond\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn here_document_larger_than_a_pipe_buffer_is_delivered_in_full() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("large.zledsh");
    let body = "x".repeat(512 * 1024);
    fs::write(&script, format!("/bin/cat <<'EOF'\n{body}\nEOF\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .arg(script)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout.len(), body.len() + 1);
    assert!(output.stdout.starts_with(body.as_bytes()));
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert!(output.stderr.is_empty());
}

#[test]
fn unterminated_here_document_does_not_execute_its_command() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args(["-c", "printf wrong <<EOF\nunterminated\n"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing here-document delimiter"));
}

// POSIX Shell Command Language 2.7.5–2.7.6: each duplication snapshots its source.
#[test]
fn descriptor_duplication_obeys_redirection_order_and_uses_the_pipeline() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#"/bin/sh -c 'printf out; printf err >&2' >both 2>&1
/bin/sh -c 'printf out; printf err >&2' 2>&1 >only-out
/bin/sh -c 'printf out; printf err >&2' 2>&1 >pipeline-out | /bin/cat
printf '%s' built-in 3>via-three 1>&3 3>&-
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"errerr");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(dir.path().join("both")).unwrap(), b"outerr");
    assert_eq!(fs::read(dir.path().join("only-out")).unwrap(), b"out");
    assert_eq!(fs::read(dir.path().join("pipeline-out")).unwrap(), b"out");
    assert_eq!(fs::read(dir.path().join("via-three")).unwrap(), b"built-in");
}

#[test]
fn a_function_shares_open_descriptors_with_builtins_and_children() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input"), "first\nrest\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args([
            "-c",
            r#"consume() { read -r first 0<&3; /bin/cat <&3; printf '<%s>\n' "$first" >&9; }
consume 3<input 9>result
printf 'after\n'
/bin/cat 8<<'END' <&8
via heredoc
END
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"rest\nafter\nvia heredoc\n");
    assert!(output.stderr.is_empty());
    assert_eq!(fs::read(dir.path().join("result")).unwrap(), b"<first>\n");
}

#[test]
fn closing_an_inherited_descriptor_is_local_to_one_command() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new("/bin/sh")
        .current_dir(dir.path())
        .args([
            "-c",
            "exec \"$1\" -c \"$2\" 9>inherited",
            "fixture",
            env!("CARGO_BIN_EXE_zledsh"),
            r#"printf before >&9
/bin/sh -c 'if ( : >&9 ) 2>/dev/null; then exit 77; fi' 9>&-
printf 'child=%s\n' "$?"
printf wrong 9>&- >&9
printf 'bad-dup=%s\n' "$?"
printf after >&9
: 9>&- 9>&-
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"child=0\nbad-dup=1\n");
    assert!(String::from_utf8_lossy(&output.stderr).contains("9: bad file descriptor"));
    assert_eq!(
        fs::read(dir.path().join("inherited")).unwrap(),
        b"beforeafter"
    );
}

#[test]
fn a_builtin_with_closed_standard_descriptors_fails_without_leaking_the_close() {
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .args([
            "-c",
            r#"true | printf wrong >&-
printf 'write=%s\n' "$?"
read -r value <&-
printf 'read=%s\n' "$?"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"write=1\nread=1\n");
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("zledsh: printf:"));
    assert!(errors.contains("zledsh: read:"));
}

#[test]
fn shell_private_descriptors_are_not_available_as_duplication_sources() {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("private.zledsh");
    fs::write(
        &script,
        "printf wrong 9>first-out >&3\nprintf 'first=%s\\n' \"$?\"\nprintf wrong >&9\nprintf 'second=%s\\n' \"$?\"\n",
    )
    .unwrap();
    let output = Command::new("/bin/sh")
        .current_dir(dir.path())
        .args([
            "-c",
            "exec \"$1\" \"$2\" 3>&- 4>&- 5>&- 6>&- 7>&- 8>&- 9>&-",
            "fixture",
            env!("CARGO_BIN_EXE_zledsh"),
        ])
        .arg(script)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"first=1\nsecond=1\n");
    assert!(fs::read(dir.path().join("first-out")).unwrap().is_empty());
    let errors = String::from_utf8_lossy(&output.stderr);
    assert!(errors.contains("3: bad file descriptor"));
    assert!(errors.contains("9: bad file descriptor"));
}

#[test]
fn compound_and_function_redirects_are_ordered_and_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let source = r#"
path=initial
f() { printf 'function\n'; printf 'diagnostic\n' >&2; } >"$path" 2>&1
path=called
f
{ printf 'group\n'; printf 'group-err\n' >&2; } 2>&1 >group
( printf 'subshell\n' ) >sub
{ printf wrong; } >partial <missing || printf 'failed\n'
f 3>&1 <missing || printf 'failed call\n'
printf 'restored\n'
"#;
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .args(["-c", source])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"group-err\nfailed\nfailed call\nrestored\n");
    assert_eq!(
        String::from_utf8_lossy(&output.stderr)
            .matches("missing")
            .count(),
        2
    );
    assert!(!dir.path().join("initial").exists());
    for (file, expected) in [
        ("called", "function\ndiagnostic\n"),
        ("group", "group\n"),
        ("sub", "subshell\n"),
        ("partial", ""),
    ] {
        assert_eq!(fs::read_to_string(dir.path().join(file)).unwrap(), expected);
    }
}

#[test]
fn here_strings_preserve_the_value_and_follow_redirection_order() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("glob.txt"), b"wrong").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(dir.path())
        .env_clear()
        .env("HOME", dir.path())
        .env("PATH", "/usr/bin:/bin")
        .args([
            "-c",
            r#"
value='two words *.txt'
/bin/cat <<<$value
true | /bin/cat <<<"$(printf 'pipeline\n')"
/bin/cat 3<<<'fd three' <&3
read -r result <<<"$value"
printf '<%s>\n' "$result"
/bin/cat <<<''
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"two words *.txt\npipeline\nfd three\n<two words *.txt>\n\n"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
}
