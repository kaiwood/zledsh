# Working on zledsh

## Shell behavior

- Use [POSIX.1-2024 shell language](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html) and the [`sh` utility specification](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/sh.html) as the authority for required behavior. Compare with other shells to spot discrepancies, not to settle them.
- Prioritize core POSIX behavior over Bash conveniences. Track User Portability (UP) and XSI separately; document deviations and claim only what has been verified. Passing regression tests alone does not establish conformance.
- Keep `zledsh` standalone. Implement shell behavior natively; never fall back to `/bin/sh` for unsupported syntax or executable text.

## Tests and documentation

- Test behavior changes with focused acceptance cases. Check stdout, stderr where specified, exit status, and side effects as relevant. Add a regression for the failure or boundary being fixed.
- Use temporary homes and working directories. Never modify the user's real startup files or saved state. Use a real PTY for terminal editing, interactive history, and job-control tests.
- Run `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`, plus the relevant parity or PTY checks listed in the [README](README.md#development). Report which checks and platforms were actually verified; macOS and Linux are both supported.
- Update the user-facing docs when behavior changes.
