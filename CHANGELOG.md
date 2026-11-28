# Changelog

## 0.3.0

### Shell

- `set -v` echoes shell input; `set -x` traces expanded commands with a `PS4` prefix. Both write to stderr even when history is paused. Agent snapshots that overlap diagnostics from zledsh or its native workers are discarded.
- Non-UTF-8 data survives external arguments, scalar variables, `read`, command substitution, and native workers. Redirections, `cd` and `pwd`, function arguments, `set --`, `for` loops, and `printf %s` preserve those bytes too. The filesystem must support any non-UTF-8 names used.
- Shell patterns and pathname ordering honor locale settings. Parser input, some builtin operands and variable listings, and the editor still have UTF-8 limits; field splitting and regex handling are not generally locale-aware.
- Added `export -p` and a `print` builtin supporting `-n`, `-r`, and common escapes for prompt helpers.
- `return` stops the innermost sourced file or function. Invalid direct `shift` calls now exit noninteractive scripts; use `command shift` to handle the failure. Earlier successful `exec` redirections persist when a later one fails and the shell survives.
- Aliases inside command substitutions can introduce here-documents, but cannot supply the closing delimiter. Fixed line continuations and enabled blocking reads in stdin script mode.

### Interactive

- Trapped signals run at an idle editing prompt without discarding the draft. Plain script input and `read` may still defer traps.
- Fixed stale editor rows and pasted line breaks.

### Install

```sh
cargo install --locked --git https://github.com/kaiwood/zledsh.git --tag v0.3.0
```

This is a source release for macOS and Linux with Rust 1.85 or newer. Open a new terminal after upgrading. zledsh does not claim POSIX conformance or drop-in compatibility with Bash.

## 0.2.0

### Shell

- `set -a` exports newly assigned scalar variables; `set -u` reports unset parameter expansions; `zledsh -n` checks script syntax without executing it.
- `ulimit` reports and changes base POSIX resource limits in the current shell and its children. The XSI CPU-time option (`-t`) is not included.
- `fc -l` lists interactive history by number, relative position, or prefix. Editing and replay are not included.
- `set -o promptsubst` opts into command substitution in `PROMPT`, so a shell function can show information such as the current Git branch. Enable it only for a trusted prompt.
- Fixed multiline prompt redraws during editing and cleared wrapped history-search results when they are replaced or cancelled.

### Assistant

- `zledsh --filter 'instruction'` transforms piped UTF-8 input with a configured provider. It prints only the result and does not execute provider output.
- Interactive agent commands no longer have a 30-second execution cutoff. Press Ctrl+C to interrupt a foreground command.
- Successful agent commands end the turn without a recap by default. The agent can request a follow-up when another step or explanation is needed.
- Submitted `?` requests and commands run by the agent are separate history entries. Private mode records neither; prepared drafts enter history only if submitted.

### Install

Build from this source release with Rust 1.85 or newer on macOS or Linux:

```sh
cargo install --locked --git https://github.com/kaiwood/zledsh.git --tag v0.2.0
```

Open a new terminal after upgrading. `zledsh` remains an early release, not a drop-in replacement for POSIX `sh` or Bash. See the [README’s current limits](https://github.com/kaiwood/zledsh/blob/v0.2.0/README.md#current-limits).
