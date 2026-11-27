use std::{fs, process::Command};

fn shell(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zledsh"));
    command.env_clear().env("HOME", home).current_dir(home);
    command
}

#[test]
fn positional_arguments_preserve_bytes_across_shell_workers() {
    use std::os::unix::ffi::OsStrExt;

    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"/usr/bin/printf '<%s>' "$1"; ( /usr/bin/printf '<%s>' "$@" ) | /bin/cat"#,
            "name",
        ])
        .arg(std::ffi::OsStr::from_bytes(b"\xff *"))
        .args(["", "café"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"<\xff *><\xff *><><caf\xc3\xa9>");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn non_utf8_read_and_substitution_report_distinct_errors() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("bytes"), b"\xff\nvalid\n").unwrap();
    let output = shell(home.path())
        .args(["-c", r#"read -r first; printf '%s\n' "$?"; read -r next; printf '%s\n' "$next"; result=$(/bin/cat bytes)"#])
        .stdin(fs::File::open(home.path().join("bytes")).unwrap())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"2\nvalid\n");
    assert_eq!(
        output.stderr,
        b"zledsh: read: input is not UTF-8\nzledsh: command substitution output is not UTF-8\n"
    );
}

#[test]
fn pathname_expansion_passes_exact_filename_bytes_in_each_execution_scope() {
    use std::os::unix::ffi::OsStrExt;

    let home = tempfile::tempdir().unwrap();
    // Include a space and a metacharacter to catch re-splitting and re-globbing.
    let name = std::ffi::OsStr::from_bytes(b"f\xff *");
    if let Err(error) = fs::write(home.path().join(name), "") {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EILSEQ) {
            eprintln!("filesystem rejected the non-UTF-8 fixture: {error}");
            return;
        }
        panic!("cannot create non-UTF-8 pathname: {error}");
    }
    fs::write(home.path().join("fa"), "").unwrap();
    fs::create_dir(home.path().join("nested")).unwrap();
    fs::write(home.path().join("nested").join(name), "").unwrap();
    for (source, expected) in [
        (r#"/usr/bin/printf '<%s>' f*"#, b"<fa><f\xff *>".as_slice()),
        (
            r#"/usr/bin/printf '<%s>' f* | /bin/cat"#,
            b"<fa><f\xff *>".as_slice(),
        ),
        (
            r#"( command /usr/bin/printf '<%s>' f* )"#,
            b"<fa><f\xff *>".as_slice(),
        ),
        (
            r#"exec /usr/bin/printf '<%s>' f*"#,
            b"<fa><f\xff *>".as_slice(),
        ),
        (
            r#"set -o globstar; /usr/bin/printf '<%s>' **/f*"#,
            b"<fa><f\xff *><nested/f\xff *>".as_slice(),
        ),
    ] {
        let output = shell(home.path()).args(["-c", source]).output().unwrap();
        assert!(output.status.success(), "{source}: {output:?}");
        assert_eq!(output.stdout, expected, "{source}");
        assert!(output.stderr.is_empty(), "{source}: {output:?}");
    }
}

#[test]
fn locale_selection_does_not_change_current_matching_or_path_order() {
    let home = tempfile::tempdir().unwrap();
    for name in ["z.txt", "a.txt", "é.txt"] {
        fs::write(home.path().join(name), "").unwrap();
    }
    for locale in ["C", "en_US.UTF-8"] {
        let output = shell(home.path())
            .env("LC_ALL", locale)
            .args([
                "-c",
                r#"
case é in [[:alpha:]]) printf 'alpha\n';; *) printf 'not-alpha\n';; esac
printf '%s\n' *.txt
"#,
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, "not-alpha\na.txt\nz.txt\né.txt\n".as_bytes());
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn noninteractive_modes_do_not_load_startup_files() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fs::create_dir(home.join(".kward")).unwrap();
    for file in [".kward/zledshrc", ".zledshrc", "env"] {
        fs::write(home.join(file), "printf unexpected; exit 81\n").unwrap();
    }
    fs::write(home.join("script"), "printf script\n").unwrap();
    for (args, stdout) in [
        (&["-c", "printf command"][..], b"command".as_slice()),
        (&["script"], b"script".as_slice()),
        (&["-s"], b"script".as_slice()),
    ] {
        let output = shell(home)
            .env("ENV", home.join("env"))
            .stdin(fs::File::open(home.join("script")).unwrap())
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, stdout);
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn input_accepts_long_lines_but_reports_descriptor_and_nesting_limits() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    let expected = "x".repeat(128 * 1024);
    fs::write(home.join("long"), format!("printf '%s' '{expected}'\n")).unwrap();
    let output = shell(home).arg("long").output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, expected.as_bytes());
    assert!(output.stderr.is_empty());

    for (source, diagnostic) in [
        (
            "printf unexpected 10> file".into(),
            "only descriptors 0 through 9 are supported",
        ),
        (
            format!("{}: {}", "{ ".repeat(40), "; } ".repeat(40)),
            "compound commands nested too deeply",
        ),
    ] {
        let output = shell(home).args(["-c", &source]).output().unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(diagnostic),
            "{output:?}"
        );
    }
    assert!(!home.join("file").exists());
}

#[test]
fn executable_format_failures_never_fall_back_to_an_interpreter() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    for (name, content) in [
        ("text", b"printf unexpected > marker\n".as_slice()),
        ("binary", b"\0not-an-executable".as_slice()),
        (
            "bad-interpreter",
            b"#!./missing-interpreter\nprintf unexpected > marker\n".as_slice(),
        ),
    ] {
        let path = home.join(name);
        fs::write(&path, content).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    fs::write(home.join("not-executable"), b"printf unexpected > marker\n").unwrap();
    for (name, status, diagnostic) in [
        ("text", 126, "cannot execute"),
        ("binary", 126, "cannot execute"),
        ("bad-interpreter", 126, "cannot execute"),
        ("not-executable", 126, "cannot execute"),
        ("missing", 127, "command not found"),
    ] {
        let output = shell(home)
            .args(["-c", &format!("./{name}")])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(status), "{name}: {output:?}");
        assert!(output.stdout.is_empty());
        assert_eq!(
            output.stderr,
            format!("zledsh: ./{name}: {diagnostic}\n").as_bytes()
        );
    }
    assert!(!home.join("marker").exists());
    let output = shell(home).arg("text").output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty() && output.stderr.is_empty());
    assert_eq!(fs::read(home.join("marker")).unwrap(), b"unexpected");
}
