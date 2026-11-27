use crate::pty::{Terminal, executable, until};
use serde_json::{Value, json};
use std::{fs, io};

#[test]
fn provider_cancellation_privacy_and_editable_draft() {
    let mut tty = Terminal::start("%1~> ", &[], |home| {
        executable(
            &home.join("provider"),
            r#"#!/bin/sh
read -r request
case "$request" in
  *private-request*) touch private-request-sent;;
  *waiting*) echo "$$" > waiting.pid; exec /bin/sleep 60;;
  *) printf '%s\n' '{"directory":"moved","draft":"printf draft > marker"}';;
esac
"#,
        );
        fs::write(
            home.join(".zledshrc"),
            "ZLEDSH_ASSISTANT_EXEC=\"$HOME/provider\"\nHISTFILE=\"$HOME/history.jsonl\"\n",
        )
        .unwrap();
    });
    tty.expect("~> ");
    tty.send("history pause\n");
    tty.expect("[private] ~> ");
    tty.send("? private-request\n");
    tty.expect("Shell agent unavailable while history is paused.");
    tty.expect("[private] ~> ");
    assert!(!tty.home().join("private-request-sent").exists());
    tty.send("history resume\n");
    tty.expect("~> ");
    tty.send("? waiting\n");
    let pid = until(|| {
        fs::read_to_string(tty.home().join("waiting.pid"))
            .ok()?
            .trim()
            .parse::<libc::pid_t>()
            .ok()
    });
    tty.send("\x03");
    tty.expect("turn cancelled\r\n");
    tty.expect("~> ");
    // SAFETY: signal 0 only checks whether the fixture PID still exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1);
    assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    tty.send("? make a draft\n");
    tty.expect("moved> printf draft > marker");
    let marker = tty.home().join("moved/marker");
    assert!(!marker.exists());
    tty.send("-edited\n");
    tty.expect("\x1b[?2004hmoved> ");
    assert_eq!(
        fs::read(marker.with_file_name("marker-edited")).unwrap(),
        b"draft"
    );
    assert!(!marker.exists());
    let saved = fs::read_to_string(tty.home().join("history.jsonl")).unwrap();
    let entries = saved
        .lines()
        .map(|line| serde_json::from_str::<(usize, String)>(line).unwrap().1)
        .collect::<Vec<_>>();
    assert_eq!(
        entries,
        [
            "? waiting",
            "? make a draft",
            "cd 'moved'",
            "printf draft > marker-edited"
        ]
    );
    tty.finish();
}

fn context_terminal() -> Terminal {
    let mut tty = Terminal::start("context> ", &[], |home| {
        executable(
            &home.join("provider"),
            r#"#!/bin/sh
/bin/cat > "$HOME/request.json"
printf '%s\n' '{"answer":"request captured"}'
"#,
        );
        fs::write(
            home.join(".zledshrc"),
            "ZLEDSH_ASSISTANT_EXEC=\"$HOME/provider\"\n",
        )
        .unwrap();
    });
    tty.expect("context> ");
    tty
}

fn request(tty: &mut Terminal) -> Value {
    tty.send("? inspect context\n");
    tty.expect("\r\nrequest captured\r\n");
    tty.expect("context> ");
    serde_json::from_slice(&fs::read(tty.home().join("request.json")).unwrap()).unwrap()
}

#[test]
fn assistant_receives_live_cwd_and_public_context_but_not_private_context() {
    let mut tty = context_terminal();
    tty.send("cd moved\n");
    tty.expect("context> ");
    let command = "/bin/sh -c 'echo visible; echo fixture-failure >&2; echo fd-nine >&9; exit 23'";
    tty.send(&format!("{command}\n"));
    tty.expect("\r\nvisible\r\nfixture-failure\r\n");
    tty.expect("context> ");
    assert_eq!(
        request(&mut tty),
        json!({ "instruction": "inspect context", "context": {
            "cwd": tty.home().join("moved").canonicalize().unwrap(), "last_command": command,
            "last_output": "visible\nfixture-failure\n", "exit_status": 23
        }})
    );
    assert_eq!(fs::read(tty.home().join("fd9")).unwrap(), b"fd-nine\n");
    tty.send("history pause\n");
    tty.expect("[private] context> ");
    tty.send("printf 'private-data\\n'\n");
    tty.expect("\r\nprivate-data\r\n");
    tty.expect("[private] context> ");
    tty.send("history resume\n");
    tty.expect("context> ");
    let request = request(&mut tty);
    for key in ["last_command", "last_output", "exit_status"] {
        assert_eq!(request["context"].get(key), Some(&Value::Null));
    }
    tty.finish();
}

#[test]
fn assistant_omits_output_from_a_job_that_reads_terminal_input() {
    let mut tty = context_terminal();
    let command = r#"/bin/sh -c 'echo waiting-for-input; read text; echo "$text"'"#;
    tty.send(&format!("{command}\n"));
    tty.expect("\r\nwaiting-for-input\r\n");
    tty.send("typed-data\n");
    tty.expect("typed-data\r\ntyped-data\r\n");
    tty.expect("context> ");
    let request = request(&mut tty);
    assert_eq!(request["context"].get("last_output"), Some(&Value::Null));
    assert_eq!(request["context"]["last_command"], command);
    assert_eq!(request["context"]["exit_status"], 0);
    tty.finish();
}
