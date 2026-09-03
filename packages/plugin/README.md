# pty-bridge

Cross-platform interactive PTY sessions for Claude Code. The plugin exposes MCP tools for starting, reading, writing, resizing, signalling, inspecting, and closing real terminal sessions.

The native process starts only after the background Monitor watcher authenticates. Ending either the Monitor task or the host session closes the PTY.

See the repository README for installation, security, and tool documentation.
