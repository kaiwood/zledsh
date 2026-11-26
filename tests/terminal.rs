#![cfg(any(target_os = "macos", target_os = "linux"))]

mod pty;

use pty::{Terminal, executable};

fn signals_and_resume(suffix: &str) {
    let mut tty = Terminal::start("jobs> ", &[], |home| {
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), home.join("probe")).unwrap()
    });
    tty.expect("jobs> ");
    let command = format!("./probe --exact pty::terminal_probe --nocapture{suffix}\n");
    tty.send(&command);
    tty.expect("foreground ready\r\n");
    tty.send("\x03");
    tty.expect("jobs> ");
    tty.send(&command);
    tty.expect("foreground ready\r\n");
    tty.send("\x1a");
    tty.expect("Stopped\r\n");
    tty.expect("jobs> ");
    tty.send("fg\n");
    tty.expect("fg\x1b[?2004l\r\n");
    tty.send("after resume\n");
    tty.expect("foreground read: after resume\r\n");
    tty.expect("jobs> ");
    tty.finish();
}

// POSIX shell command language 2.11: signal delivery and foreground group handoff.
#[test]
fn foreground_signals_resume_and_restore() {
    signals_and_resume("");
}

#[test]
fn pipeline_stops_and_resumes_as_a_foreground_group() {
    signals_and_resume(" | /bin/cat");
}

#[test]
fn stopped_job_can_run_in_background_and_return_to_foreground() {
    let mut tty = Terminal::start("job> ", &[], |home| {
        executable(
            &home.join("worker"),
            r#"#!/bin/sh
trap 'printf "worker resumed\n"' CONT
printf 'worker ready\n'
while :; do /bin/sleep 1; done
"#,
        )
    });
    tty.expect("job> ");
    tty.send("./worker\n");
    tty.expect("worker ready\r\n");
    tty.send("\x1a");
    tty.expect("Stopped\r\n");
    tty.expect("job> ");
    tty.send("jobs\n");
    tty.expect("[1] Stopped ");
    tty.expect("job> ");
    tty.send("bg %1\n");
    tty.expect("job> ");
    tty.send("jobs\n");
    tty.expect("[1] Running ");
    tty.expect("job> ");
    tty.send("fg %1\n");
    tty.expect("fg %1\x1b[?2004l\r\n");
    tty.expect("worker resumed\r\n");
    tty.send("\x03");
    tty.expect("job> ");
    tty.send("printf '%s\\n' \"$?\"\n");
    tty.expect("\r\n130\r\n");
    tty.expect("job> ");
    tty.finish();
}

#[test]
fn foreground_child_inherits_descriptor_nine() {
    let mut tty = Terminal::start("fd> ", &[], |_| {});
    tty.expect("fd> ");
    tty.send("/bin/sh -c 'printf fd-nine >&9'\n");
    tty.expect("fd> ");
    assert_eq!(std::fs::read(tty.home().join("fd9")).unwrap(), b"fd-nine");
    tty.finish();
}
