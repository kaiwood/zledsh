# POSIX.1-2024 shell portability inventory

This is an inventory of shell-facing requirements, not a claim that zledsh or the host implements either optional profile.

## User Portability

- [`sh` startup and history](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/sh.html): interactive `ENV`, numbered history, `HISTFILE`, and `HISTSIZE` already have coverage. Audit the precise startup and history rules separately; do not infer full coverage from the existing editor.
- [`fc`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/fc.html): start with listing (`-l`, `-n`, `-r`, and selectors). Defer editing and replay, including `FCEDIT`, redirection scope, and how replay handles zledsh's agent requests. The external-editor shortcut does not implement `fc`.
- [Interactive options and job control](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html#set): existing foreground control and prompt-boundary notices are not `monitor` or asynchronous `notify`. Track those and the remaining [`jobs`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/jobs.html) behavior separately. Defer `ignoreeof`, `vi`, and the compatibility option `nolog` unless needed. In Issue 8, `nolog` is marked obsolescent and belongs here, not to XSI.
- `MAIL`, `MAILCHECK`, and `MAILPATH`: defer prompt-time mailbox notifications.

## XSI

- [`ulimit -t`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/ulimit.html) (CPU time) is XSI. Issue 8 moved the rest of `ulimit` into the base standard. Implement base resource limits first and keep `-t` separate.
- `NLSPATH` and localized shell diagnostics: defer until there is a message-catalog and locale plan. Track other XSI-marked shell behaviors independently rather than assuming that implementing one option supports the profile.
