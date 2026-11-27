# Shell diagnostic tracing

[POSIX.1-2024 `set`](https://pubs.opengroup.org/onlinepubs/9799919799/utilities/V3_chap02.html#set) requires `-v` to write shell input to stderr as it is read, and `-x` to write a trace after expansion and before execution. We trace the command that disables tracing. `PS4` defaults to `+ ` and receives parameter expansion; arithmetic expansion is supported and command substitution is not.

The interactive PTY relay sees stdout and stderr together. Echoes produced while editing or reading input before a capture starts are drained outside the capture. Diagnostics during a capture can originate in the current shell, sourced files, eval, functions, substitutions, pipeline stages, and background workers. Background workers can write during a later command's capture. A flag checked only at the end of the command cannot detect those writes or tracing toggled on and off inside the command.

Use a session-local diagnostic generation shared with native shell workers. A snapshot is eligible only if no diagnostic write overlaps its capture boundaries. Do not suppress POSIX stderr output in private mode. This boundary does not attempt to classify ordinary output from arbitrary external programs or independently launched shells.
