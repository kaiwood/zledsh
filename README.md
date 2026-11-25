# kwsh

`kwsh` is a work-in-progress standalone Rust port of Kward’s shell for macOS and Linux. `kwsh` is the only executable. Run it without arguments for an interactive shell or use `kwsh -c` to run a command. No Ruby or Kward installation is required. The terminal prototype is a separate Cargo example.

```sh
cargo run --locked -- -c 'printf "<%s>\n" "$0" "$@"' demo 'two words'
```

This slice supports `cd`, `pwd`, `exit` and a limited `printf` (`%s` and `%%`), quoted positional arguments and simple environment substitutions, sequential commands, foreground pipelines such as `ls | grep pattern`, and output redirection for fd 1 and 2. With redirected stdin, the shell reads commands from that stream; `read [-r] name` consumes data from the same stream and sets a session variable. The shell runs external commands by path or by searching `PATH`. An executable file without a valid native format or shebang fails with status 126; nothing is handed off to `/bin/sh` as a fallback.

On macOS, run `target/debug/kwsh` without arguments for an interactive prompt. It supports `PS1`, `PS2`, and a Zsh-style `PROMPT` (try `PROMPT='%~ [%?] %# '`). Interactive sessions load supported commands from `~/.kward/kwshrc` and `~/.kwshrc`, then the file named by the launch environment’s `ENV`, if set. `ENV`, `PS1`, and `PS2` allow parameter expansion, not command substitution. Ctrl+C interrupts a foreground command or pipeline; Ctrl+Z stops it, and `fg` resumes the most recently stopped job. A pipeline reports its last stage’s status, and builtin stages run separately from the live shell. Background jobs are not implemented yet.

Set `HISTFILE` before the first prompt to opt into persistent history. For example, put `export HISTFILE="$HOME/.kwsh_history"` in `~/.kwshrc`. History otherwise stays in memory; `HISTSIZE` defaults to 512. At the `kwsh` prompt, use Up and Down to recall commands, Tab to complete simple command names or paths, Ctrl+W to erase the last word, and Ctrl+U to clear the draft. Use `history pause` before entering a sensitive command, then `history resume` to start recording again. Pausing does not erase earlier history or hide your input from terminal scrollback. Unsafe files (including symlinks and files not owned by you with mode `0600`) are left untouched.

### Try ChatGPT subscription sign-in (macOS)

The first time, start `kwsh --chatgpt`, then continue in your browser with a ChatGPT account eligible for plan usage. Back at the `kwsh` prompt, try `? Say hello in one sentence`. No API key, Codex CLI, or local provider executable is needed. This uses [OpenAI’s Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) and the [Responses API for ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

By default `kwsh` selects the first listed model in the account’s catalog and uses that model’s default reasoning effort. To override either, add `~/.kwsh/chatgpt.json` and restart `kwsh`:

```json
{
  "model": "your-model-slug",
  "reasoning_effort": "medium"
}
```

Use a model slug listed for your account. Omit either setting to keep its default. [Reasoning effort](https://developers.openai.com/api/docs/guides/reasoning) can be `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`, depending on what your model supports. `KWSH_CHATGPT_MODEL` and `KWSH_CHATGPT_REASONING_EFFORT` override the corresponding file settings. For example, run `KWSH_CHATGPT_REASONING_EFFORT=high kwsh`. `--chatgpt` takes precedence over a configured local provider. The JSON file does not depend on `.kwshrc` parsing.

This preview sends only the text after `?`. It does not send history, read files, run tools, or stage commands. Private mode blocks requests. Ctrl+C discards a pending answer and returns to the prompt. Later sessions can start with plain `kwsh`: if no local provider is configured, the first `?` prompt uses the saved ChatGPT account. `kwsh --chatgpt` also reuses the saved account and only opens a browser when sign-in is needed. `kwsh` refreshes access before expiry. Access and refresh tokens are in `~/.kwsh/chatgpt-session`, an owner-only (`0600`) file, separate from shell config and history. It is not encrypted; do not share it. The installation identifier is in `~/.kwsh/chatgpt-host`. `kwsh` creates `~/.kwsh` with mode `0700` and moves existing ChatGPT state there on first use. Quit any sessions running an older build before upgrading. This preview supports one saved account. To revoke the app’s grant, use your ChatGPT settings.

### Use a local provider

The experimental `?` prompt is available in interactive `kwsh` with the line editor enabled. Set `KWSH_ASSISTANT_EXEC` to the absolute path of a trusted local provider executable. It receives a JSON object with `instruction` on stdin and returns a JSON object with optional `answer`, `directory`, and `draft` strings. It does not receive shell history or terminal input. The optional directory changes the live shell’s cwd; a draft is placed in the editor and only runs if you submit it. Ctrl+C cancels the turn, and private mode blocks requests.

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

Set `KWSH_REFERENCE` to use a checkout in a different location. Each case runs with a temporary home and working directory. The suite checks command mode, redirected input, and interactive startup and execution in a PTY. The legacy Ruby `kward shell` command is used **only** as a test reference; the Rust project does not ship that command.

Set `KWSH_CMD` to run the same cases against a build of `kwsh`. The value is split into arguments, not evaluated by a shell. Use absolute paths for path arguments after the executable, since the tests run from a temporary directory.

```sh
KWSH_CMD='./target/debug/kwsh' ruby tests/shell_parity_test.rb
```

Linux validation is deferred.

### Terminal and process spike (macOS)

```sh
cargo build --locked --bins --example terminal_spike
ruby tests/terminal_spike_test.rb
```

The PTY suite exercises Ctrl+Z and `fg`, restoration of the stopped job’s terminal settings and the original settings on exit, and inheritance of fd 9. It also tests a fake `?` turn over private pipes: the interpreter changes its own working directory, the provider’s draft is editable without executing, and Ctrl+C cancels and reaps a waiting provider without ending the session. The suite also runs `kwsh` to check foreground signals, stop/resume, and restoration of the original terminal settings.

The example recognizes only fixed test commands. It does not parse shell syntax, run arbitrary commands, contact a provider, or change `kwsh` behavior. Ruby runs the test harness, not the Rust example.
