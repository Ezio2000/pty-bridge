# pty-bridge

Self-contained interactive PTY plugin for Claude Code, with a reusable Rust core, separate writer/reader interfaces, native background completion tasks and a persistent silence monitor.

Use an interactive Claude CLI session where plugin monitors are available. The monitor starts automatically. `start` creates the target only after the monitor handshake and returns a native bgshell **wait** command to run in the background. `write` confirms input bytes; `read` is the only source of terminal output. A process can exist without its application being ready.

The monitor reports unread output after 30 seconds of silence, or an interaction with no output after 90 seconds, at most once per interaction. It asks Claude to read and investigate, without streaming output or guessing a cause. A disconnected monitor ends its session's PTYs; native bgshells report completion/failure. SessionEnd can clean up process trees independently of the MCP server.

The package contains native binaries for macOS, Linux and Windows on arm64 and x64. No Skill is required. Restart Claude Code when upgrading to version 2 because the MCP and runtime contracts changed. See the repository README for interfaces, lifecycle, monitor availability, limitations and development instructions.
