use crate::pty::{Terminal, executable, until};
use std::{fs, os::unix::fs::PermissionsExt, process::Command};

#[test]
fn startup_prompt_and_cancelled_conditional() {
    let mut tty = Terminal::start(
        "unused> ",
        &[
            ("ENV", "$HOME/startup file"),
            ("PS1", "$MARK! !!> "),
            ("PS2", "more> "),
        ],
        |home| {
            fs::create_dir(home.join(".kward")).unwrap();
            fs::write(home.join(".kward/zledshrc"), "RC=primary\n").unwrap();
            fs::write(
                home.join(".zledshrc"),
                "if true; then RC=\"$RC:personal\"; fi\nENV=ignored\nPROMPT='%~ [%?] %%> '\n",
            )
            .unwrap();
            fs::write(
                home.join("startup file"),
                "printf 'startup:%s\\n' \"$RC\"\nMARK=env\n",
            )
            .unwrap();
            fs::write(home.join("ignored"), "exit 42\n").unwrap();
        },
    );
    assert!(
        tty.expect("~ [0] %> ")
            .contains("startup:primary:personal\r\n")
    );
    tty.send("cd moved; /usr/bin/false\n");
    tty.expect("~/moved [1] %> ");
    tty.send("unset PROMPT\n");
    tty.expect("env3 !> ");
    tty.send("if true; then printf not-yet > skipped\n");
    tty.expect("more> ");
    assert!(!tty.home().join("moved/skipped").exists());
    tty.send("\x03");
    tty.expect("env3 !> ");
    assert!(!tty.home().join("moved/skipped").exists());
    tty.finish();
}

#[test]
fn prompt_substitution_is_opt_in_and_runs_functions_without_changing_status() {
    let mut tty = Terminal::start("‹$(git_branch)› [%?]> ", &[], |home| {
        fs::write(
            home.join(".zledshrc"),
            r#"branch=main
git_branch() { printf '%s\n' "$branch"; branch=worker; return 7; }
"#,
        )
        .unwrap();
    });
    tty.expect("‹$(git_branch)› [0]> ");
    tty.send("set -o promptsubst\n");
    tty.expect("‹main› [0]> ");
    tty.send("branch=topic; false\n");
    tty.expect("‹topic› [1]> ");
    tty.send("printf 'status:%s branch:%s\\n' \"$?\" \"$branch\"\n");
    tty.expect("\r\nstatus:1 branch:topic\r\n");
    tty.expect("‹topic› [0]> ");
    tty.send("set +o promptsubst\n");
    tty.expect("‹$(git_branch)› [0]> ");
    tty.finish();
}

#[test]
fn command_lookup_guards_startup_path() {
    for mode in [0o644, 0o755] {
        let mut tty = Terminal::start("lookup> ", &[], |home| {
            let bin = home.join("rbenv");
            fs::write(&bin, "#!/bin/sh\nexit 0\n").unwrap();
            fs::set_permissions(&bin, fs::Permissions::from_mode(mode)).unwrap();
            fs::write(
                home.join(".zledshrc"),
                r#"export PATH="$HOME:/usr/bin:/bin"
if command -v rbenv >/dev/null 2>&1; then
  export PATH="${RBENV_ROOT:-$HOME/.rbenv}/shims:$PATH"
fi
"#,
            )
            .unwrap();
        });
        tty.expect("lookup> ");
        let home = tty.home().display();
        let path = if mode == 0o755 {
            format!("{home}/.rbenv/shims:{home}:/usr/bin:/bin")
        } else {
            format!("{home}:/usr/bin:/bin")
        };
        tty.send("printf '%s\\n' \"$PATH\"\n");
        tty.expect(&format!("\r\n{path}\r\n"));
        tty.expect("lookup> ");
        tty.finish();
    }
}

#[test]
fn cancelled_draft_leaves_the_interpreter_usable() {
    let mut tty = Terminal::start("rust> ", &[], |_| {});
    tty.expect("rust> ");
    tty.send("exit 17\x03");
    tty.expect("^C\r\n");
    tty.expect("rust> ");
    tty.send("printf '%s-%s\\n' still here\n");
    tty.expect("\r\nstill-here\r\n");
    tty.expect("rust> ");
    tty.finish();
}

#[test]
fn heredoc_prompts_for_input_and_can_be_cancelled() {
    let mut tty = Terminal::start("here> ", &[("PS2", "body> ")], |_| {});
    tty.expect("here> ");
    tty.send("printf wrong > marker <<END\n");
    tty.expect("body> ");
    tty.send("not submitted\n");
    tty.expect("body> ");
    tty.send("\x03");
    tty.expect("here> ");
    assert!(!tty.home().join("marker").exists());
    tty.send("cat <<'END'\n");
    tty.expect("body> ");
    tty.send("actual body\n");
    tty.expect("body> ");
    tty.send("END\n");
    tty.expect("\r\nactual body\r\n");
    tty.expect("here> ");
    tty.finish();
}

#[test]
fn private_commands_are_not_persisted_or_recalled() {
    let mut tty = Terminal::start("hist> ", &[("HISTFILE", "history.jsonl")], |_| {});
    tty.expect("hist> ");
    tty.send("printf 'public-output\\n'\n");
    tty.expect("\r\npublic-output\r\n");
    tty.expect("hist> ");
    tty.send("history pause\n");
    tty.expect("[private] hist> ");
    tty.send("printf 'private-output\\n'\n");
    tty.expect("\r\nprivate-output\r\n");
    tty.expect("[private] hist> ");
    tty.send("history resume\n");
    tty.expect("\x1b[?2004hhist> ");
    tty.send("\x1b[A\n");
    tty.expect("\r\npublic-output\r\n");
    tty.expect("hist> ");
    let home = tty.finish();
    let path = home.path().join("history.jsonl");
    let contents = fs::read_to_string(&path).unwrap();
    let commands: Vec<String> = contents
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            row.as_array()
                .unwrap()
                .last()
                .unwrap()
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    assert_eq!(
        commands,
        [
            "printf 'public-output\\n'",
            "printf 'public-output\\n'",
            "exit 0"
        ]
    );
    assert_eq!(
        fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[test]
fn bracketed_paste_can_be_edited_and_rejects_nul() {
    let mut tty = Terminal::start("paste> ", &[], |_| {});
    tty.expect("paste> ");
    tty.send("\x1b[200~printf first > one\nprintf bad > two\x1b[201~");
    tty.expect("printf bad > two");
    assert!(!tty.home().join("one").exists());
    assert!(!tty.home().join("two").exists());
    tty.send(&format!("{}{}good\n", "\x1b[D".repeat(6), "\x7f".repeat(3)));
    tty.expect("\x1b[?2004hpaste> ");
    assert_eq!(fs::read(tty.home().join("one")).unwrap(), b"first");
    assert_eq!(fs::read(tty.home().join("two")).unwrap(), b"good");
    tty.send("\x1b[200~printf unsafe > three\n\0\x1b[201~");
    tty.expect("\x07");
    assert!(!tty.home().join("three").exists());
    tty.finish();
}

#[test]
fn word_deletion_and_tab_complete_a_name_with_spaces() {
    let mut tty = Terminal::start("edit> ", &[], |home| {
        fs::write(home.join("file with space"), "completed-content\n").unwrap()
    });
    tty.expect("edit> ");
    tty.send("/bin/cat wrong\x17fi\t\n");
    tty.expect("\r\ncompleted-content\r\n");
    tty.expect("edit> ");
    tty.finish();
}

#[test]
fn erasing_a_completion_does_not_repeat_the_multiline_prompt_header() {
    let mut tty = Terminal::start("prompt header\n$ ", &[], |home| {
        fs::write(home.join("report"), "").unwrap();
    });
    tty.expect("prompt header\r\n$ ");
    tty.send("printf '%s\\n' rep\t");
    tty.expect("report ");
    tty.send("\x7f\x7f\x7f\x7f\x7f\x7f\x7freplacement\n");
    let output = tty.expect("\r\nreplacement\r\n");
    assert!(!output.contains("prompt header"), "{output:?}");
    tty.expect("prompt header\r\n$ ");
    tty.send("\x0c");
    tty.expect("\x1b[H\x1b[2Jprompt header\r\n\r\x1b[2K$ ");
    tty.finish();
}

#[test]
fn completion_uses_live_home_without_replacing_the_tilde() {
    let mut tty = Terminal::start("home> ", &[], |home| {
        let notes = home.join("other home/notes");
        fs::create_dir_all(&notes).unwrap();
        fs::write(notes.join("report draft"), "home-completed-content\n").unwrap();
    });
    tty.expect("home> ");
    tty.send("HOME=\"$HOME/other home\"\n");
    tty.expect("\x1b[?2004hhome> ");
    tty.send("/bin/cat ~\t");
    tty.expect("/bin/cat ~/");
    tty.send("no\t");
    tty.expect("notes/");
    tty.send("re\t");
    tty.expect("report\\ draft ");
    tty.send("\n");
    tty.expect("\r\nhome-completed-content\r\n");
    tty.expect("home> ");
    tty.finish();
}

#[test]
fn ambiguous_completion_lists_and_cycles_in_both_directions() {
    let mut tty = Terminal::start("suggest> ", &[], |home| {
        fs::create_dir(home.join(".zled-dir")).unwrap();
        fs::write(home.join(".zledsh notes"), "selected-file-content\n").unwrap();
        fs::write(home.join(".zledshrc.example"), "different-file-content\n").unwrap();
    });
    tty.expect("suggest> ");
    tty.send("/bin/cat ~/.zled\t");
    tty.expect("suggest> /bin/cat ~/.zled\r\x1bD.zled-dir/  .zledsh\\ notes  .zledshrc.example");
    tty.expect("\x1b[1A\r");
    for (key, candidate) in [
        ("\t", ".zled-dir/"),
        ("\t", ".zledsh\\ notes"),
        ("\t", ".zledshrc.example"),
        ("\t", ".zled-dir/"),
        ("\x1b[Z", ".zledshrc.example"),
        ("\x1b[Z", ".zledsh\\ notes"),
    ] {
        tty.send(key);
        tty.expect(&format!("suggest> /bin/cat ~/{candidate}\r\x1bD"));
        tty.expect(&format!("\x1b[7m{candidate}\x1b[27m"));
        tty.expect("\x1b[1A\r");
    }
    tty.send("\n");
    assert!(
        tty.expect("\r\nselected-file-content\r\n")
            .contains("\x1b[1B\r\x1b[J")
    );
    tty.expect("suggest> ");
    tty.finish();
}

#[test]
fn completion_extends_shared_prefix_and_restarts_after_edit() {
    let mut tty = Terminal::start("suggest> ", &[], |home| {
        fs::write(home.join("report-a"), "report-a-content\n").unwrap();
        fs::write(home.join("report-b"), "report-b-content\n").unwrap();
    });
    tty.expect("suggest> ");
    tty.send("/bin/cat ~/re\t");
    tty.expect("suggest> /bin/cat ~/report-\r\x1bDreport-a  report-b");
    tty.expect("\x1b[1A\r");
    tty.send("b\t");
    assert!(tty.expect("b ").contains("\x1b[1B\r\x1b[J"));
    tty.send("\n");
    tty.expect("\r\nreport-b-content\r\n");
    tty.expect("suggest> ");
    tty.finish();
}

#[test]
fn ctrl_k_deletes_to_end_of_current_line() {
    let mut tty = Terminal::start("kill-line> ", &[], |_| {});
    tty.expect("kill-line> ");
    let prefix = "printf '%s\\n' gardé";
    tty.send(&format!(
        "\x1b[200~{prefix} unwanted\nprintf 'next\\n'\x1b[201~\x01{}\x0b-edited\n",
        "\x06".repeat(prefix.chars().count())
    ));
    tty.expect("\r\ngardé-edited\r\nnext\r\n");
    tty.expect("kill-line> ");
    tty.finish();
}

#[test]
fn word_motion_and_clear_screen_keep_the_draft_editable() {
    let mut tty = Terminal::start("move> ", &[], |_| {});
    tty.expect("move> ");
    tty.send("printf '<%s>\\n' one café\x1bbpré-\x1bf-fin\x0c");
    tty.expect("\x1b[H\x1b[2J");
    tty.expect("move> printf '<%s>\\n' one pré-café-fin");
    tty.send("\n");
    tty.expect("\r\n<one>\r\n<pré-café-fin>\r\n");
    tty.expect("move> ");
    tty.finish();
}

#[test]
fn yank_restores_cuts_across_prompts_but_private_mode_clears_them() {
    let mut tty = Terminal::start("yank> ", &[], |_| {});
    tty.expect("yank> ");
    tty.send("/bin/echo first café\x17\x17\x19\n");
    tty.expect("\r\nfirst café\r\n");
    tty.expect("yank> ");
    for cut in ["/bin/echo via-u\x15\n", "/bin/echo via-k\x01\x0b\n"] {
        tty.send(cut);
        tty.expect("\x1b[?2004hyank> ");
        tty.send("\x19\n");
        tty.expect(if cut.contains("via-u") {
            "\r\nvia-u\r\n"
        } else {
            "\r\nvia-k\r\n"
        });
        tty.expect("yank> ");
    }
    tty.send("history pause\n");
    tty.expect("[private] yank> ");
    tty.send("/bin/echo private \x19\n");
    tty.expect("\r\nprivate\r\n");
    tty.expect("[private] yank> ");
    tty.send("private-draft\x15history resume\n");
    tty.expect("\x1b[?2004hyank> ");
    tty.send("/bin/echo resumed \x19\n");
    tty.expect("\r\nresumed\r\n");
    tty.expect("yank> ");
    tty.finish();
}

#[test]
fn rbenv_bootstrap_installs_wrapper_and_completes_commands_and_values() {
    let mut tty = Terminal::start("rbenv> ", &[], |home| {
        fs::write(
            home.join("generated-rbenv-startup"),
            include_str!("../fixtures/rbenv-generated-startup.sh"),
        )
        .unwrap();
        fs::write(
            home.join("mock-completions"),
            include_str!("../fixtures/rbenv-completion.sh"),
        )
        .unwrap();
        fs::write(
            home.join(".zledshrc"),
            r#"export PATH="$HOME:/usr/bin:/bin"
if command -v rbenv >/dev/null 2>&1; then
  eval "$(rbenv init - bash)"
fi
"#,
        )
        .unwrap();
        executable(
            &home.join("rbenv"),
            r#"#!/bin/sh
case "$1" in
  init) exec /bin/cat ./generated-rbenv-startup;;
  rehash) exit 0;;
  commands) printf '%s\n' rehash shell version;;
  completions) [ "$#:$2" = "2:shell" ] || exit 1; printf '%s\n' system custom;;
  sh-shell) [ "$2" = system ] || exit 1; printf 'export RBENV_VERSION=system\n';;
  *) printf '%s\n' "$1";;
esac
"#,
        );
    });
    assert!(!tty.expect("rbenv> ").contains("zledsh:"));
    tty.send("rbenv ver\t\n");
    tty.expect("\r\nversion\r\n");
    tty.expect("rbenv> ");
    tty.send("rbenv shell sys\t\n");
    tty.expect("\x1b[?2004hrbenv> ");
    tty.send("printf '%s:%s:%s\\n' \"$RBENV_VERSION\" \"$fixture_loaded\" \"$(rbenv version)\"\n");
    tty.expect("\r\nsystem:yes:version\r\n");
    tty.expect("rbenv> ");
    tty.finish();
}

#[test]
fn slow_completion_does_not_stall_editor_or_leave_worker_running() {
    let mut tty = Terminal::start("complete> ", &[], |home| {
        fs::write(
            home.join(".zledshrc"),
            r#"_slow() { /bin/sh -c 'echo $$ > worker; exec /bin/sleep 15'; }
complete -F _slow printf
"#,
        )
        .unwrap()
    });
    tty.expect("complete> ");
    tty.send("printf survived\t\n");
    tty.expect("\r\nsurvived");
    tty.expect("complete> ");
    let worker = fs::read_to_string(tty.home().join("worker")).unwrap();
    until(|| {
        let state = Command::new("/bin/ps")
            .args(["-o", "stat=", "-p", worker.trim()])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&state.stdout);
        (text.trim().is_empty() || text.trim().starts_with('Z')).then_some(())
    });
    tty.finish();
}

#[test]
fn history_arrows_search_prefix_and_restore_original_draft() {
    let mut tty = Terminal::start("prefix> ", &[], |_| {});
    tty.expect("prefix> ");
    for word in ["match-first", "match-second", "unrelated"] {
        tty.send(&format!("/bin/echo {word}\n"));
        tty.expect(&format!("\r\n{word}\r\n"));
        tty.expect("prefix> ");
    }
    tty.send("/bin/echo match\x1b[A");
    tty.expect("prefix> /bin/echo match-second");
    tty.send("\x1b[A");
    tty.expect("prefix> /bin/echo match-first");
    tty.send("\x1b[B\x1b[B\n");
    tty.expect("\r\nmatch\r\n");
    tty.expect("prefix> ");
    tty.finish();
}

#[test]
fn reverse_search_cycles_matches_and_preserves_cancelled_draft() {
    let mut tty = Terminal::start("history> ", &[], |_| {});
    tty.expect("history> ");
    for word in ["first-café", "second-café", "unrelated"] {
        tty.send(&format!("/bin/echo {word}\n"));
        tty.expect(&format!("\r\n{word}\r\n"));
        tty.expect("history> ");
    }
    tty.send("/bin/echo draft\x12café");
    tty.expect("(reverse-i-search)`café': /bin/echo second-café");
    tty.send("\x07\n");
    tty.expect("\r\ndraft\r\n");
    tty.expect("history> ");
    tty.send("\x12café\x12");
    tty.expect("(reverse-i-search)`café': /bin/echo first-café");
    tty.send("\n");
    tty.expect("\r\nfirst-café\r\n");
    tty.expect("history> ");
    tty.send("history pause\n");
    tty.expect("[private] history> ");
    tty.send("\x12/bin/echo private\n");
    assert!(!tty.expect("\r\nprivate\r\n").contains("reverse-i-search"));
    tty.expect("[private] history> ");
    tty.finish();
}

#[test]
fn reverse_search_clears_wrapped_matches_when_updating_and_cancelling() {
    let mut tty = Terminal::start("header\nsearch> ", &[], |_| {});
    tty.expect("header\r\nsearch> ");
    let word = format!("wrapped-{}", "x".repeat(90));
    for value in ["wrapped-short", &word] {
        tty.send(&format!("/bin/echo {value}\n"));
        tty.expect(&format!("\r\n{value}\r\n"));
        tty.expect("search> ");
    }
    tty.send("/bin/echo draft\x12wrapped");
    tty.expect(&format!("(reverse-i-search)`wrapped': /bin/echo {word}"));
    tty.send("\x12");
    let update = tty.expect("(reverse-i-search)`wrapped': /bin/echo wrapped-short");
    assert!(update.contains("\x1b[1A\r\x1b[J"), "{update:?}");
    tty.send("\x7f");
    tty.expect(&format!("(reverse-i-search)`wrappe': /bin/echo {word}"));
    tty.send("\x07");
    let restored = tty.expect("search> /bin/echo draft");
    assert!(restored.contains("\x1b[1A\r\x1b[J"), "{restored:?}");
    assert!(!restored.contains("header"), "{restored:?}");
    tty.send("\n");
    tty.expect("\r\ndraft\r\n");
    tty.expect("search> ");
    tty.finish();
}

#[test]
fn delete_and_control_history_navigation_preserve_the_draft() {
    let mut tty = Terminal::start("keys> ", &[], |_| {});
    tty.expect("keys> ");
    for word in ["one", "two"] {
        tty.send(&format!("/bin/echo {word}\n"));
        tty.expect(&format!("\r\n{word}\r\n"));
        tty.expect("keys> ");
    }
    tty.send("/bin/echo éXy\x1b[D\x1b[D\x1b[D\x10");
    tty.expect("keys> /bin/echo two");
    tty.send("\x10");
    tty.expect("keys> /bin/echo one");
    tty.send("\x0e\x0e\x1b[3~\x1b[3~\n");
    tty.expect("\r\ny\r\n");
    tty.expect("keys> ");
    tty.send("history pause\n");
    tty.expect("[private] keys> ");
    tty.send("/bin/echo private\x10\x0e\n");
    tty.expect("\r\nprivate\r\n");
    tty.expect("[private] keys> ");
    tty.finish();
}

#[test]
fn meta_word_cuts_and_yank_pop_are_local_to_public_edits() {
    let mut tty = Terminal::start("cuts> ", &[], |_| {});
    tty.expect("cuts> ");
    tty.send("/bin/echo kept old\x1b\x7f\n");
    tty.expect("\r\nkept\r\n");
    tty.expect("cuts> ");
    tty.send("/bin/echo kept new\x1bb\x1bd\n");
    tty.expect("\r\nkept\r\n");
    tty.expect("cuts> ");
    tty.send("/bin/echo \x19\x1by\x1by\x1by\n");
    tty.expect("\r\nold\r\n");
    tty.expect("cuts> ");
    tty.send("history pause\n");
    tty.expect("[private] cuts> ");
    tty.send("/bin/echo private secret\x1b\x7f\x19\x1by\n");
    tty.expect("\r\nprivate\r\n");
    tty.expect("[private] cuts> ");
    tty.send("history resume\n");
    tty.expect("\x1b[?2004hcuts> ");
    tty.send("/bin/echo empty \x19\x1by\n");
    tty.expect("\r\nempty\r\n");
    tty.expect("cuts> ");
    tty.finish();
}

#[test]
fn forward_history_search_cycles_switches_direction_and_restores_draft() {
    let mut tty = Terminal::start("forward> ", &[], |_| {});
    tty.expect("forward> ");
    for word in ["first-match", "second-match"] {
        tty.send(&format!("/bin/echo {word}\n"));
        tty.expect(&format!("\r\n{word}\r\n"));
        tty.expect("forward> ");
    }
    tty.send("/bin/echo draft\x13match");
    tty.expect("(forward-i-search)`match': /bin/echo first-match");
    tty.send("\x13");
    tty.expect("(forward-i-search)`match': /bin/echo second-match");
    tty.send("\x12");
    tty.expect("(reverse-i-search)`match': /bin/echo first-match");
    tty.send("\x07\n");
    tty.expect("\r\ndraft\r\n");
    tty.expect("forward> ");
    tty.send("\x13match\x13\n");
    tty.expect("\r\nsecond-match\r\n");
    tty.expect("forward> ");
    tty.finish();
}

#[test]
fn undo_restores_multiline_edits_and_resets_on_submit() {
    let mut tty = Terminal::start("undo> ", &[], |_| {});
    tty.expect("undo> ");
    tty.send("/bin/echo é\x1b[200~ bad\n/bin/echo wrong\x1b[201~\x1fX\x1f\n");
    tty.expect("\r\né\r\n");
    tty.expect("undo> ");
    tty.send("\x1f/bin/echo next\n");
    tty.expect("\r\nnext\r\n");
    tty.expect("undo> ");
    tty.finish();
}

#[test]
fn last_argument_recall_keeps_quoting_and_excludes_private_commands() {
    let mut tty = Terminal::start("arg> ", &[], |_| {});
    tty.expect("arg> ");
    tty.send("printf '%s' 'two words' > public\n");
    tty.expect("\x1b[?2004harg> ");
    tty.send("history pause\n");
    tty.expect("[private] arg> ");
    tty.send("/bin/echo classified\n");
    tty.expect("\r\nclassified\r\n");
    tty.expect("[private] arg> ");
    tty.send("/bin/echo absent\x1b.\n");
    tty.expect("\r\nabsent\r\n");
    tty.expect("[private] arg> ");
    tty.send("history resume\n");
    tty.expect("\x1b[?2004harg> ");
    tty.send("printf '<%s>\\n' \x1b.");
    tty.expect("arg> printf '<%s>\\n' 'two words'");
    tty.send("\n");
    tty.expect("\r\n<two words>\r\n");
    tty.expect("arg> ");
    tty.finish();
}

#[test]
fn external_editor_returns_a_draft_and_keeps_original_on_interrupt() {
    let mut tty = Terminal::start("external-edit> ", &[("EDITOR", "./edit-draft")], |home| {
        executable(
            &home.join("edit-draft"),
            r#"#!/bin/sh
case "$(/bin/cat "$1")" in
  *cancel*) printf 'editor ready\n'; exec /bin/sleep 30;;
esac
printf 'printf saved > edited\n' > "$1"
"#,
        );
    });
    tty.expect("external-edit> ");
    tty.send("printf original\x18\x05");
    tty.expect("external-edit> printf saved > edited");
    assert!(!tty.home().join("edited").exists());
    tty.send("\n");
    tty.expect("\x1b[?2004hexternal-edit> ");
    assert_eq!(fs::read(tty.home().join("edited")).unwrap(), b"saved");
    tty.send("/bin/echo cancel\x18\x05");
    tty.expect("editor ready\r\n");
    tty.send("\x03");
    tty.expect("external-edit> /bin/echo cancel");
    tty.send("\n");
    tty.expect("\r\ncancel\r\n");
    tty.expect("external-edit> ");
    tty.send("history pause\n");
    tty.expect("[private] external-edit> ");
    tty.send("/bin/echo private\x18\x05\n");
    tty.expect("\r\nprivate\r\n");
    tty.expect("[private] external-edit> ");
    tty.finish();
}

#[test]
fn completion_keeps_paths_inside_quotes_and_respects_typed_escapes() {
    let mut tty = Terminal::start("quote-tab> ", &[], |home| {
        fs::write(home.join("mid path.txt"), b"original path\n").unwrap();
        fs::create_dir(home.join("some directory")).unwrap();
        fs::write(home.join("some directory/file name"), b"escaped path\n").unwrap();
        fs::write(home.join("a'b.txt"), b"single quote\n").unwrap();
        fs::write(home.join("x$y.txt"), b"double quote\n").unwrap();
    });
    tty.expect("quote-tab> ");
    for (command, expected) in [
        ("/bin/cat some\\ directory/fi\t\n", "escaped path"),
        ("/bin/cat 'a\t\n", "single quote"),
        ("/bin/cat \"x\t\n", "double quote"),
    ] {
        tty.send(command);
        tty.expect(&format!("\r\n{expected}\r\n"));
        tty.expect("quote-tab> ");
    }
    let tail = " path.txt'";
    tty.send(&format!(
        "/bin/cat 'mid{tail}{}\t\n",
        "\x1b[D".repeat(tail.len())
    ));
    tty.expect("\r\noriginal path\r\n");
    tty.expect("quote-tab> ");
    tty.finish();
}

// Optional because Git is not required by the regular Cargo suite.
#[test]
#[ignore = "requires Git on the system PATH"]
fn git_switch_completes_a_real_local_branch() {
    let mut tty = Terminal::start("git-tab> ", &[], |home| {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "Initial",
                "--allow-empty",
            ],
            &["branch", "topic-release"],
        ] {
            let output = Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .current_dir(home)
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
        }
        fs::write(
            home.join(".zledshrc"),
            r#"_git_branches() {
  COMPREPLY=()
  if [ "${COMP_WORDS[1]}" = switch ]; then
    local branches=$(git for-each-ref --format='%(refname:short)' refs/heads)
    COMPREPLY=( $(compgen -W "$branches" -- "${COMP_WORDS[COMP_CWORD]}") )
  fi
}
complete -F _git_branches git
"#,
        )
        .unwrap();
    });
    tty.expect("git-tab> ");
    tty.send("git switch 'topic-r\t\n");
    tty.expect("Switched to branch 'topic-release'");
    tty.expect("git-tab> ");
    assert_eq!(
        fs::read_to_string(tty.home().join(".git/HEAD")).unwrap(),
        "ref: refs/heads/topic-release\n"
    );
    tty.finish();
}

#[test]
fn history_status_does_not_record_controls_and_respects_the_limit() {
    let mut tty = Terminal::start(
        "limited> ",
        &[("HISTSIZE", "3"), ("HISTFILE", "history.jsonl")],
        |_| {},
    );
    tty.expect("limited> ");
    tty.send("history status\n");
    tty.expect("\r\nShell history is recording.\r\n");
    tty.expect("limited> ");
    for word in ["first", "second", "third"] {
        tty.send(&format!("/bin/echo {word}\n"));
        tty.expect(&format!("\r\n{word}\r\n"));
        tty.expect("limited> ");
    }
    tty.send("history pause\n");
    tty.expect("[private] limited> ");
    tty.send("history status\n");
    tty.expect("\r\nShell history is paused.\r\n");
    tty.expect("[private] limited> ");
    tty.send("history resume\n");
    tty.expect("\x1b[?2004hlimited> ");
    let home = tty.finish();
    let contents = fs::read_to_string(home.path().join("history.jsonl")).unwrap();
    let commands: Vec<_> = contents
        .lines()
        .map(|line| serde_json::from_str::<(usize, String)>(line).unwrap().1)
        .collect();
    assert_eq!(commands, ["/bin/echo second", "/bin/echo third", "exit 0"]);
}

#[test]
fn read_prompts_for_an_escaped_terminal_newline() {
    let mut tty = Terminal::start("read> ", &[("PS2", "continue> ")], |_| {});
    tty.expect("read> ");
    tty.send("read value; printf '<%s>\\n' \"$value\"\n");
    tty.send("first\\\n");
    tty.expect("continue> ");
    tty.send("second\n");
    tty.expect("\r\n<firstsecond>\r\n");
    tty.expect("read> ");
    tty.finish();
}

#[test]
fn exit_trap_runs_when_leaving_an_interactive_shell() {
    let mut tty = Terminal::start("trap> ", &[], |_| {});
    tty.expect("trap> ");
    tty.send("trap 'printf \"%s\\n\" \"$value\" > on-exit' EXIT\n");
    tty.expect("trap> ");
    tty.send("value=interactive\n");
    tty.expect("trap> ");
    let home = tty.finish();
    assert_eq!(
        fs::read(home.path().join("on-exit")).unwrap(),
        b"interactive\n"
    );
}

#[test]
fn fc_lists_numbered_history_and_respects_private_mode() {
    let mut tty = Terminal::start("fc> ", &[], |_| {});
    tty.expect("fc> ");
    for source in ["printf 'first\\n'", "printf 'second\\n'"] {
        tty.send(&format!("{source}\n"));
        tty.expect("\x1b[?2004hfc> ");
    }
    tty.send("fc -l 1 2\n");
    tty.expect("\r\n1\tprintf 'first\\n'\r\n2\tprintf 'second\\n'\r\n");
    tty.expect("\x1b[?2004hfc> ");
    tty.send("fc -ln -2 1\n");
    tty.expect("\r\n\tprintf 'second\\n'\r\n\tprintf 'first\\n'\r\n");
    tty.expect("\x1b[?2004hfc> ");
    tty.send("fc -l 'printf' 2 > listed\n");
    tty.expect("\x1b[?2004hfc> ");
    assert_eq!(
        fs::read_to_string(tty.home().join("listed")).unwrap(),
        "2\tprintf 'second\\n'\n"
    );
    tty.send("history pause\n");
    tty.expect("[private] fc> ");
    tty.send("fc -l\n");
    tty.expect("zledsh: fc: history is paused");
    tty.expect("[private] fc> ");
    tty.finish();
}
