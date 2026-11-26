use std::{fs, process::Command};

fn shell(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_zledsh"));
    command.env_clear().env("HOME", home).current_dir(home);
    command
}

// These fixtures record current representation limits, not required behavior.
#[test]
fn non_utf8_argument_is_rejected_before_execution() {
    use std::os::unix::ffi::OsStrExt;

    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .args(["-c", "printf unexpected", "name"])
        .arg(std::ffi::OsStr::from_bytes(b"\xff"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(
        output.stderr,
        b"zledsh: positional arguments must be UTF-8\n"
    );
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

#[cfg(target_os = "linux")]
#[test]
fn pathname_expansion_rejects_non_utf8_matches() {
    use std::os::unix::ffi::OsStrExt;

    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(std::ffi::OsStr::from_bytes(b"f\xff")), "").unwrap();
    let output = shell(home.path())
        .args(["-c", "printf '%s' f*"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert_eq!(output.stderr, b"zledsh: pathname match is not UTF-8\n");
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
