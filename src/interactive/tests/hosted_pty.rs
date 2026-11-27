use super::*;
use serde_json::{Value, json};
use std::io::Read;
use std::process::Stdio;
use std::time::Duration;

#[test]
fn hosted_turns_in_a_real_terminal() {
    const CHILD: &str = "ZLEDSH_TEST_HOSTED_PTY_CHILD";
    if env::var_os(CHILD).is_some() {
        let status = transcript::run(|transcript| {
            let mut shell = Invocation::new("zledsh", &[]);
            let mut input = io::BufReader::with_capacity(1, crate::process::Stdin);
            let session = chatgpt::Session::fixture(respond);
            std::process::exit(i32::from(
                run_shell(
                    &mut shell,
                    &mut input,
                    false,
                    transcript,
                    Some(Arc::new(session)),
                    true,
                )
                .unwrap(),
            ));
        })
        .unwrap();
        assert_eq!(status, 0);
        return;
    }

    let work = tempfile::tempdir().unwrap();
    let name =
        concat!(module_path!(), "::hosted_turns_in_a_real_terminal").trim_start_matches("zledsh::");
    let mut child = crate::test_terminal::test_command(name, true)
        .current_dir(work.path())
        .envs([
            ("HOME", work.path().to_str().unwrap()),
            ("TERM", "xterm"),
            ("PROMPT", "test> "),
            ("HISTFILE", "history.jsonl"),
        ])
        .env(CHILD, "1")
        .env_remove("ENV")
        .env_remove("KWARD_CONFIG_PATH")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut writer = child.stdin.take().unwrap();
    let mut reader = io::BufReader::new(child.stdout.take().unwrap());
    let result = (|| -> io::Result<()> {
        wait_for(&mut reader, "test> ")?;
        writer.write_all(b"? create a file\n")?;
        wait_for(&mut reader, "\r\ntest> touch foo.txt\r\ncreated\r\ntest> ")?;
        assert_eq!(fs::read(work.path().join("foo.txt"))?, b"");
        writer.write_all(b"? write to that file\n")?;
        wait_for(
            &mut reader,
            "\r\ntest> printf bar > foo.txt\r\nwrote\r\ntest> ",
        )?;
        assert_eq!(fs::read(work.path().join("foo.txt"))?, b"bar");
        writer.write_all(b"? cat the file\n")?;
        wait_for(&mut reader, "\r\ntest> cat foo.txt\r\nbar\r\ntest> ")?;
        writer.write_all(b"history pause\n")?;
        wait_for(&mut reader, "[private] test> ")?;
        writer.write_all(b"? must not be sent\n")?;
        wait_for(
            &mut reader,
            "Shell agent unavailable while history is paused. Use history resume first.\r\n[private] test> ",
        )?;
        writer.write_all(b"history resume\n")?;
        wait_for(&mut reader, "\r\ntest> ")?;
        writer.write_all(b"? memory cleared\n")?;
        wait_for(&mut reader, "cleared\r\ntest> ")?;
        let saved = fs::read_to_string(work.path().join("history.jsonl"))?;
        let entries = saved
            .lines()
            .map(|line| serde_json::from_str::<(usize, String)>(line).unwrap().1)
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            [
                "? create a file",
                "touch foo.txt",
                "? write to that file",
                "printf bar > foo.txt",
                "? cat the file",
                "cat foo.txt",
                "? memory cleared",
            ]
        );
        writer.write_all(b"\x12? cat the file")?;
        wait_for(
            &mut reader,
            "(reverse-i-search)`? cat the file': ? cat the file",
        )?;
        writer.write_all(b"\x07")?;
        wait_for(&mut reader, "test> ")?;
        writer.write_all(b"fc -l 1 2\n")?;
        wait_for(
            &mut reader,
            "1\t? create a file\r\n2\ttouch foo.txt\r\ntest> ",
        )?;
        writer.write_all(b"? listing stays local\n")?;
        wait_for(&mut reader, "listing kept local\r\ntest> ")?;
        fs::write(
            work.path().join("verbose-input"),
            "# VERBOSE_FIXTURE\nset +v\n",
        )?;
        writer.write_all(b"set -v; . ./verbose-input\n")?;
        wait_for(&mut reader, "\r\n# VERBOSE_FIXTURE\r\nset +v\r\ntest> ")?;
        writer.write_all(b"? diagnostics stay local\n")?;
        wait_for(&mut reader, "diagnostics kept local\r\ntest> ")?;

        writer.write_all(b"value=TRACE_FIXTURE\n")?;
        wait_for(&mut reader, "\r\ntest> ")?;
        writer.write_all(b"set -x; : \"$value\"; set +x\n")?;
        wait_for(&mut reader, "\r\n+ : TRACE_FIXTURE\r\n+ set +x\r\ntest> ")?;
        writer.write_all(b"? diagnostics stay local\n")?;
        wait_for(&mut reader, "diagnostics kept local\r\ntest> ")?;

        writer.write_all(b"history pause\n")?;
        wait_for(&mut reader, "[private] test> ")?;
        writer.write_all(b"set -vx\n")?;
        wait_for(&mut reader, "\r\n[private] test> ")?;
        writer.write_all(b": PRIVATE_FIXTURE\n")?;
        wait_for(
            &mut reader,
            "\r\n: PRIVATE_FIXTURE\r\n+ : PRIVATE_FIXTURE\r\n[private] test> ",
        )?;
        writer.write_all(b"set +vx\n")?;
        wait_for(&mut reader, "\r\n[private] test> ")?;
        writer.write_all(b"history resume\n")?;
        wait_for(&mut reader, "\r\ntest> ")?;
        let saved = fs::read_to_string(work.path().join("history.jsonl"))?;
        assert!(!saved.contains("PRIVATE_FIXTURE"));
        assert!(!saved.contains("+ : TRACE_FIXTURE"));
        assert!(!saved.contains("VERBOSE_FIXTURE"));
        writer.write_all(b"printf 'clean\\n'\n")?;
        wait_for(&mut reader, "\r\nclean\r\ntest> ")?;
        writer.write_all(b"? clean output returns\n")?;
        wait_for(&mut reader, "clean snapshot received\r\ntest> ")?;
        // A cancelled worker may still be unwinding after the prompt returns.
        writer.write_all(b"? wait\n")?;
        wait_for(&mut reader, "fixture pending\r\n")?;
        writer.write_all(b"\x03")?;
        wait_for(&mut reader, "turn cancelled\r\ntest> ")?;
        assert!(fs::read_to_string(work.path().join("history.jsonl"))?.contains("\"? wait\""));
        writer.write_all(b"exit\n")?;
        wait_for(&mut reader, "1 passed")?;
        Ok(())
    })();
    if result.is_err() {
        let _ = child.kill();
    }
    assert!(
        child.wait().unwrap().success() && result.is_ok(),
        "{result:?}"
    );
}

fn wait_for(
    reader: &mut io::BufReader<std::process::ChildStdout>,
    expected: &str,
) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let end = std::time::Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while !output.ends_with(expected.as_bytes()) {
        let mut poll = libc::pollfd {
            fd: reader.get_ref().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if std::time::Instant::now() >= end
            || (reader.buffer().is_empty() && unsafe { libc::poll(&mut poll, 1, 100) } == -1)
        {
            return Err(io::Error::other(format!(
                "waiting for {expected:?}: {:?}",
                String::from_utf8_lossy(&output)
            )));
        }
        if reader.buffer().is_empty() && poll.revents == 0 {
            continue;
        }
        let mut byte = [0];
        reader.read_exact(&mut byte)?;
        output.push(byte[0]);
        for mode in [b"\x1b[?2004h", b"\x1b[?2004l"] {
            if output.ends_with(mode) {
                output.truncate(output.len() - mode.len());
            }
        }
    }
    Ok(())
}

fn respond(request: &Value, cancelled: &AtomicBool) -> Result<chatgpt::Reply, chatgpt::Error> {
    let input = request["input"].as_array().unwrap();
    let prompt: Value = input
        .iter()
        .rev()
        .filter_map(|item| item["content"].as_str())
        .find_map(|content| serde_json::from_str(content).ok())
        .unwrap();
    if prompt["question"] == "memory cleared" {
        assert_eq!(input.len(), 1);
    }
    if prompt["question"] == "write to that file" {
        assert_eq!(input[0]["content"], "create a file");
        assert_eq!(input[1]["content"], "created");
    }
    if prompt["question"] == "memory cleared" {
        let event = json!({"type":"response.completed", "response": {"object":"response", "status":"completed", "output":[
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"cleared"}]}
        ]}});
        return chatgpt::Session::fixture_stream(&format!("data: {event}\n\n"), cancelled);
    }
    if prompt["question"] == "listing stays local" {
        assert_eq!(prompt["context"]["last_command"], "fc -l 1 2");
        assert!(prompt["context"]["last_output"].is_null());
        let event = json!({"type":"response.completed", "response": {"object":"response", "status":"completed", "output":[
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"listing kept local"}]}
        ]}});
        return chatgpt::Session::fixture_stream(&format!("data: {event}\n\n"), cancelled);
    }
    let diagnostic_answer = match prompt["question"].as_str() {
        Some("diagnostics stay local") => {
            assert!(prompt["context"]["last_output"].is_null(), "{prompt}");
            Some("diagnostics kept local")
        }
        Some("clean output returns") => {
            assert_eq!(
                prompt["context"]["last_output"].as_str().unwrap().trim(),
                "clean"
            );
            Some("clean snapshot received")
        }
        _ => None,
    };
    if let Some(answer) = diagnostic_answer {
        let event = json!({"type":"response.completed", "response": {"object":"response", "status":"completed", "output":[
            {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":answer}]}
        ]}});
        return chatgpt::Session::fixture_stream(&format!("data: {event}\n\n"), cancelled);
    }
    let (command, needs_followup, answer) = match prompt["question"].as_str().unwrap() {
        "create a file" => ("touch foo.txt", true, "created"),
        "write to that file" => ("printf bar > foo.txt", true, "wrote"),
        "cat the file" => ("cat foo.txt", false, "unexpected recap"),
        "wait" => {
            eprintln!("fixture pending");
            while !cancelled.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(5));
            }
            return Err(chatgpt::Error::Message("fixture cancelled"));
        }
        _ => panic!("unexpected request"),
    };
    let item = if input.last().unwrap()["type"] == "function_call_output" {
        assert!(needs_followup);
        let result: Value =
            serde_json::from_str(input.last().unwrap()["output"].as_str().unwrap()).unwrap();
        assert_eq!(result["exit_status"], 0);
        json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":answer}]})
    } else {
        json!({"type":"function_call", "namespace":"shell", "name":"run_shell_command", "call_id":"fixture_call", "arguments":json!({"command":command,"needs_followup":needs_followup}).to_string()})
    };
    let event = json!({"type":"response.completed", "response": {"object":"response", "status":"completed", "output":[item]}});
    chatgpt::Session::fixture_stream(&format!("data: {event}\n\n"), cancelled)
}
