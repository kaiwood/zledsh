use std::{
    fs,
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
};

fn run(home: &Path, args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_zledsh"))
        .env_clear()
        .env("HOME", home)
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C")
        .current_dir(home)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if !input.is_empty() {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    child.wait_with_output().unwrap()
}

fn expect(output: Output, stdout: &str, stderr: &str) {
    assert!(output.status.success(), "{output:?}");
    assert_eq!(output.stdout, stdout.as_bytes());
    assert_eq!(output.stderr, stderr.as_bytes());
}

#[test]
fn verbose_echoes_each_source_line_once_across_input_modes() {
    let home = tempfile::tempdir().unwrap();
    let source = "if :; then\n/bin/cat <<EOF\nhello\nEOF\nfi\nset +v\nprintf 'quiet\\n'\n";
    fs::write(home.path().join("script"), source).unwrap();
    for (args, input) in [
        (vec!["-v", "-c", source], ""),
        (vec!["-o", "verbose", "script"], ""),
        (vec!["-v", "-s"], source),
    ] {
        expect(
            run(home.path(), &args, input),
            "hello\nquiet\n",
            source.split_once("printf").unwrap().0,
        );
    }
    expect(run(home.path(), &["-nv", "script"], ""), "", source);
}

#[test]
fn verbose_distinguishes_sourced_code_from_read_data() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("sourced"), "printf 'inside\\n'\n").unwrap();
    expect(
        run(
            home.path(),
            &["-v", "-s"],
            "read data\nnot shell source\n. ./sourced\nprintf '%s\\n' \"$data\"\n",
        ),
        "inside\nnot shell source\n",
        "read data\n. ./sourced\nprintf 'inside\\n'\nprintf '%s\\n' \"$data\"\n",
    );
}
