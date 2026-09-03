# pty-bridge

Cross-platform interactive PTY support for Claude Code.

`pty-bridge` exposes native terminal sessions through MCP. It uses Unix PTYs and process groups on macOS/Linux and ConPTY plus a Job Object on Windows. The plugin is a single self-contained npm package with native binaries for all supported platforms.

## Architecture

```text
LLM -> MCP start
         |
         +-- create waiting session
         +-- create private one-use ticket (default TTL: 30 minutes)
         +-- return exact Background Task tool + command (no secret)
         |
LLM -> Bash/PowerShell(command, run_in_background=true)
         |
         +-- read ticket out of band
         +-- authenticate over loopback
         +-- start target in a native PTY
         +-- display throttled lifecycle/output events
         |
LLM -> MCP read/write/resize/signal/status

PostToolUse hook -> bind host-session ownership
natural exit     -> automatically finalize and remove credentials
SessionEnd hook  -> finish any session still active
```

MCP instructions, tool descriptions, and structured `next_action` values contain the complete orchestration contract. No Skill is installed or required. Lifecycle correctness is enforced by the native state machine rather than model compliance.

## Install

```sh
claude plugin marketplace add Ezio2000/pty-bridge
claude plugin install pty-bridge@pty-bridge --scope user
```

The marketplace resolves `@pty-bridge/plugin@latest` from npm. Restart Claude Code after installation or upgrading.

For local development:

```sh
cargo build --workspace
node scripts/check-packages.cjs
claude plugin validate packages/plugin
```

`PTY_BRIDGE_BIN` may point the launcher at a development binary.

## MCP tools

- `start`: creates a waiting session and returns `background_task.tool`, `background_task.command`, and `next_action=run_background_task`. The target cannot start until the Background Task authenticates.
- `read`: waits for and reads retained terminal bytes from an independent cursor. UTF-8 text is convenient; base64 is lossless for arbitrary terminal bytes.
- `write`: waits through the attachment race, writes UTF-8/control characters, then waits for output without polling.
- `resize`: changes the active terminal dimensions.
- `signal`: sends interrupt, terminate, or kill semantics to the active process tree.
- `status`: reports the lifecycle state, explicit termination reason, dimensions, retained byte range, and recent output.
- `close`: force-finishes only an active or abandoned session. A naturally finished session is already finalized and does not need `close`.

The lifecycle has four states: `awaiting_background_task`, `starting`, `running`, and `finished`. Every `finished` session includes one reason such as `natural_exit`, `explicit_close`, `background_task_disconnected`, `host_session_ended`, or `start_failed`.

Output is retained in a bounded 1 MiB ring per session. Up to 64 active or retained sessions are kept per MCP server; finalized sessions are evicted oldest-first.

## Security and lifecycle

- Listeners bind only to a random `127.0.0.1` port.
- Background Task and control credentials are separate 256-bit random values.
- The one-use Background Task credential is stored in a private runtime file, never returned through MCP, and consumed only after successful authentication.
- Runtime directories use mode `0700` and files use `0600` on Unix. Windows uses the current user's LocalAppData ACL.
- Natural exit, start failure, ticket expiry, explicit close, Background Task disconnect, host SessionEnd, and MCP shutdown all converge on one finalization path.
- Finalization removes credentials automatically while retaining bounded output and the termination result.
- Windows force termination uses a Job Object; Unix force termination targets the PTY process group.

## Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node scripts/check-packages.cjs
```

Release tags build six native targets in GitHub Actions, assemble them into `@pty-bridge/plugin`, and publish that single package through npm trusted publishing with provenance.

## License

Apache-2.0.
