# pty-bridge

Interactive PTY plugin for Claude Code.

`pty-bridge` is a cross-platform plugin that gives an LLM a real interactive terminal through MCP. It uses native PTYs through `portable-pty`: Unix `openpty`/process groups on macOS and Linux, and ConPTY on Windows.

The repository keeps the host-specific project name at the boundary. Plugin, MCP, tool, process, and thread identifiers use the neutral `pty-bridge` or `pty` names.

## Flow

```text
Host session
    |
    |  mcp__pty__start(program, args, cwd, monitor=required)
    v
MCP server (pty-bridge)
    |
    +-- create PendingMonitor session
    +-- write private, one-use ticket (default TTL: 30 minutes)
    +-- return session_id + watch_command (no secret)
    |
LLM immediately calls built-in Monitor(watch_command)
    |
    v
watch process -- reads ticket out of band --> loopback authentication
    |                                          |
    |                                          +-- consume ticket
    |                                          +-- spawn target in PTY
    |                                          +-- stream throttled status/tail
    v
LLM uses mcp__pty__read/write/resize/signal/status/close

PostToolUse hook: bind host session -> PTY session ownership
SessionEnd hook: authenticate over the private control channel -> close owned PTYs
Monitor disconnect: close its PTY immediately
```

There is no tmux server, daemon, plugin skill, or model-visible credential. The Monitor is started by the LLM only after `start`; hooks are limited to ownership bookkeeping and guaranteed cleanup.

## Install

Published installations use the plugin package and one optional native package selected by npm:

```sh
npm install @pty-bridge/plugin
```

For local development:

```sh
cargo build --workspace
node scripts/check-packages.cjs
claude plugin validate packages/plugin
```

Then add this repository as a local plugin marketplace or point your development plugin configuration at `packages/plugin`. The launcher automatically uses `target/debug/pty-bridge` when a native npm package is not installed. `PTY_BRIDGE_BIN` can override the binary path.

## MCP tools

- `start`: creates a session. The default `monitor=required` delays process launch until Monitor authentication. `watch_ticket_ttl_seconds` defaults to 1800 and accepts 30–86400.
- `read`: reads retained output from a byte cursor without consuming it globally.
- `write`: sends text/control characters and returns promptly available output.
- `resize`: changes terminal rows and columns.
- `signal`: sends `interrupt`, `terminate`, or `kill` semantics.
- `status`: returns session state, dimensions, retained cursor range, exit code, and tail.
- `close`: terminates the process and releases terminal resources.

Output is retained in a bounded 1 MiB per-session ring. A stale cursor reports the number of dropped bytes. Up to 64 live/retained sessions are kept per MCP server instance.

## Security and lifecycle

- Watch and cleanup listeners bind only to a random `127.0.0.1` port.
- Watch credentials are 256-bit random values stored in private runtime files, consumed once, and never returned through MCP or placed on the command line.
- Control credentials are separate from watch credentials.
- Runtime directories are mode `0700` and files `0600` on Unix. Windows uses the current user's LocalAppData directory and its inherited user ACL.
- A required-monitor process cannot start before the watcher proves possession of its ticket.
- Monitor disconnect, explicit close, host `SessionEnd`, and MCP server shutdown all terminate associated sessions.
- Windows sessions are assigned to a Job Object with `KILL_ON_JOB_CLOSE`; Unix termination targets the PTY process group, so descendant processes are cleaned up on both families.

`monitor=disabled` is available for automation and tests. It intentionally removes the Monitor-disconnect guarantee; the SessionEnd hook and server shutdown still clean up the process.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node scripts/check-packages.cjs
```

Releases are built natively on six GitHub-hosted runner architectures. The native packages are published before `@pty-bridge/plugin`. npm trusted publishing supplies OIDC authentication and provenance; no long-lived npm token is used.

## License

Apache-2.0.
