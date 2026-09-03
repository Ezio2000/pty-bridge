# pty-bridge

Cross-platform interactive PTY sessions for Claude Code. The plugin exposes MCP tools for starting, reading, writing, resizing, signalling, inspecting, and closing real terminal sessions.

The native process starts only after a Claude Code Background Task authenticates. The Background Task continuously displays throttled PTY activity. Ending either the Background Task or the host session closes the PTY.

The bundled `pty` skill teaches Claude to launch the returned command with Bash using `run_in_background=true` immediately after every `mcp__pty__start` call.

The npm artifact bundles binaries for all supported platforms so it remains self-contained when loaded from Claude Code's plugin cache.

See the repository README for installation, security, and tool documentation.
