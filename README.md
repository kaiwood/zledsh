# zledsh

`zledsh` is a work-in-progress standalone Rust port of Kward’s shell for macOS and Linux. `zledsh` is the only executable. Run it without arguments for an interactive shell or use `zledsh -c` to run a command. No Ruby or Kward installation is required.

```sh
cargo run --locked -- -c 'printf "<%s>\n" "$0" "$@"' demo 'two words'
```

This slice supports `cd`, `pwd`, `exit` and a limited `printf` (`%s` and `%%`), quoted positional arguments and simple environment substitutions, sequential commands, foreground pipelines such as `ls | grep pattern`, and redirection on file descriptors 0–9. Use `cat < input` or `read -r line < input` to redirect a command without changing the shell’s own input. When `zledsh` itself reads from redirected stdin, it reads commands from that stream; `read [-r] name` consumes data from the same stream and sets a session variable. The shell runs external commands by path or by searching `PATH`. An executable file without a valid native format or shebang fails with status 126; nothing is handed off to `/bin/sh` as a fallback.

On macOS or Linux, run `target/debug/zledsh` without arguments for an interactive prompt. It supports `PS1`, `PS2`, and a Zsh-style `PROMPT` (try `PROMPT='%~ [%?] %# '`). Interactive sessions load supported commands from `~/.kward/zledshrc` and `~/.zledshrc`, then the file named by the launch environment’s `ENV`, if set. `ENV`, `PS1`, and `PS2` allow parameter expansion, not command substitution. Ctrl+C interrupts a foreground command or pipeline; Ctrl+Z stops it, and `fg` resumes the most recently stopped job. A pipeline reports its last stage’s status, and builtin stages run separately from the live shell. They can read shell variables, but only exported variables are passed to external commands they launch. Use `command &` to start a background job, `jobs` to list jobs, `bg %1` to resume a stopped job in the background, and `fg %1` to bring it to the foreground. `wait "$!"` collects the most recent background pipeline’s status. Background commands do not read from the keyboard.

Startup files support simple aliases such as `alias ll='ls -l'`. Use the alias on the next input line after defining it. Alias bodies can contain a command and its arguments, not pipelines or redirection. `unalias ll` removes it.

Startup files support `if`, `elif`, `else`, and `fi`, including nested branches. For example, add rbenv shims to `PATH` only when rbenv is installed:

```sh
if command -v rbenv >/dev/null 2>&1; then
  export PATH="${RBENV_ROOT:-$HOME/.rbenv}/shims:$PATH"
fi
```

For `rbenv shell` and Tab completion, use `eval "$(rbenv init - bash)"` instead of the `export PATH` line. The generated bootstrap and completion script from rbenv 1.3.2 run natively in `zledsh`. Try typing `rbenv shell sys` and pressing Tab.

### Upgrading from `kwsh`

`zledsh` does not read the old startup files or saved ChatGPT state automatically. Existing files are left untouched.

- Update launch commands and shebangs to use `zledsh`, and change any `KWSH_` environment variables to `ZLEDSH_`. Script filenames do not need to change.
- Copy `~/.kwshrc` to `~/.zledshrc` and, if you use the shared startup file, copy `~/.kward/kwshrc` to `~/.kward/zledshrc`. Review the files for old names. `HISTFILE` can still point at your existing history file.
- For ChatGPT, sign in again with `zledsh --chatgpt`, or, after closing both shells, move `~/.kwsh` to `~/.zledsh` if the destination does not exist. Keep the directory at mode `0700` and the session file at mode `0600`.

### Script with `zledsh`

Run a script with `zledsh ./setup.zledsh arg1 arg2`. The script path becomes `$0`; its arguments become `$1`, `$2`, and so on. The script need not be executable, and its commands can still read from stdin. If the script file is missing, `zledsh` reports the error and exits with status 127 without reading stdin. Use `zledsh -- script-name` if the name starts with `-`. To read the script itself from stdin and pass it arguments, use `zledsh -s arg1 arg2 < setup.zledsh`.

Use `zledsh -i` to force interactive input even on a pipe. It loads startup files, prints prompts, and keeps reading after a syntax error. Raw editing and terminal job control still require a terminal.

Use the same syntax in your startup files, `zledsh -c`, redirected stdin, or at the prompt. `&&` and `||` short-circuit, and `!` reverses a pipeline’s exit status. `for`, `while`, and `until` support `break` and `continue`. `case` selects the first matching pattern, with `*`, `?`, bracket expressions, and `|` alternatives. Quote metacharacters to match them literally; unlike filename patterns, `case` patterns can match slashes and leading dots. Functions declared as `name() { ...; }` support positional arguments, `local`, `shift`, and `return` for standalone calls, not pipeline stages.

Use `printf '%s\n' done >> run.log` to append without replacing the log. `<>file` opens or creates `file` for reading and writing without truncating it. `>|file` explicitly replaces the contents; it currently behaves like `>file` because `noclobber` is not implemented yet.

Command arguments expand before redirects open files, so a `$(...)` in an argument sees the original contents of a file the command redirects to. Redirections are applied left to right: `tool >run.log 2>&1` sends both streams to the log, while `tool 2>&1 >run.log` leaves errors on the original stdout. Use `tool 3<input 0<&3` to copy fd 3 onto stdin, or `tool 3>&-` to close fd 3 for that command. These changes don't affect later commands.

Pass inline input to a command with a here-document:

```sh
name=world
cat <<EOF
Hello, $name.
EOF
```

Use `<<'EOF'` to keep the body literal, or `<<-EOF` to strip leading tabs, not spaces. Unquoted bodies support the shell's parameter and `$(…)` expansions. At the prompt, Ctrl+C cancels an unfinished here-document without running its command.

Unquoted `~` uses `HOME`; `~login` looks up the account’s initial directory. In assignments, both forms work just after `=` or an unquoted `:`, as in `PATH=~/bin:~root/bin`. Unknown logins and quoted tildes stay literal.

Use `${#value}` to get a value’s character count. `${value:=default}` sets and substitutes the default if `value` is unset or empty; `${value:?message}` reports an error instead and exits a non-interactive shell. Without the colon, these operators test only for an unset parameter. Quote text inside an operator word when it should stay together, as in `${value:-"two words"}`.

`${value#pattern}` removes the shortest matching prefix and `${value%pattern}` removes the shortest matching suffix. Use `##` or `%%` for the longest match. With no match, the value is unchanged. Quote the pattern itself to make its metacharacters literal, as in `${value#"*"}`.

Use `$'…'` for quoted text with escapes, such as `$'first\nsecond'`. The result stays one argument. Escaped bytes must form UTF-8; a NUL escape ends the quoted text’s value.

Unquoted parameter expansions and `$(...)` results in command arguments are split using `IFS` (space, tab, and newline by default). Quote expansions like `"$value"` to keep whitespace and empty values in one argument. Assignments, redirection filenames, and here-document bodies are not field-split.

After field splitting, unquoted `*`, `?`, and bracket patterns such as `[a-z]` expand to matching filenames. For example, `printf '%s\n' *.txt` lists matching paths, one argument per filename even if it contains spaces. Quote or escape patterns to pass them literally. A pattern with no matches stays unchanged. Wildcards do not cross `/` or match a leading dot unless the pattern starts that part with a dot. Redirection filenames remain literal with respect to pathname expansion, including at the prompt. Matching uses the C locale and paths are sorted bytewise; other locales’ character and collation rules are not applied. Filenames that are not valid UTF-8 cannot be returned as matches.

Use `"$@"` to forward each positional argument unchanged, including empty arguments. `"$*"` instead joins them into one argument using the first character of `IFS` (a space if unset, no separator if empty). Unquoted `$@` and `$*` split each positional argument separately.

`$(...)` captures stdout in an isolated `zledsh` process. Quote it to keep the result as one argument. `eval`, `.` and `source` execute native `zledsh` syntax in the current shell. For example, `. ./setup.zledsh` loads a script into an existing session. This is a scripting subset, not a Bash interpreter: arithmetic expansion and associative arrays are unsupported. Backquoted command substitution also runs natively; prefer `$(...)` when nesting substitutions.

The completion subset supports indexed array literals, `"${name[@]}"`, `"${name[index]}"`, and `unset 'name[0]'`. Register a trusted callback with `complete -F function command`. On simple unquoted drafts, it receives `COMP_WORDS` and `COMP_CWORD` as data and returns candidates in `COMPREPLY`. `compgen -W wordlist -- prefix` matches a whitespace-separated list. A callback can run local commands, but runs in a separate, time-limited shell so its changes to shell variables and cwd do not carry back to the prompt.

At the `zledsh` prompt:

- Use **Ctrl+R** to search history by substring, **Ctrl+R** again for an older match, **Enter** to run the match, or **Ctrl+G** to return to your draft. Up and Down recall commands by prefix.
- Use **Alt+B / Alt+F** to move by word, or Left/Right, Home/End, and **Ctrl+A / Ctrl+E** to move the cursor. **Ctrl+L** clears the screen without discarding the draft.
- **Ctrl+W** cuts the previous word, **Ctrl+U** cuts to the start of the draft, and **Ctrl+K** cuts to the end of the current line. **Ctrl+Y** pastes the most recent cut. Consecutive cuts are combined in order.
- **Tab** completes simple command names or paths, including `~/` paths. If several matches are possible, the first Tab fills in their shared prefix and shows a grid below the draft. Press **Tab** again to highlight the next match or **Shift+Tab** to go back. The grid closes when you resume editing or press Enter.
- Bracketed paste inserts text into the draft without running it.

History stays in memory unless you set `HISTFILE` before the first prompt, for example with `export HISTFILE="$HOME/.zledsh_history"` in `~/.zledshrc`. `HISTSIZE` defaults to 512. Unsafe history files are left untouched. Use `history pause` before entering sensitive text and `history resume` afterward. Pausing clears saved cuts and disables search, saving cuts, and yanking. It does not erase earlier history or hide input from terminal scrollback.

### Try ChatGPT subscription sign-in

On Linux, browser sign-in requires `xdg-open` and a desktop browser. The first time, start `zledsh --chatgpt`, then continue in your browser with a ChatGPT account eligible for plan usage. Back at the `zledsh` prompt, try `? Say hello in one sentence`. No API key, Codex CLI, or local provider executable is needed. This uses [OpenAI’s Sign in with ChatGPT flow](https://developers.openai.com/siwc/token-sharing-open-source/sign-in) and the [Responses API for ChatGPT plan usage](https://developers.openai.com/siwc/token-sharing-open-source/models-and-inference).

By default `zledsh` selects the first listed model in the account’s catalog and uses that model’s default reasoning effort. To override either, add `~/.zledsh/chatgpt.json` and restart `zledsh`:

```json
{
  "model": "your-model-slug",
  "reasoning_effort": "medium"
}
```

Use a model slug listed for your account. Omit either setting to keep its default. [Reasoning effort](https://developers.openai.com/api/docs/guides/reasoning) can be `none`, `minimal`, `low`, `medium`, `high`, or `xhigh`, depending on what your model supports. `ZLEDSH_CHATGPT_MODEL` and `ZLEDSH_CHATGPT_REASONING_EFFORT` override the corresponding file settings. For example, run `ZLEDSH_CHATGPT_REASONING_EFFORT=high zledsh`. `--chatgpt` takes precedence over a configured local provider. The JSON file does not depend on `.zledshrc` parsing.

Try `? which directory are we in?` or `? how many directories are here?`. ChatGPT gets the current working directory and a count from filesystem metadata; it doesn't have to guess from `ls`. The count excludes hidden directories and symlinks.

Ask `? create an empty file called foo.txt` to let the agent act in the live shell. `zledsh` prints the prompt and `touch foo.txt` **before** executing it, then sends the result back to ChatGPT. Agent commands run with your shell's permissions and without a separate confirmation; they are not sandboxed. Each call can run one supported `zledsh` command or pipeline, with stdin closed to keyboard input. Ask it to *prepare* a command instead to get an editable draft that runs only when you press Enter.

For requests such as `? cat the file`, the agent can show the command's terminal output and end the turn without restating it. Requests to change something or explain the output can still get a reply.

Each request also sends your question, up to 128 visible directory names, and the most recent command, exit status, and a bounded snapshot of its terminal output. Terminal output from agent commands can also be sent back to ChatGPT. Output is omitted if you type during the command, if it uses terminal controls such as an alternate screen, or if it exceeds 32 KB. Redirected output, shell history, and terminal input are not sent.

ChatGPT also gets the last four completed questions and replies in this shell, limited to 4 KiB each. This follow-up context stays in memory and does not include old tool transcripts.

Use `history pause` before handling sensitive data. It clears the follow-up context and last-command snapshot and blocks requests until `history resume`. The live directory is shared again after you resume. Ctrl+C discards a pending answer and returns to the prompt.

Later sessions can start with plain `zledsh`: if no local provider is configured, the first `?` prompt uses the saved ChatGPT account. `zledsh --chatgpt` also reuses the saved account and only opens a browser when sign-in is needed. `zledsh` refreshes access before expiry. Access and refresh tokens are in `~/.zledsh/chatgpt-session`, an owner-only (`0600`) file, separate from shell config and history. It is not encrypted; do not share it. The installation identifier is in `~/.zledsh/chatgpt-host`. `zledsh` creates `~/.zledsh` with mode `0700`. This preview supports one saved account. Quit other `zledsh` sessions, then run `zledsh --chatgpt-sign-out` to remove saved local sessions without changing model settings or moving legacy files. This does not revoke the grant; use your ChatGPT settings for revocation.

### Use a local provider

The experimental `?` prompt is available in interactive `zledsh` with the line editor enabled. Set `ZLEDSH_ASSISTANT_EXEC` to the absolute path of a trusted local provider executable. It receives a JSON object with `instruction` and `context` (`cwd`, `last_command`, `exit_status`, and `last_output`) on stdin and returns a JSON object with optional `answer`, `directory`, and `draft` strings. It does not receive shell history or terminal input. The optional directory changes the live shell’s cwd; a draft is placed in the editor and only runs if you submit it. Ctrl+C cancels the turn, and private mode blocks requests.

## Development

Use Rust 1.85 or newer (edition 2024). From the repository root:

```sh
cargo build --locked
cargo test --locked
cargo fmt --check
```

`cargo test --locked` runs the shell-language, command, and real-terminal regressions. It does not require Ruby, rbenv, or a sibling Kward checkout. On Linux, install util-linux for `/usr/bin/script`, which some Rust unit tests use. The terminal integration tests also need a working PTY (such as `/dev/pts` in a Linux container) and the system `stty` and `ps` commands.

To run just the terminal integration tests:

```sh
cargo test --locked --test terminal
```

These tests drive the built `zledsh` binary in isolated temporary homes. They cover foreground signals and job control, restoration of terminal settings, startup and editing, completion, history privacy, and assistant turns with fake local providers.
