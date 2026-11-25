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
