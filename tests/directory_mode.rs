use std::{fs, process::Command};

#[test]
fn directory_changes_are_live_and_failed_changes_preserve_the_previous_directory() {
    let root = std::env::temp_dir().join(format!("zledsh-directory-{}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let home = root.join("home with spaces");
    let nested = home.join("nested");
    fs::create_dir_all(&nested).unwrap();
    let root = root.canonicalize().unwrap();
    let home = home.canonicalize().unwrap();
    let nested = nested.canonicalize().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .current_dir(&root)
        .env("HOME", &home)
        .args(["-c", "cd; cd nested; cd missing; printf 'new cwd' > artifact; pwd; /bin/pwd; cd -; printf '%s\\n' \"$PWD\" \"$OLDPWD\""])
        .output().unwrap();
    let artifact = fs::read(nested.join("artifact"));
    fs::remove_dir_all(&root).unwrap();
    assert!(output.status.success());
    assert_eq!(artifact.unwrap(), b"new cwd");
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!(
            "{nested}\n{nested}\n{home}\n{home}\n{nested}\n",
            nested = nested.display(),
            home = home.display()
        )
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("cd: missing:"));
}
