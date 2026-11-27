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
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), home.join("probe")).unwrap();
    });
    tty.expect("job> ");
    tty.send("./probe --exact pty::job_control_probe --nocapture\n");
    tty.expect("worker ready\r\n");
    tty.send("\x1a");
    tty.expect("Stopped\r\n");
    tty.expect("job> ");
    tty.send("jobs\n");
    tty.expect("[1] Stopped ");
    tty.expect("job> ");
    tty.send("bg %1\n");
    let background = tty.expect("job> ");
    if !background.contains("worker resumed\r\n") {
        tty.expect("worker resumed\r\n");
    }
    tty.send("jobs\n");
    tty.expect("[1] Running ");
    tty.expect("job> ");
    tty.send("fg %1\n");
    tty.expect("fg %1\x1b[?2004l\r\n");
    tty.expect("worker foreground\r\n");
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

#[path = "terminal/assistant.rs"]
mod assistant;
#[path = "terminal/editor.rs"]
mod editor;

#[test]
fn jobs_selects_a_stopped_current_job_and_a_running_previous_job() {
    let mut tty = Terminal::start("select> ", &[], |home| {
        executable(
            &home.join("stop"),
            "#!/bin/sh\nprintf 'ready to stop\\n'\nkill -STOP \"$$\"\nexit 9\n",
        );
    });
    tty.expect("select> ");
    tty.send("./stop\n");
    tty.expect("Stopped\r\n");
    tty.expect("select> ");
    tty.send("/bin/sleep 30 &\n");
    tty.expect("select> ");
    tty.send("jobs %%; jobs %-\n");
    tty.expect("\r\n[1] Stopped ");
    tty.expect("\r\n[2] Running ");
    tty.expect("select> ");
    tty.send("fg %%; printf 'resumed=%s\\n' \"$?\"\n");
    tty.expect("\r\nresumed=9\r\n");
    tty.expect("select> ");
    tty.send("/bin/kill \"$!\"; wait \"$!\"\n");
    tty.expect("select> ");
    tty.finish();
}

#[test]
fn completion_notice_is_printed_once_and_preserves_the_wait_status() {
    let mut tty = Terminal::start("notice> ", &[], |_| {});
    tty.expect("notice> ");
    // Wait for a later foreground command to return so the first job can
    // complete before the next prompt, without needing an idle redraw.
    tty.send("exit 23 & saved=$!; /bin/sleep 0.2\n");
    tty.expect("\r\n[1] Done(23) ");
    tty.expect("notice> ");
    tty.send(":\n");
    let next_prompt = tty.expect("notice> ");
    assert!(!next_prompt.contains("Done"), "{next_prompt:?}");
    tty.send("wait \"$saved\"; printf 'first=%s\\n' \"$?\"; wait \"$saved\"; printf 'second=%s\\n' \"$?\"\n");
    tty.expect("\r\nfirst=23\r\nsecond=127\r\n");
    tty.expect("notice> ");
    tty.finish();
}

#[test]
fn interactive_exec_keeps_earlier_redirect_after_later_failure() {
    let mut tty = Terminal::start("exec> ", &[], |_| {});
    tty.expect("exec> ");
    tty.send("exec 3>kept 4>missing/file\n");
    tty.expect("zledsh: missing/file:");
    tty.expect("\x1b[?2004hexec> ");
    tty.send("printf '%s\\n' \"$?\" >&3\n");
    tty.expect("\x1b[?2004hexec> ");
    assert_eq!(std::fs::read(tty.home().join("kept")).unwrap(), b"1\n");
    tty.finish();
}

#[test]
fn idle_trap_runs_without_a_keypress_and_preserves_the_draft() {
    let mut tty = Terminal::start("signal> ", &[], |home| {
        executable(
            &home.join("shell-pid"),
            "#!/bin/sh\nprintf '%s\\n' \"$PPID\" > pid\n",
        );
    });
    tty.expect("signal> ");
    tty.send("trap 'printf \"idle-trap:%s\\n\" \"$?\"' USR1; ./shell-pid; false\n");
    tty.expect("\x1b[?2004hsignal> ");
    let pid = std::fs::read_to_string(tty.home().join("pid"))
        .unwrap()
        .trim()
        .parse::<libc::pid_t>()
        .unwrap();
    let draft = "printf '%s\\n' 'first\nsecond'";
    tty.send(&format!("\x1b[200~{draft}\x1b[201~"));
    tty.expect("second'");
    assert_eq!(unsafe { libc::kill(pid, libc::SIGUSR1) }, 0);
    tty.expect("\r\nidle-trap:1\r\n");
    tty.expect("second'");
    tty.send("\n");
    tty.expect("\r\nfirst\r\nsecond\r\n");
    tty.expect("signal> ");
    tty.finish();
}
