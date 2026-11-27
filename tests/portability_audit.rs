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
fn read_and_substitution_preserve_non_utf8_values() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("bytes"), b"\xff:part two\nvalid\n").unwrap();
    fs::write(home.path().join("capture"), b"one \xfe\n\n").unwrap();
    let output = shell(home.path())
        .args(["-c", r#"IFS=: read -r first rest; read -r next; /usr/bin/printf '<%s>' "$first" "$rest" "$next"; result=$(/bin/cat capture); /usr/bin/printf '<%s>' "$result" $result"#])
        .stdin(fs::File::open(home.path().join("bytes")).unwrap())
        .output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        output.stdout,
        b"<\xff><part two><valid><one \xfe><one><\xfe>"
    );
    assert!(output.stderr.is_empty(), "{output:?}");
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

fn installed_utf8_locale() -> Option<&'static str> {
    ["en_US.UTF-8", "C.UTF-8"].into_iter().find(|name| {
        let name = std::ffi::CString::new(*name).unwrap();
        let locale =
            unsafe { libc::newlocale(libc::LC_CTYPE_MASK, name.as_ptr(), std::ptr::null_mut()) };
        if locale.is_null() {
            return false;
        }
        unsafe { libc::freelocale(locale) };
        true
    })
}

#[test]
fn locale_selects_character_matching_with_lc_all_precedence() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("é.txt"), "").unwrap();
    let source = r#"
case é in [[:alpha:]]) printf 'alpha\n';; *) printf 'not-alpha\n';; esac
printf '%s\n' ?.txt
word=étail
printf '%s\n' "${word#?}"
"#;
    for (all, expected) in [
        ("C", "not-alpha\n?.txt\nétail\n"),
        ("", "alpha\né.txt\ntail\n"),
    ] {
        let locale = installed_utf8_locale();
        if all.is_empty() && locale.is_none() {
            eprintln!("no supported UTF-8 locale installed");
            continue;
        }
        let output = shell(home.path())
            .env("LC_ALL", all)
            .env("LC_CTYPE", locale.unwrap_or("C"))
            .args(["-c", source])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected.as_bytes(), "LC_ALL={all:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }
}

#[test]
fn pathname_order_follows_collation_category() {
    use std::io::Write;
    use std::process::Stdio;
    let home = tempfile::tempdir().unwrap();
    let names = "z.txt\na.txt\né.txt\n";
    for name in names.lines() {
        fs::write(home.path().join(name), "").unwrap();
    }
    for locale in [Some("C"), installed_utf8_locale()] {
        let Some(locale) = locale else {
            eprintln!("no supported non-C collation fixture");
            continue;
        };
        let mut sort = Command::new("/usr/bin/sort")
            .env_clear()
            .env("LC_COLLATE", locale)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        sort.stdin
            .take()
            .unwrap()
            .write_all(names.as_bytes())
            .unwrap();
        let expected = sort.wait_with_output().unwrap();
        assert!(expected.status.success());
        let output = shell(home.path())
            .env("LC_COLLATE", locale)
            .args(["-c", "printf '%s\\n' *.txt"])
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, expected.stdout, "{locale}");
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

#[test]
fn scalar_assignments_and_exports_preserve_native_values() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .env("INPUT", std::ffi::OsStr::from_bytes(b"a\xff b"))
        .args(["-c", r#"first=$INPUT; export SECOND="$first"; ( /usr/bin/printenv SECOND ); /usr/bin/printf '<%s>' "$first""#])
        .output().unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"a\xff b\n<a\xff b>");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn redirects_use_native_path_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let path = std::ffi::OsStr::from_bytes(b"file\xff");
    if let Err(error) = fs::write(home.path().join(path), "seed") {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EILSEQ) {
            eprintln!("filesystem rejected non-UTF-8 filename: {error}");
            return;
        }
        panic!("{error}");
    }
    fs::write(home.path().join("names"), b"file\xff\n").unwrap();
    let output = shell(home.path())
        .args([
            "-c",
            r#"
read -r path < names
/bin/cat < "$path"
printf append >> "$path"
/bin/cat <> "$path"
printf replace > "$path"
set -C
printf force >| "$path"
/bin/cat < "$path"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"seedseedappendforce");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(fs::read(home.path().join(path)).unwrap(), b"force");
}

#[test]
fn here_inputs_preserve_expanded_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let result = shell(home.path())
        .env("VALUE", std::ffi::OsStr::from_bytes(b"a\xff b"))
        .args([
            "-c",
            "/bin/cat <<EOF\n<$VALUE>\nEOF\n/bin/cat <<< \"$VALUE\"\n",
        ])
        .output()
        .unwrap();
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"<a\xff b>\na\xff b\n");
    assert!(result.stderr.is_empty(), "{result:?}");
}

#[test]
fn directory_builtins_keep_native_path_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let base = home.path().canonicalize().unwrap();
    let child = base.join(std::ffi::OsStr::from_bytes(b"dir\xff"));
    if let Err(error) = fs::create_dir(&child) {
        #[cfg(target_os = "macos")]
        if error.raw_os_error() == Some(libc::EILSEQ) {
            eprintln!("filesystem rejected non-UTF-8 directory: {error}");
            return;
        }
        panic!("{error}");
    }
    let output = shell(&base)
        .env("DEST", &child)
        .args([
            "-c",
            r#"
cd -L "$DEST" && pwd -L && pwd -P && /usr/bin/printenv PWD && cd - && /usr/bin/printenv OLDPWD
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let mut expected = Vec::new();
    for path in [&child, &child, &child, &base, &child] {
        expected.extend_from_slice(path.as_os_str().as_bytes());
        expected.push(b'\n');
    }
    assert_eq!(output.stdout, expected);
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn function_calls_and_set_forward_byte_positionals() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .env("VALUE", std::ffi::OsStr::from_bytes(b"v\xff"))
        .args([
            "-c",
            r#"
f() { /usr/bin/printf '<%s>' "$@"; set -- "$2"; /usr/bin/printf '<%s>' "$1"; }
set -- "$VALUE" ''
f "$@"
/usr/bin/printf '<%s>' "$@"
"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"<v\xff><><><v\xff><>");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn for_loops_keep_explicit_and_implicit_byte_values() {
    use std::os::unix::ffi::OsStrExt;
    let home = tempfile::tempdir().unwrap();
    let output = shell(home.path())
        .env("VALUE", std::ffi::OsStr::from_bytes(b"v\xff *"))
        .args([
            "-c",
            r#"for value in "$VALUE" ''; do /usr/bin/printf '<%s>' "$value"; done
set -- "$VALUE" ''
for value; do /usr/bin/printf '<%s>' "$value"; done"#,
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, b"<v\xff *><><v\xff *><>");
    assert!(output.stderr.is_empty(), "{output:?}");
}
