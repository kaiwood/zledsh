use std::{fs, process::Command};

// POSIX Shell Command Language 2.7.1 and 2.9.2.
#[test]
fn input_redirect_is_local_to_the_command_and_works_for_builtins_and_children() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("input file"), "file line\nfile tail\n").unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "original stdin\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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

#[test]
fn pipeline_input_can_be_overridden_by_an_explicit_redirect() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("file with spaces"), "from file\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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

#[test]
fn failed_input_redirect_skips_the_command_and_leaves_input_intact() {
    let dir = tempfile::tempdir().unwrap();
    let stdin = dir.path().join("stdin");
    fs::write(&stdin, "still here\n").unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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

// POSIX Shell Command Language 2.7.4.
#[test]
fn here_document_quotes_and_tab_stripping_control_expansion() {
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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
    let script = dir.path().join("large.kwsh");
    let body = "x".repeat(512 * 1024);
    fs::write(&script, format!("/bin/cat <<'EOF'\n{body}\nEOF\n")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
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
    let output = Command::new(env!("CARGO_BIN_EXE_kwsh"))
        .args(["-c", "printf wrong <<EOF\nunterminated\n"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing here-document delimiter"));
}
