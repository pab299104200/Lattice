# MCP proxy lifetime

A stdio proxy belongs to its client transport. It remains connected while client stdin is open, including periods without requests. Inactivity must not terminate the transport: agents can spend minutes implementing or testing between tool calls. Client stdin EOF closes the daemon connection and terminates the proxy. Existing error and daemon reconnection handling remains in effect.

The former five-minute idle reaper and `LATTICE_PROXY_IDLE_TIMEOUT_SECS` setting are removed. Old environment values have no effect. Workspace cache eviction remains a separate daemon policy.

Beacon lifecycle logs on September 14 showed four proxies exiting with `idle_exit` while the shared daemon stayed alive. CLI requests continued working because they opened fresh connections. This explains the persistent client's later `Transport closed` responses; it does not establish that degraded impact coverage has been repaired.

Regression coverage uses a private authenticated daemon fixture, holds client stdin open beyond the former configured timeout, issues a later tool request and checks the response, then verifies prompt EOF shutdown. Separate tests cover immediate EOF and slow in-flight responses.

Activation requires rebuilding the binary and reconnecting affected MCP clients so they launch fresh proxies. Replacing a binary does not update already-running proxies. A shared daemon restart is unnecessary for this proxy-only correction.

## Verification recorded September 14

The coordinator reran all three `stdio_proxy` integration tests successfully using private localhost fixtures after the sandbox denied socket binding. The implementation agent also ran five proxy unit tests successfully. The release build passed and the existing `~/.local/bin/lattice` symlink points to that release binary. Existing compiler warnings remain. No shared daemon or client process was restarted.

A Lattice `context` lookup returned `runtime_bootstrap_pending` with no graph or memory operation performed; the diagnosis and acceptance relied on source, lifecycle logs, and executable regression tests. Live Beacon client reconnection and subsequent long-session validation remain to be performed by its team.
