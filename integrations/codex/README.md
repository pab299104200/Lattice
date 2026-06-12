# Lattice Codex Integration

Codex can use Lattice through MCP with a single workspace registration:

```toml
[mcp_servers.lattice]
command = "/home/pete/cadres/lattice/daemon/target/release/lattice"
args = ["--stdio", "--workspace", "/path/to/workspace"]
cwd = "/path/to/workspace"
```

Use one `lattice` registration per scope. Do not point a workspace at `$HOME`; use the project directory.

The CLI twins from Phase 3 also work directly from Codex shell commands without MCP configuration:

```bash
/home/pete/cadres/lattice/daemon/target/release/lattice context "where is memory verification handled?"
/home/pete/cadres/lattice/daemon/target/release/lattice impact daemon/crates/lattice-daemon/src/rpc/mcp.rs --no-tests
```
