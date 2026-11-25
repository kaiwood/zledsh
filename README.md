# kwsh

`kwsh` is a work-in-progress standalone Rust port of Kward’s shell for macOS and Linux. This repository currently contains only the project skeleton; it does not run shell commands yet.

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

To run the same cases against another build, set **both** `KWSH_KWARD_CMD` and `KWSH_PLAIN_CMD` to the corresponding launch commands. They are split into arguments, not evaluated by a shell. Use absolute paths for any path arguments after the executable, since the tests run from a temporary directory. For example, once the Rust implementation supports both profiles:

```sh
KWSH_KWARD_CMD='./target/debug/kward shell' \
KWSH_PLAIN_CMD='./target/debug/kwsh' \
ruby tests/shell_parity_test.rb
```

The current Rust binary is a placeholder and is not expected to pass. The initial baseline is macOS only; Linux validation is deferred.
