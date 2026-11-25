# kwsh

`kwsh` is a work-in-progress standalone Rust port of Kward’s shell for macOS and Linux. Both `kwsh -c` and `kward shell -c` run a small subset of shell commands using the same Rust interpreter. The terminal prototype is a separate Cargo example.

```sh
cargo run --locked -- -c 'printf "<%s>\n" "$0" "$@"' demo 'two words'
```

This slice supports `cd`, `pwd`, `exit` and a limited `printf` (`%s` and `%%`), quoted positional arguments and simple environment substitutions, sequential commands, foreground pipelines such as `ls | grep pattern`, and output redirection for fd 1 and 2. With redirected stdin, the shell reads commands from that stream; `read [-r] name` consumes data from the same stream and sets a session variable. The shell runs external commands by path or by searching `PATH`. An executable file without a valid native format or shebang fails with status 126; nothing is handed off to `/bin/sh` as a fallback.

On macOS, run `target/debug/kwsh` or `target/debug/kward shell` without arguments for an interactive prompt. Both use `PS1` and `PS2`; `kward shell` also accepts a Zsh-style `PROMPT` (try `PROMPT='%~ [%?] %# '`), provides basic line editing, and runs supported commands and assignments from `.kward/kwshrc` and `.kwshrc` under your home directory. Plain `kwsh` skips those files. Both profiles load the file named by the launch environment’s `ENV`, if set, after the rc files. `ENV`, `PS1`, and `PS2` allow parameter expansion, not command substitution. Ctrl+C interrupts a foreground command or pipeline; Ctrl+Z stops it, and `fg` resumes the most recently stopped job. A pipeline reports its last stage’s status, and builtin stages run separately from the live shell. Background jobs and the `?` assistant are not implemented yet.

## Development

Use Rust 1.85 or newer (edition 2024). From the repository root:

```sh
cargo build --locked
cargo test --locked
cargo fmt --check
```

### Shell parity checks (macOS)

The parity suite is separate from `cargo test`. It uses Ruby with Minitest and expects a sibling `../kward` checkout at `051440ed` with no local changes to tracked files. It does not switch branches or modify that checkout. Run it against the Ruby reference from this repository:

```sh
ruby tests/shell_parity_test.rb
```

Set `KWSH_REFERENCE` to use a checkout in a different location. Each case runs with a temporary home and working directory. The suite checks both `kward shell` and plain `kwsh`: `-c` arguments, exit status, output and a redirected file, input redirected into the shell, and interactive startup, command output and exit in a PTY. The PTY checks also verify that `kward shell` loads `~/.kwshrc` and plain `kwsh` does not.

To run the same cases against another build, set **both** `KWSH_KWARD_CMD` and `KWSH_PLAIN_CMD` to the corresponding launch commands. They are split into arguments, not evaluated by a shell. Use absolute paths for any path arguments after the executable, since the tests run from a temporary directory. To check the Rust binaries against the same fixtures:

```sh
KWSH_KWARD_CMD='./target/debug/kward shell' \
KWSH_PLAIN_CMD='./target/debug/kwsh' \
ruby tests/shell_parity_test.rb
```

Linux validation is deferred.

### Terminal and process spike (macOS)

```sh
cargo build --locked --bins --example terminal_spike
ruby tests/terminal_spike_test.rb
```

The PTY suite exercises Ctrl+Z and `fg`, restoration of the stopped job’s terminal settings and the original settings on exit, and inheritance of fd 9. It also tests a fake `?` turn over private pipes: the interpreter changes its own working directory, the provider’s draft is editable without executing, and Ctrl+C cancels and reaps a waiting provider without ending the session. The suite also runs both Rust shell profiles to check foreground signals, stop/resume, and restoration of the original terminal settings.

The example recognizes only fixed test commands. It does not parse shell syntax, run arbitrary commands, contact a provider, or change either invocation profile. Ruby runs the test harness, not the Rust example.
