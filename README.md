# kwsh

`kwsh` is a work-in-progress standalone Rust port of Kward’s shell for macOS and Linux. `kwsh` is the only executable. Run it without arguments for an interactive shell or use `kwsh -c` to run a command. No Ruby or Kward installation is required. The terminal prototype is a separate Cargo example.

```sh
cargo run --locked -- -c 'printf "<%s>\n" "$0" "$@"' demo 'two words'
```

This slice supports `cd`, `pwd`, `exit` and a limited `printf` (`%s` and `%%`), quoted positional arguments and simple environment substitutions, sequential commands, foreground pipelines such as `ls | grep pattern`, and output redirection for fd 1 and 2. With redirected stdin, the shell reads commands from that stream; `read [-r] name` consumes data from the same stream and sets a session variable. The shell runs external commands by path or by searching `PATH`. An executable file without a valid native format or shebang fails with status 126; nothing is handed off to `/bin/sh` as a fallback.

On macOS or Linux, run `target/debug/kwsh` without arguments for an interactive prompt. It supports `PS1`, `PS2`, and a Zsh-style `PROMPT` (try `PROMPT='%~ [%?] %# '`). Interactive sessions load supported commands from `~/.kward/kwshrc` and `~/.kwshrc`, then the file named by the launch environment’s `ENV`, if set. `ENV`, `PS1`, and `PS2` allow parameter expansion, not command substitution. Ctrl+C interrupts a foreground command or pipeline; Ctrl+Z stops it, and `fg` resumes the most recently stopped job. A pipeline reports its last stage’s status, and builtin stages run separately from the live shell. Use `command &` to start a background job, `jobs` to list jobs, `bg %1` to resume a stopped job in the background, and `fg %1` to bring it to the foreground. `wait "$!"` collects the most recent background pipeline’s status. Background commands do not read from the keyboard.

Startup files support simple aliases such as `alias ll='ls -l'`. Use the alias on the next input line after defining it. Alias bodies can contain a command and its arguments, not pipelines or redirection. `unalias ll` removes it.

For rbenv shims, use `export PATH="$HOME/.rbenv/shims:$PATH"` in `~/.kwshrc` (adjust the path if you use a custom `RBENV_ROOT`). The Ruby reference's `eval "$(rbenv init - bash)"` recipe is not supported yet: it needs compound conditionals, command substitution, `eval`, functions, and programmed completion. The shims-only setup does not install `rbenv shell` support or that completion.

Set `HISTFILE` before the first prompt to opt into persistent history. For example, put `export HISTFILE="$HOME/.kwsh_history"` in `~/.kwshrc`. History otherwise stays in memory; `HISTSIZE` defaults to 512. At the `kwsh` prompt, use Up and Down to recall commands, Tab to complete simple command names or paths, Ctrl+W to erase the last word, and Ctrl+U to erase to the start of the draft. Left/Right, Home/End, and Ctrl+A/Ctrl+E move the cursor. Bracketed paste inserts text into the draft without running it; press Enter to submit. Use `history pause` before entering a sensitive command, then `history resume` to start recording again. Pausing does not erase earlier history or hide your input from terminal scrollback. Unsafe files (including symlinks and files not owned by you with mode `0600`) are left untouched.

### Try ChatGPT subscription sign-in

On Linux, browser sign-in requires `xdg-open` and a desktop browser. The first time, start `kwsh --chatgpt`, then continue in your browser with a ChatGPT account eligible for plan usage. Back at the `kwsh` prompt, try `? Say hello in one sentence`. No API key, Codex CLI, or local provider executable is needed. This uses [OpenAI’s Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) and the [Responses API for ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

By default `kwsh` selects the first listed model in the account’s catalog and uses that model’s default reasoning effort. To override either, add `~/.kwsh/chatgpt.json` and restart `kwsh`:

```json
{
  "model": "your-model-slug",
  "reasoning_effort": "medium"
}
```

Use a model slug listed for your account. Omit either setting to keep its default. [Reasoning effort](https://developers.openai.com/api/docs/guides/reasoning) can be `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`, depending on what your model supports. `KWSH_CHATGPT_MODEL` and `KWSH_CHATGPT_REASONING_EFFORT` override the corresponding file settings. For example, run `KWSH_CHATGPT_REASONING_EFFORT=high kwsh`. `--chatgpt` takes precedence over a configured local provider. The JSON file does not depend on `.kwshrc` parsing.

Try `? which directory are we in?` or `? how many directories are here?`. ChatGPT gets the current working directory and a count from filesystem metadata; it doesn't have to guess from `ls`. The count excludes hidden directories and symlinks.

Ask `? create an empty file called foo.txt` to let the agent act in the live shell. `kwsh` prints the prompt and `touch foo.txt` **before** executing it, then sends the result back to ChatGPT. Agent commands run with your shell's permissions and without a separate confirmation; they are not sandboxed. Each call can run one supported `kwsh` command or pipeline, with stdin closed to keyboard input. Ask it to *prepare* a command instead to get an editable draft that runs only when you press Enter.

For requests such as `? cat the file`, the agent can show the command's terminal output and end the turn without restating it. Requests to change something or explain the output can still get a reply.

Each request also sends your question, up to 128 visible directory names, and the most recent command, exit status, and a bounded snapshot of its terminal output. Terminal output from agent commands can also be sent back to ChatGPT. Output is omitted if you type during the command, if it uses terminal controls such as an alternate screen, or if it exceeds 32 KB. Redirected output, shell history, and terminal input are not sent.

ChatGPT also gets the last four completed questions and replies in this shell, limited to 4 KiB each. This follow-up context stays in memory and does not include old tool transcripts.

Use `history pause` before handling sensitive data. It clears the follow-up context and last-command snapshot and blocks requests until `history resume`. The live directory is shared again after you resume. Ctrl+C discards a pending answer and returns to the prompt.

Later sessions can start with plain `kwsh`: if no local provider is configured, the first `?` prompt uses the saved ChatGPT account. `kwsh --chatgpt` also reuses the saved account and only opens a browser when sign-in is needed. `kwsh` refreshes access before expiry. Access and refresh tokens are in `~/.kwsh/chatgpt-session`, an owner-only (`0600`) file, separate from shell config and history. It is not encrypted; do not share it. The installation identifier is in `~/.kwsh/chatgpt-host`. `kwsh` creates `~/.kwsh` with mode `0700` and moves existing ChatGPT state there on first use. Quit any sessions running an older build before upgrading. This preview supports one saved account. Quit other `kwsh` sessions, then run `kwsh --chatgpt-sign-out` to remove the saved local session without changing model settings. This does not revoke the grant; use your ChatGPT settings for revocation.

### Use a local provider

The experimental `?` prompt is available in interactive `kwsh` with the line editor enabled. Set `KWSH_ASSISTANT_EXEC` to the absolute path of a trusted local provider executable. It receives a JSON object with `instruction` and `context` (`cwd`, `last_command`, `exit_status`, and `last_output`) on stdin and returns a JSON object with optional `answer`, `directory`, and `draft` strings. It does not receive shell history or terminal input. The optional directory changes the live shell’s cwd; a draft is placed in the editor and only runs if you submit it. Ctrl+C cancels the turn, and private mode blocks requests.

## Development

Use Rust 1.85 or newer (edition 2024). From the repository root:

```sh
cargo build --locked
cargo test --locked
cargo fmt --check
```

### Shell parity checks

The parity suite is separate from `cargo test`. It uses Ruby with Minitest and expects a sibling `../kward` checkout at `051440ed` with no local changes to tracked files. It does not switch branches or modify that checkout. Run it against the Ruby reference from this repository:

```sh
ruby tests/shell_parity_test.rb
```

Set `KWSH_REFERENCE` to use a checkout in a different location. Each case runs with a temporary home and working directory. The suite checks command mode, redirected input, and interactive startup and execution in a PTY. The legacy Ruby `kward shell` command is used **only** as a test reference; the Rust project does not ship that command.

Set `KWSH_CMD` to run the same cases against a build of `kwsh`. The value is split into arguments, not evaluated by a shell. Use absolute paths for path arguments after the executable, since the tests run from a temporary directory.

```sh
KWSH_CMD='./target/debug/kwsh' ruby tests/shell_parity_test.rb
```

The Cargo, differential, and terminal suites run on macOS and Linux. On Linux, the Cargo tests use `script` from util-linux. Ruby and the reference’s development gems (including `unicode-display_width`) are needed only for the separate Ruby fixtures. The parity harness starts each shell with an isolated environment, so install those gems where the reference Ruby can find them without `GEM_HOME` or `GEM_PATH` overrides.

### Terminal and process spike

```sh
cargo build --locked --bins --example terminal_spike
ruby tests/terminal_spike_test.rb
```

The PTY suite exercises Ctrl+Z and `fg`, restoration of the stopped job’s terminal settings and the original settings on exit, and inheritance of fd 9. It also tests a fake `?` turn over private pipes: the interpreter changes its own working directory, the provider’s draft is editable without executing, and Ctrl+C cancels and reaps a waiting provider without ending the session. The suite also runs `kwsh` to check foreground signals, stop/resume, and restoration of the original terminal settings.

The example recognizes only fixed test commands. It does not parse shell syntax, run arbitrary commands, contact a provider, or change `kwsh` behavior. Ruby runs the test harness, not the Rust example.
