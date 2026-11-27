# Changelog

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
