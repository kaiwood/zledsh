use std::{
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, Stdio},
};

fn shell() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kwsh"))
}

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "kwsh-external-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str, script: &str, mode: u32) {
        let path = self.0.join(name);
        fs::write(&path, script).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn path_lookup_preserves_arguments_exports_redirections_and_status() {
    let scratch = Scratch::new();
    scratch.file("probe", "#!/bin/sh\nprintf '<%s>\\n' \"$@\" \"$KWSH_EXPORT\"\nprintf 'diagnostic\\n' >&2\nexit 17\n", 0o700);
    let output = shell()
        .current_dir(&scratch.0)
        .args([
            "-c",
            "PATH=.; export KWSH_EXPORT=visible; probe \"$@\" > result 2>&1; exit",
            "name",
            "a b",
            "",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(17));
    assert!(output.stdout.is_empty());
    assert!(output.stderr.is_empty());
    assert_eq!(
        fs::read(scratch.0.join("result")).unwrap(),
        b"<a b>\n<>\n<visible>\ndiagnostic\n"
    );
}

#[test]
fn distinguishes_missing_and_unexecutable_without_shell_fallback() {
    let scratch = Scratch::new();
    scratch.file("no-shebang", "printf executed\\n\n", 0o700);
    scratch.file("denied", "#!/bin/sh\nexit 0\n", 0o600);
    for (program, code) in [("missing", 127), ("no-shebang", 126), ("denied", 126)] {
        let output = shell()
            .current_dir(&scratch.0)
            .args(["-c", &format!("PATH=.; {program}")])
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(code),
            "{program}: {:?}",
            output.stderr
        );
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn external_program_can_read_the_next_line_of_a_pipe() {
    let scratch = Scratch::new();
    scratch.file(
        "reader",
        "#!/bin/sh\nread -r value\nprintf '<%s>\\n' \"$value\"\n",
        0o700,
    );
    let mut child = shell()
        .current_dir(&scratch.0)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"./reader\nnot shell syntax |\nprintf 'after\\n'\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"<not shell syntax |>\nafter\n");
}
