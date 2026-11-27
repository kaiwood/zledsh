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

#[test]
fn xtrace_reports_expanded_commands_before_execution_and_honors_ps4() {
    let home = tempfile::tempdir().unwrap();
    let script = r#"value='two words'
PS4='[$tag] '
tag=trace
set -o xtrace
value='two words'
false && printf skipped
printf '<%s>\n' "$value" ''
printf 'stderr\n' >&2
set +x
printf 'quiet\n'
"#;
    expect(
        run(home.path(), &["-c", script], ""),
        "<two words>\n<>\nquiet\n",
        "[trace] 'value=two words'\n[trace] false\n[trace] printf '<%s>\\n' 'two words' ''\n[trace] printf 'stderr\\n'\nstderr\n[trace] set +x\n",
    );
}

#[test]
fn xtrace_keeps_worker_diagnostics_out_of_pipeline_and_substitution_stdout() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("sourced"), "printf '%s' sourced\n").unwrap();
    let result = run(
        home.path(),
        &[
            "-x",
            "-c",
            r#"
f() { . ./sourced; }
value=$(f)
printf '%s\n' "$value" | /bin/cat
( printf '%s\n' background ) &
wait
"#,
        ],
        "",
    );
    assert!(result.status.success(), "{result:?}");
    assert_eq!(result.stdout, b"sourced\nbackground\n");
    let stderr = String::from_utf8(result.stderr).unwrap();
    for line in [
        "+ f",
        "+ . ./sourced",
        "+ printf '%s' sourced",
        "+ value=sourced",
        "+ printf '%s\\n' sourced",
        "+ /bin/cat",
        "+ printf '%s\\n' background",
        "+ wait",
    ] {
        assert!(
            stderr.lines().any(|actual| actual == line),
            "Missing {line:?}: {stderr:?}"
        );
    }
}

#[test]
fn diagnostic_flags_reach_shell_workers_and_verbose_keeps_physical_continuations() {
    let home = tempfile::tempdir().unwrap();
    let source = concat!(
        "printf '%s\\n' \\\n",
        "continued\n",
        "eval 'printf \"flags:%s\\n\" \"$-\"'\n",
        "( printf 'worker:%s\\n' \"$-\" )\n",
    );
    expect(
        run(home.path(), &["-v", "-c", source], ""),
        "continued\nflags:v\nworker:v\n",
        concat!(
            "printf '%s\\n' \\\ncontinued\n",
            "eval 'printf \"flags:%s\\n\" \"$-\"'\n",
            "printf \"flags:%s\\n\" \"$-\"",
            "( printf 'worker:%s\\n' \"$-\" )\n",
        ),
    );
    expect(
        run(
            home.path(),
            &["-x", "-c", "( printf '%s\\n' \"$-\" )\n"],
            "",
        ),
        "x\n",
        "+ printf '%s\\n' x\n",
    );
}
