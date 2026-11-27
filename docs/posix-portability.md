# POSIX.1-2024 shell portability inventory

This is an inventory of shell-facing requirements, not a claim that zledsh or the host implements either optional profile.

## User Portability

- [`sh` startup and history](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/sh.html): interactive `ENV`, numbered history, `HISTFILE`, and `HISTSIZE` already have coverage. Audit the precise startup and history rules separately; do not infer full coverage from the existing editor.
- [`fc`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/fc.html): history listing is implemented (`-l`, `-n`, `-r`, and selectors). Editing and replay remain unimplemented, including `FCEDIT`, redirection scope, and how replay handles zledsh's agent requests. The external-editor shortcut does not implement `fc`.
- [Interactive options and job control](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html#set): existing foreground control and prompt-boundary notices are not `monitor` or asynchronous `notify`. Track those and the remaining [`jobs`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/jobs.html) behavior separately. Defer `ignoreeof`, `vi`, and the compatibility option `nolog` unless needed. In Issue 8, `nolog` is marked obsolescent and belongs here, not to XSI.
- `MAIL`, `MAILCHECK`, and `MAILPATH`: defer prompt-time mailbox notifications.

## XSI

- [`ulimit -t`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/ulimit.html) (CPU time) is XSI. The base resource-limit operations are implemented; `-t` remains unsupported.
- `NLSPATH` and localized shell diagnostics: defer until there is a message-catalog and locale plan. Track other XSI-marked shell behaviors independently rather than assuming that implementing one option supports the profile.
