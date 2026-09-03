# pty-bridge

Self-contained cross-platform interactive PTY plugin for Claude Code.

The plugin exposes MCP tools for starting and controlling native terminal sessions. Complete orchestration instructions are supplied by the MCP server; no Skill invocation is required. Target processes start only after a host Background Task authenticates, and every terminal outcome automatically finalizes credentials while retaining bounded output for inspection.

The npm artifact contains binaries for macOS, Linux, and Windows on arm64 and x64. See the repository README for installation, lifecycle, security, and tool documentation.
