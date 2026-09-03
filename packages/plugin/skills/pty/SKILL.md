---
name: pty
description: Use a real interactive terminal through pty-bridge for REPLs, TUIs, debuggers, installers, prompts, and commands that require a TTY or multiple rounds of input.
---

# Interactive PTY workflow

Use `mcp__pty__start` when a command needs a real terminal or continued interaction.

Immediately after every successful `mcp__pty__start` call:

1. Copy `background_task_command` from the tool result exactly.
2. Invoke the Bash tool with that command and `run_in_background: true`.
3. Do not run the command in the foreground and do not alter, wrap, or inspect it.
4. Wait for Bash to report that the Background Task started before sending PTY input.

The Background Task is the live display and lifecycle owner for the PTY. It remains running while the interactive program runs and prints throttled status/output updates. Do not start a second Background Task for the same PTY session. Do not replace it with polling or sleep loops.

After attachment, use:

- `mcp__pty__read` to read retained terminal output from a cursor.
- `mcp__pty__write` to answer prompts or send control characters.
- `mcp__pty__resize` before interacting with a full-screen TUI when dimensions matter.
- `mcp__pty__signal` for interrupt, terminate, or kill behavior.
- `mcp__pty__status` to inspect state when needed.
- `mcp__pty__close` when the task is complete or abandoned.

Always preserve the `session_id` and `next_cursor` returned by the tools. Continue reading from `next_cursor` so output is not duplicated. A `SessionEnd` hook closes sessions if the conversation ends unexpectedly.
