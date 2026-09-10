# pty-bridge

Cross-platform interactive PTYs for Claude Code, with separate input and output interfaces, native background completion tasks, and a persistent silence monitor.

## Architecture

```text
Claude plugin monitor (one per Claude session)
    ↕ local session connection; suspected-silence notices only
pty-bridge MCP application
    ├── Claude ownership, read acknowledgements, silence policy
    ├── one native bgshell `wait` command per PTY
    └── pty-core
          ├── reader → terminal protocol → bounded byte buffer
          ├── single writer ← user input + terminal replies
          └── supervisor → child, process tree, faults, finalization
```

`pty-core` is a reusable Rust library with no MCP, Claude, hook, schema, or LLM dependency. It exports `Session`, `PtyWriter`, `PtyReader`, `StartSpec`, snapshots, change subscriptions, structured write results, and platform process locators. `Session::start` creates a real process; `writer().write` confirms input bytes; `reader().read` and `read_wait` return independently addressable byte ranges. `Session::wait` is replayable after completion. Holding a reader or writer keeps the public session alive; dropping the last public handle stops its process tree.

The reader never writes to the PTY directly. Terminal responses and user inputs go through one writer queue. Blocking writes hold no lifecycle lock; a separate supervisor can terminate a process tree while input is blocked. macOS/Linux use Unix PTYs and process groups; Windows uses ConPTY and a named Job Object.

## Requirements and installation

Use a Claude Code **interactive CLI session with native plugin monitors available**. Plugin monitors are an experimental Claude capability; they are skipped in unsupported hosts, including non-interactive `-p` sessions. Provider and environment restrictions on Claude's Monitor tool also apply. See the [official plugin monitor reference](https://code.claude.com/docs/en/plugins-reference#monitors) and [Monitor availability](https://code.claude.com/docs/en/tools-reference#monitor-tool).

```sh
claude plugin marketplace add Ezio2000/pty-bridge
claude plugin install pty-bridge@pty-bridge --scope user
```

Restart Claude Code after an upgrade. The npm package contains native binaries for macOS, Linux and Windows, on arm64 and x64. No Skill invocation is required.

For local development:

```sh
cargo build --workspace
claude plugin validate packages/plugin
claude --plugin-dir ./packages/plugin
```

`PTY_BRIDGE_BIN` selects a development binary. The launcher otherwise chooses the bundled native binary, then the workspace debug binary when developing from source.

## Starting and controlling a PTY

1. Claude automatically starts the plugin monitor. Its runtime endpoint is keyed by `CLAUDE_CODE_SESSION_ID`.
2. The `PreToolUse` hook injects the current `host_session_id` into `start`; no directory-based ownership guessing is used.
3. `start` waits at most 5 seconds for a live monitor handshake, then actually creates the PTY and target. If the monitor is unavailable, no target is created.
4. The response contains `background_task.tool`, `background_task.command`, and `run_in_background=true`. Claude immediately executes that exact command to register the PTY's native completion/failure task. The command only waits; it does not start the target. If registration fails or is denied, Claude must close the PTY.
5. Claude uses `write` for input and `read` for terminal output. Successful process creation or byte delivery does not prove a command executed, a password was accepted, or an application is ready.

| Tool | Result and behavior |
|---|---|
| `start` | Actual process state, PTY ID, and exact bgshell wait command. A rapidly exiting process still has a replayable completion result. |
| `write` | `bytes_written`, `interaction_id`, lifecycle state; no output text and no output-wait parameter. Inputs are limited to 64 KiB. |
| `read` | `output.text`, lossless `output.base64`, `text_lossy`, actual `start_cursor..next_cursor`, `dropped_bytes`, state and optional silence notice. Default maximum is 64 KiB; `yield_time_ms` is capped at 30 seconds. |
| `status` | Lifecycle, reason, dimensions, input/output activity timestamps and retained byte range; no terminal body. |
| `resize` | Changes dimensions without restarting silence observation. |
| `signal` | `interrupt` writes Ctrl-C; `terminate` and `kill` act on the process tree independently of the writer. |
| `close` | Idempotently abandons a session. Finished output remains readable. |

Writes have a 5-second deadline. A write failure reports the confirmed byte count and `delivery_uncertain`. At a timeout, an in-flight OS write may not yet have acknowledged all delivered bytes, so the count is a lower bound. Do not automatically retry a partial or uncertain input. A failed or timed-out write ends the session.

The `PostToolUse` read hook acknowledges a receipt identifying the actual returned byte range. Calling `status`, reading old output, skipping a cursor or losing bytes to the ring buffer does not acknowledge unseen bytes. Read-result receipts briefly suppress competing monitor notices while the hook runs; unacknowledged receipts do not indefinitely silence the monitor.

## Silence monitor

A successful start or nonempty user input starts one interaction. The policy uses monotonic time:

| Observation | Notification threshold |
|---|---|
| The interaction produced output which is still unread | 30 seconds since the last output |
| The interaction produced no output | 90 seconds since start/input completion |

Each interaction can produce at most one reminder:

```text
PTY pty_x 疑似停滞，请调用 read 检查。
```

Ordinary output updates the silence origin but does not rearm an already delivered reminder. Input in flight suspends the old candidate; output during a successful write belongs to the new interaction. Actual returned read ranges suppress corresponding candidates. Empty reads, metadata queries, resize, internal terminal replies and connection heartbeats never reset the timer. Before dispatch, the owner checks the interaction, output version, read acknowledgements and lifecycle again. A notice already delivered in a read result is not repeated through the monitor.

The monitor emits no output text, password guesses, readiness assertions or automatic input. Its stdout is reserved for notices; diagnostics use stderr. Normal quiet computation can trigger a reminder, and a stuck application that keeps refreshing output can evade detection. The monitor reports suspected silence, not a proven application hang.

## Lifecycle and cleanup

States are `starting`, `running`, and `finished`. During finalization, `ending=true` rejects new input while workers drain. Reader/writer faults are supervised independently of child exit. On normal exit, the process tree is cleaned up, terminal handles are released while the reader drains, then the final result is published. The drain deadline is 2 seconds; an incomplete drain is reported as a fault.

- The native bgshell receives a final reason and exit code, never a stream of terminal previews. Nonzero target exits and internal failures make the waiting command fail.
- A broken monitor connection ends every PTY owned by that Claude session. There is no reconnect grace period. Closed connections are checked every 250 ms; an unresponsive connection has a 2-second heartbeat deadline. Reconnecting can only support new PTYs.
- Disconnecting an established bgshell wait connection ends its PTY. Duplicate waiters cannot displace the original.
- SessionEnd and MCP shutdown terminate owned process trees. Runtime ownership records include process locators, so SessionEnd can clean up even after the MCP server has disappeared.
- Output remains in a 1 MiB ring per session. Each MCP instance holds up to 64 active or retained sessions; completed sessions are evicted oldest-first when space is needed.

Runtime files are ordinary JSON containing endpoint/ownership metadata. There are no startup tickets, ticket credentials or delayed target-start attachment phases.

## Development and verification

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
node scripts/check-packages.cjs
claude plugin validate packages/plugin
```

Tests cover core PTY I/O and cancellation, byte-range observation, controlled-clock silence policy, separate real monitor processes, bgshell completion, ownership and cleanup races. CI runs on macOS, Linux and Windows. Release builds preserve all six native targets and the single self-contained npm package.

Version 2 changes the MCP input/output contract and the internal runtime format. Restart sessions when upgrading; no version-1 compatibility path is provided.

## License

Apache-2.0.
