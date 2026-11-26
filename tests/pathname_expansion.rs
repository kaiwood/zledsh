use std::{fs, process::Command};

fn assert_paths(files: &[&str], source: &str, expected: &str) {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let work = dir.path().join("work");
    fs::create_dir(&home).unwrap();
    fs::create_dir(&work).unwrap();
    for name in files {
        let path = work.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "file contents\n").unwrap();
    }
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(work)
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .args(["-c", source])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
}

// POSIX.1-2024 Shell Command Language 2.6.6 and 2.14.3.
#[test]
fn basic_patterns_expand_and_unmatched_patterns_are_unchanged() {
    assert_paths(
        &["a space.txt", "b.txt", "c.log"],
        r#"printf '<%s>\n' a*.txt ?.txt *.missing"#,
        "<a space.txt>\n<b.txt>\n<*.missing>\n",
    );
}

#[test]
fn only_unquoted_metacharacters_are_active() {
    assert_paths(
        &["x*1.txt", "x?2.txt", "xy3.txt"],
        r#"pattern='xy?.txt'
printf '<%s>\n' 'x*.txt' x\?.txt x"*"?.txt x\??.txt "$pattern" $pattern $(printf 'xy*.txt')
"#,
        "<x*.txt>\n<x?.txt>\n<x*1.txt>\n<x?2.txt>\n<xy?.txt>\n<xy3.txt>\n<xy3.txt>\n",
    );
}
