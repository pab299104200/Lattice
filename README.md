# Lattice

Lattice is a local code, documentation, and memory engine for coding agents. It indexes a workspace into a dependency graph and serves focused context through MCP and the command line: relevant files, symbols, document sections, tests, dependencies, and applicable lessons from prior work.

Use it to find the working set for a task, understand the effect of a change, investigate failures, and carry verified knowledge across sessions. Use direct source reads and tests to establish what is true; use `rg` for exact text searches.

Lattice supports Python, TypeScript, JavaScript, Rust, Go, Java, and Markdown. Structural indexing and keyword retrieval work without an embedding model. Optional local embeddings add search by meaning.

## Install

### Build and put Lattice on PATH

A source installation requires Rust and Cargo. The hook integrations require Bash; the Python evaluation harnesses require Python 3.

```bash
git clone https://github.com/pab299104200/Lattice.git
cd Lattice
cargo build --manifest-path daemon/Cargo.toml --release
mkdir -p "$HOME/.local/bin"
ln -s "$PWD/daemon/target/release/lattice" "$HOME/.local/bin/lattice"
export PATH="$HOME/.local/bin:$PATH"
command -v lattice
```

Add the `export PATH` line to your shell startup file if that directory is not already on PATH. If `lattice` already exists there, inspect the existing installation before replacing it.

Keep the checkout's `integrations/` directory available: the project installer records absolute paths to its hook assets. For a separately packaged binary, set `LATTICE_ASSET_ROOT` to the absolute directory containing `integrations/`. Copying only the executable does not install the hook assets.

### Connect a project

```bash
lattice install --workspace /path/to/project --verify
```

The default installer configures both Codex and Claude Code for one workspace:

| File | Purpose |
| --- | --- |
| `.mcp.json` | Claude Code MCP registration |
| `.codex/config.toml` | Codex MCP registration |
| `.claude/settings.json` | Claude Code hooks |
| `.codex/hooks.json` | Codex hooks |
| `AGENTS.md` | Managed Lattice instructions for Codex |
| `CLAUDE.md` | Managed Lattice instructions for Claude Code |

Existing project instructions, unrelated settings, and other servers are preserved. Reinstalling updates the managed sections without duplicating them. Invalid configuration, malformed instruction markers, unsafe symlink targets, or missing assets fail preflight. Each file is published atomically; the complete installation spans several files. If publication fails partway through, inspect the reported paths, resolve the error, and rerun the command.

Trust the project in your client and reconnect its MCP session. Installation does not grant project trust or bypass approval policy. `--verify` exercises the configured MCP protocol and bounded hook fixtures; it does not prove that an agent will follow the instructions.

Focused installation targets are also available:

```bash
lattice install mcp --workspace /path/to/project
lattice install codex --workspace /path/to/project
lattice install claude-code --workspace /path/to/project
lattice doctor --workspace /path/to/project
```

`install mcp` writes only `.mcp.json`. The `codex` and `claude-code` targets install only their respective hooks. Use the default installer for the complete project setup. MCP-only registration accepts repeated `--workspace` arguments for a multi-root view; complete project installation and hook targets require one workspace.

The installer configures a lightweight MCP proxy that starts or reuses the local daemon. See the [agent integration guide](docs/operator-guide/agent-integration.md) for client-specific configuration and troubleshooting.

### Enable semantic search

ONNX Runtime executes Lattice's local `all-MiniLM-L6-v2` embedding model. Embeddings help match natural-language questions to code whose identifiers use different wording. Inference runs locally; the runtime is not a coding agent or a cloud API.

Install the checksum-verified model and tokenizer once per user:

```bash
lattice install --with-embeddings
```

The model lives at `~/.lattice/models/all-minilm-l6-v2-1110a243/`, or the complete bundle directory selected by `LATTICE_EMBEDDING_MODEL_DIR`. Repeating installation verifies and reuses a valid bundle. Downloads are staged and checked before activation.

**The model installer does not install the ONNX Runtime shared library.** Install a compatible native runtime separately. Lattice has been exercised with ONNX Runtime 1.23.2. Use the archive matching the executable's operating system and architecture from the [official runtime release](https://github.com/microsoft/onnxruntime/releases/tag/v1.23.2).

#### macOS (Apple Silicon)

Install the official archive into a persistent user directory and check its published SHA-256 before extraction:

```bash
(
  set -eu
  runtime_dir="$HOME/.local/share/lattice/onnxruntime"
  mkdir -p "$runtime_dir"
  cd "$runtime_dir"
  curl --fail --location --output onnxruntime-osx-arm64-1.23.2.tgz \
    https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-osx-arm64-1.23.2.tgz
  echo 'b4d513ab2b26f088c66891dbbc1408166708773d7cc4163de7bdca0e9bbb7856  onnxruntime-osx-arm64-1.23.2.tgz' \
    | shasum -a 256 -c -
  tar -xzf onnxruntime-osx-arm64-1.23.2.tgz
)
export ORT_DYLIB_PATH="$HOME/.local/share/lattice/onnxruntime/onnxruntime-osx-arm64-1.23.2/lib/libonnxruntime.1.23.2.dylib"
```

#### Linux (x64)

Run in Bash with `curl`, `tar`, and `sha256sum` installed. This example is for an x64 Lattice executable and uses the CPU runtime; no CUDA installation is needed.

```bash
(
  set -eu
  runtime_dir="$HOME/.local/share/lattice/onnxruntime"
  mkdir -p "$runtime_dir"
  cd "$runtime_dir"
  curl --fail --location --output onnxruntime-linux-x64-1.23.2.tgz \
    https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/onnxruntime-linux-x64-1.23.2.tgz
  echo '1fa4dcaef22f6f7d5cd81b28c2800414350c10116f5fdd46a2160082551c5f9b  onnxruntime-linux-x64-1.23.2.tgz' \
    | sha256sum -c -
  tar -xzf onnxruntime-linux-x64-1.23.2.tgz
)
export ORT_DYLIB_PATH="$HOME/.local/share/lattice/onnxruntime/onnxruntime-linux-x64-1.23.2/lib/libonnxruntime.so.1.23.2"
```

For an ARM64 Linux executable, replace `linux-x64` with `linux-aarch64` throughout the example and use SHA-256 `7c63c73560ed76b1fac6cff8204ffe34fe180e70d6582b5332ec094810241e5c`. The archive must match the executable architecture, including when using emulation. These are upstream native Linux builds; check the [upstream installation requirements](https://onnxruntime.ai/docs/install/) for system compatibility, particularly on musl-based distributions.

#### Windows (x64, PowerShell)

Run in PowerShell. This installs the CPU runtime below your local application-data directory, verifies the archive, and sets `ORT_DYLIB_PATH` for both the current shell and future processes launched with your user environment.

```powershell
& {
    $ErrorActionPreference = 'Stop'
    $runtimeDir = Join-Path $env:LOCALAPPDATA 'Lattice\onnxruntime'
    $archiveName = 'onnxruntime-win-x64-1.23.2.zip'
    $expectedHash = '0b38df9af21834e41e73d602d90db5cb06dbd1ca618948b8f1d66d607ac9f3cd'
    New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null
    $archivePath = Join-Path $runtimeDir $archiveName
    Invoke-WebRequest -Uri "https://github.com/microsoft/onnxruntime/releases/download/v1.23.2/$archiveName" -OutFile $archivePath
    if ((Get-FileHash -LiteralPath $archivePath -Algorithm SHA256).Hash -ne $expectedHash) {
        throw 'ONNX Runtime archive checksum mismatch; extraction stopped.'
    }
    Expand-Archive -LiteralPath $archivePath -DestinationPath $runtimeDir -Force
    $libraryPath = Join-Path $runtimeDir 'onnxruntime-win-x64-1.23.2\lib\onnxruntime.dll'
    if (-not (Test-Path -LiteralPath $libraryPath -PathType Leaf)) {
        throw "ONNX Runtime DLL not found: $libraryPath"
    }
    $env:ORT_DYLIB_PATH = $libraryPath
    [Environment]::SetEnvironmentVariable('ORT_DYLIB_PATH', $libraryPath, 'User')
}
```

For a Windows ARM64 executable, replace `win-x64` with `win-arm64` throughout the example and use SHA-256 `1cfe88b6435df3b5fb0e9f6bd7d6f5df1e887b6174de7f6e2a47bab956f3f168`. An x64 Lattice executable running under emulation still needs the x64 runtime. Windows ONNX Runtime also requires the [Microsoft Visual C++ runtime](https://onnxruntime.ai/docs/install/); install the supported redistributable matching the executable architecture if it is missing. Native Windows execution remains a separate Lattice validation gate.

#### Activate and verify

`ORT_DYLIB_PATH` must be absolute and present in the environment of the process starting the daemon. A terminal export does not configure an already-running daemon or a separately launched desktop client. Set it in the daemon's launch environment, or place the library beside the actual executable using the platform name below. For a PATH symlink, use the directory containing the executable it points to.

| Platform | Library name beside `lattice` |
| --- | --- |
| macOS | `libonnxruntime.dylib` |
| Linux | `libonnxruntime.so` |
| Windows | `onnxruntime.dll` |

Restart the daemon after provisioning or repairing these dependencies, then check `semantic_retrieval` in workspace status. Missing or incompatible assets leave graph and lexical retrieval available with an explicit fallback reason. Runtime initialization is shared across workspaces; a failed initialization is retried only in a new daemon process.

```text
lattice status --workspace /path/to/project --scope index --json
```

On Windows, use your project path, such as `C:\Projects\Beacon`, and reopen the terminal or desktop client after setting the user environment. A Windows service requires its own launch environment. Coordinate daemon restarts with other workspace users; installing the library does not restart Lattice automatically.

Full-graph background vector synchronization is opt-in: set `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC=1` in the daemon environment to enable it. It adds indexing work, particularly on large repositories. A loaded runtime alone does not establish that every source file has a vector. See [embedding provisioning](docs/architecture/2026-08-13-embedding-provisioning.md) and [embedding storage](docs/embedding-storage.md).

## Use Lattice

The public MCP surface contains eight tools:

| Tool | Use it for |
| --- | --- |
| `context` | Unfamiliar code, docs, repository rules, working sets, and handle expansion |
| `prepare_change` | Implementation preparation, edit planning, and scenario tracing |
| `impact` | Dependencies, dependents, diff impact, and relevant tests |
| `diagnose` | Compiler errors, failing tests, stack traces, and runtime failures |
| `search` | Symbols, call paths, document backlinks, and outgoing links |
| `remember` | Reusable lessons, workflow outcomes, and auditable lesson revisions |
| `recall` | Task memory, durable lessons, verification, and delivery acknowledgement |
| `status` | Index health, storage, stale docs, stale memories, and conflicts |

The same verbs are available from the shell:

```bash
lattice context "how does authentication work?"
lattice context "deployment procedure" --mode docs
lattice prepare_change "add retry handling to the payment worker"
lattice impact src/payments/worker.ts
lattice search "PaymentWorker" --kind symbol
lattice diagnose - < failure.txt
lattice recall "payment retry failure" --mode search
lattice remember "Retry only transient failures; verified by the worker regression tests" --kind quick
lattice status --scope index
```

Use `--workspace /path/to/project` when operating outside the target workspace. Shared CLI flags include `--timeout <seconds>` and `--json`. CLI queries connect to the live daemon; an MCP proxy can start it automatically, or an operator can run `lattice --daemon` explicitly. A bare `lattice` invocation does not select a runtime mode.

Default output is compact Markdown. `--json` returns the MCP result envelope; workflow commands also request structured JSON content inside it. Exit codes are `0` for results, `1` for no result or usage/RPC errors, `2` for an unreachable daemon, and `3` for timeout. A result may still be partial: inspect its freshness and completeness fields.

### Agent work and long-running plans

Prompt hooks provide an initial briefing. The installed instructions also direct the agent to use Lattice during its own work:

1. At each bounded task, call `prepare_change`, or `context` for investigation, with concrete file or symbol anchors.
2. Use scoped `recall` before relying on prior knowledge, and check the returned evidence against current code and tests.
3. Use `impact` before broader edits and `diagnose` when implementation or tests fail.
4. Reuse a current task bundle within the task. Refresh after branch/worktree switches, material dependency changes, expired handles, or context compaction.
5. Record verified reusable corrections with `remember`; keep task progress and test results in the plan's execution tracker.
6. Include relevant calls, evidence, and incomplete results in sub-agent handoffs. Each agent needs context for its own task.

When Lattice is unavailable, continue with direct inspection and record the limitation; retry at the next task boundary. A default hook installation does not enforce model behavior or automatically detect arbitrary task transitions. A workspace can opt in with `lattice install --workspace <path> --enforce`: product edits are then denied until `prepare_change` has been served for the checkout, shell-made edits get the same feedback as tool-made edits, and an unavailable Lattice says so once instead of failing silent. Every Lattice failure still allows the tool call. See [hook enforcement](docs/hook-enforcement.md). Actual tool-call traces establish adoption. See [agent workflow](docs/agent-workflow.md).

### Response budgets and follow-up context

Workflow responses support `budget` values `tiny`, `compact`, and `full`, an approximate `max_tokens` cap, `wire_format` values `standard` and `dense`, and `render` values `markdown` and `json`. Tool-specific `mode` selects the operation; use the live MCP schema for its accepted values.

Responses expose budget and truncation metadata, ranking evidence, and guidance about when direct search is more useful. A `context_handle` and `suggested_expand` let an agent request focused follow-up context through `context` with `mode=expand`. Handles persist across restarts but remain subject to expiry, workspace epochs, authority, and current memory lifecycle checks.

A workflow can include a complete applicable lesson, its identity, trust metadata, and delivery receipt when they fit. If they cannot fit, the lesson is withheld with an explicit retrieval path. Persisted navigation handles retain scoped references instead of copied lesson payloads. Ranking diagnostics describe retrieval-time decisions; they do not verify or renew a memory.

### Documentation and Git context

Markdown indexing includes document and section nodes, Markdown links, wiki-links, and code references. Use `context --mode docs` for questions, `search` with `kind=links` for relationships, and `status --scope docs` for likely drift.

Git intelligence contributes bounded file-history signals from up to 500 reachable commits: hotspots, co-change, and attribution summaries. These are secondary ranking and impact advisories. Unavailable, stale, or degraded history does not supply current ranking evidence. See [Git intelligence](docs/architecture/2026-08-13-git-intelligence.md).

## Indexing and workspace isolation

Lattice parses supported source and Markdown, builds symbols and relationships, and keeps the graph current through file watching. Traversal, reads, and tool routing respect canonical workspace boundaries, ignored paths, and security exclusions. A repository's total file count is not the number of supported indexed files.

Check progress from any directory:

```bash
lattice status --workspace /path/to/project --scope index --json
```

For a periodic view:

```bash
while true; do
  lattice status --workspace /path/to/project --scope index
  sleep 5
done
```

Inspect graph counts, parse failures, watcher health, `is_partial`, `graph_storage_state`, `index_work`, and `semantic_retrieval`. `index_work` reports queued, active, and completed jobs. Status does not provide a percentage-complete estimate or ETA. Parsed-source counts and files represented by graph nodes can differ.

Inspect analysis availability separately:

```bash
lattice status --workspace /path/to/project --scope health --json
```

Health facts refresh after graph startup, including a warm startup with no file changes. Failed background production retries with bounded backoff. Git availability is reported separately for file history, symbol history, and co-change: commits too broad for co-change analysis do not suppress complete per-file churn history. Stale history remains excluded. Coverage uses all indexed files as its denominator; test proximity covers eligible production files, and complexity excludes documentation without executable control flow. These counts are not test execution coverage.

A fast startup may reuse a persisted graph and immutable cached objects. It is not evidence of a full source scan. During bootstrap, status reports stages that have not been evaluated. During eligible same-checkout refreshes, queries can use the last published snapshot with explicit refreshing metadata. Branch or substantial workspace changes invalidate incompatible snapshots and handles. Deadline or capacity limits return bounded partial results rather than pretending discovery completed.

### Linked worktrees and multiple roots

Linked Git worktrees share repository memory and immutable parsed content, symbol bodies, and embedding objects. A cold worktree can reuse a committed manifest for unchanged eligible files without rereading, hashing, or parsing their source. Dirty or ineligible files use the normal read/parse path; traversal and metadata checks still occur.

Import resolution, graph generations, and vector membership remain checkout-specific. Lattice does not implement a single base graph with only a worktree overlay. Shared immutable objects avoid repeated content storage and inference while preserving divergent checkout answers. Graph changes and the exact file manifest publish transactionally; failed publication preserves the previous generation.

Multi-root MCP sessions compose canonical workspace shards. Results identify source workspaces and incomplete or failed shards. Secondary shards are leased for active requests and can be evicted afterward. Neither directory-name similarity nor matching remote URLs grants access to another repository's knowledge.

See [parse reuse](docs/parse-history-storage.md), [graph storage](docs/graph-storage.md), and [workspace invalidation](docs/architecture/2026-08-13-worktree-git-invalidation-contract.md).

## Memory

Repository memory is shared across linked worktrees and filtered by repository, branch, checkout, task, and applicability. Keyword retrieval uses maintained SQLite FTS5 and scope indexes. Exact path, symbol, and failure matches retain priority; broad queries have bounded database work.

A memory's confidence, evidence strength, behavioral verification, checkout applicability, contradictions, and retention age are separate dimensions. Recall renews retention only; it does not erase evidence drift or failed verification. Inspect trust labels, evidence links, validity conditions, and suggested recheck commands before relying on a lesson.

### Capture and verification

Hooks supply bounded session, prompt-briefing, edit, turn-summary, and session-close integration, plus an opt-in [plan gate](docs/hook-enforcement.md). Capture accepts allowlisted metadata and a bounded final-assistant-message summary; it does not read transcript files or retain raw prompts, commands, process streams, or environment values in capture state. `Stop` is nonterminal; `SessionEnd` closes a session. Failed close delivery is not successful capture.

Navigation summaries are derived caches. Session prose and edit counts alone cannot certify a reusable correction. Lesson capture requires observed failure, matching resolution, and validated correction. Deterministic capture needs no LLM; optional consolidation is repository-scoped and proposal-only.

Behavioral verification is explicit opt-in. Declare checks in checkout-local `.lattice/verification-checks.json` using schema version 2, then invoke a named check for a specific memory:

```bash
lattice recall --mode verify --memory-id <memory-id> --run-check <check-id>
```

The runner uses declared executables without a shell, bounds execution and output, and binds observations to the memory and source state before and after execution. Verification without `run_check` executes no command. Stored prose and the existence of a test file cannot certify that a test passed.

### Recall, expiry, and feedback

| Time without acknowledged recall | Behavior |
| --- | --- |
| 90 days | Memory becomes retention-stale and leaves default retrieval |
| 180 days | Memory becomes eligible for bounded purge |

Historical memories receive a persisted 30-day migration grace. Configure policy with `LATTICE_MEMORY_STALE_SECS` and `LATTICE_MEMORY_PURGE_SECS`; invalid policy fails startup. One owner scheduler and persisted sweep deadlines prevent extra worktrees or retries from accelerating aging.

Discover retention-stale lessons before purge explicitly:

```bash
lattice recall "payment retry" --include-retention-stale
lattice status --scope memory
lattice status --scope conflicts
```

Only content actually delivered and acknowledged renews retention. MCP and hook responses carry exact `memory_deliveries` receipts; `recall` with `mode=acknowledge_delivery` validates the authority, session, delivery ID, and payload hash. Internal candidates, maintenance, verification, and failed or unacknowledged attempts do not count as recall.

Purge removes lesson payloads and dependent indexes and snapshots through restartable bounded work, preserving evidence still needed elsewhere. Deletion fences and restore floors prevent replay or old backups from resurrecting purged knowledge. Logical row deletion and physical disk reclamation are reported separately.

Use feedback is separate from delivery: a returned `memory_attribution` can be submitted through `remember` with `kind=outcome` and disposition `used` or `not_used`. Feedback survives restart, has idempotent resolution, and does not extend retention. Auditable lesson revisions use `remember` with `kind=evolution`; supersession requires an existing replacement under the same authority.

See [trust and verification](docs/memory-trust.md), [capture](docs/lesson-capture.md), [retrieval](docs/memory-retrieval.md), [retention](docs/memory-retention.md), and [feedback](docs/memory-feedback.md).

### Organization memory

Optional organization memory is configured by the operator in `~/.lattice/config.toml` under `[memory]`, using `organization_id` and an optional absolute `shared_store_path`. Environment overrides are `LATTICE_ORGANIZATION_ID` and `LATTICE_SHARED_MEMORY_PATH`; the default store path is `~/.lattice/shared/memories.db`.

A request cannot widen this configured authority. Cross-repository results remain explicitly advisory and unverified until verified locally. This is local shared-memory support, not a hosted multi-user service with team authentication or distributed writes. Team operation is an architectural extension considered in the [storage and memory design](docs/plans/2026-09-12-storage-and-agent-memory-redesign.md).

## Storage and recovery

An ordinary Git repository owns its shared storage in the primary checkout's `.lattice/`. Linked worktrees use that same proven home. Bare and separate-Git-directory layouts use `<common-git-dir>/lattice`; non-Git workspaces use their own `.lattice/`.

| Storage class | Contents |
| --- | --- |
| Repository knowledge | `memories.db`, evidence, verification, delivery, and feedback state |
| Shared derived objects | Parsed-file cache, immutable symbol bodies, and embedding objects |
| Checkout cache | `checkouts/<checkout-id>/cache/`: graph, vectors, ANN accelerator, and navigation handles |
| Checkout event history | Event database and snapshots outside the disposable cache bundle |
| Telemetry | Repository-owned `adoption_metrics.sqlite3` |
| User assets | Shared model bundle and separately installed ONNX Runtime |

SQLite is the local persistence layer. Graph snapshots, exact vector storage, and ANN accelerators have distinct recovery roles. A damaged ANN accelerator can be reconstructed from SQLite vectors. Durable memory open failures never silently replace the database with an empty store.

### Inspect and reclaim disk space

```bash
lattice storage status --workspace /path/to/project
lattice storage cache plan --workspace /path/to/project --output cache-plan.json
# Review cache-plan.json before applying it.
lattice storage cache apply --workspace /path/to/project --plan cache-plan.json
```

Status distinguishes logical bytes, filesystem allocation, WAL, free pages, reclaimable cache, and incomplete accounting. Planning advances bounded inventory and may require another attempt. Apply rejects stale or altered plans and rechecks ownership, active OS leases, idle grace, and allowed derived files. Unknown artifacts and durable knowledge are preserved. Interrupted cache deletion resumes through a durable rename journal.

Repository cache defaults are a 2 GiB high watermark, a 75% low watermark, and 24-hour idle grace. Shared object collection preserves referenced content. Disk budgets cannot force deletion of active checkouts or protected knowledge. Event compaction reclaims acknowledged payloads while preserving required cursors and receipts.

Cargo build output such as `daemon/target/debug/deps` is separate from Lattice runtime storage. It is not managed by `lattice storage`. Avoid debug-symbol and incremental-build accumulation during development with the build settings below; do not delete a build directory while Cargo is using it.

### Migration, backup, and restore

Startup classifies memory failures such as contention, access denial, full storage, unsupported schema, and corruption. Graph service may continue with memory explicitly unavailable; memory writes fail rather than acknowledging temporary in-process data. Preserve the database and WAL/SHM files when diagnosing a durable-store failure.

Historical repository identities migrate transactionally only when current local Git metadata proves ownership. Record IDs, evidence, and scope are preserved; unrelated identities remain isolated. Repository moves require explicit offline relocation rather than inferred remote aliases.

Knowledge backup and restore are offline operations: stop all Lattice processes using any worktree of that repository before running them.

```bash
lattice storage backup --workspace /path/to/project --destination /path/to/new-backup
lattice storage restore --workspace /path/to/project --backup /path/to/new-backup \
  --replace-existing --confirm-offline
```

Backups include verified database integrity, identity, and checksums. Restore validates current deletion proof and the restore floor, and uses a recoverable journal for replacement. Historical cache retirement is a separate backup-gated offline plan/apply operation. Do not manually remove an entire `.lattice/` directory to rebuild an index.

See [storage operations](docs/operator-storage.md), [memory recovery](docs/memory-recovery.md), [identity migration](docs/memory-identity-migration.md), [cache lifecycle](docs/storage-lifecycle.md), and [event reclamation](docs/event-storage-reclamation.md).

## Operate the daemon

One long-lived local daemon serves multiple workspace proxies. Its loopback endpoint defaults to `127.0.0.1:47659`. Proxies coordinate startup through a cross-process lock and remain connected while client stdin is open, including long gaps between tool calls. Closing client stdin ends the proxy. After a proxy-only update, reconnect the MCP client to launch the new proxy; the shared daemon can remain running.

After rebuilding or changing the ONNX environment, stop the daemon and reconnect clients so the fresh process loads the new binary and configuration. On macOS/Linux:

```bash
pkill -f 'lattice --daemon'
```

This affects every workspace served by that daemon. Coordinate shared-service restarts. A shell-started daemon can also be run explicitly with `lattice --daemon`; configure its environment before launch.

| Setting | Default / purpose |
| --- | --- |
| `LATTICE_DAEMON_ADDR` | `127.0.0.1:47659`; alternate loopback endpoint |
| `LATTICE_DAEMON_EXE` | Explicit absolute binary path for proxy startup |
| `LATTICE_MAX_LOADED_SHARDS` | `3`; inactive non-indexing shards can be evicted |
| `LATTICE_WORKSPACE_IDLE_TTL_SECS` | `1800` |
| `LATTICE_MAX_CONCURRENT_INDEX_JOBS` | `1`; shared indexing capacity |
| `LATTICE_MATERIALIZATION_BUDGET_BYTES` | `2147483648`; logical materialization admission |
| `LATTICE_USER_CACHE_BUDGET_BYTES` | `8589934592`; accounting across proven repository homes |
| `LATTICE_DISPOSABLE_CACHE_BUDGET_BYTES` | `6442450944`; disposable class budget |
| `LATTICE_ENABLE_BACKGROUND_VECTOR_SYNC` | Disabled; set `1` for full background vector sync |
| `LATTICE_LIFECYCLE_LOG_DIR` | `~/.lattice/logs`; process lifecycle log directory |

Admission budgets are logical reservations, not hard RSS limits. Active work or protected data can keep usage above disk watermarks. Capacity pressure remains visible instead of deleting knowledge or publishing a truncated graph as complete. See [resource budgets](docs/resource-budgets.md).

Operational metrics are CLI reports, not additional MCP tools:

```bash
lattice metrics --workspace /path/to/project --days 7
lattice metrics --workspace /path/to/project --memory --json
```

Metrics distinguish direct CLI, MCP, and client hook traffic, and separate memory delivery from reported use. Indexed telemetry retains 90 days and performs bounded reclamation. Telemetry failure is observable without withholding a valid workflow result. Process lifecycle events are recorded in `lifecycle.jsonl` under the configured log directory. See [telemetry storage](docs/telemetry-storage.md).

## Development and validation

| Directory | Contents |
| --- | --- |
| `daemon/crates/lattice-core/` | Parsing, graph, indexing, retrieval, storage, and memory |
| `daemon/crates/lattice-daemon/` | CLI, MCP/RPC, runtime, installation, and metrics |
| `integrations/` | Codex and Claude Code hook packages |
| `tools/` | Storage, retrieval, and agent-efficacy evaluation harnesses |
| `docs/` | Architecture, operator contracts, evaluation evidence, and plans |

Build and run the supported Rust suite:

```bash
cargo build --manifest-path daemon/Cargo.toml --release
cargo test --manifest-path daemon/Cargo.toml --workspace
```

For lower local build-disk usage, use one shared target directory and disable development/test debug information and incremental compilation:

```bash
CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0 CARGO_INCREMENTAL=0 \
  cargo test --manifest-path daemon/Cargo.toml --workspace
```

Integration and evaluation harness checks:

```bash
bash integrations/codex/tests/hooks_test.sh
bash integrations/codex/tests/install_test.sh
bash tools/tests/worth-it-benchmark_test.sh
python3 -W error::ResourceWarning tools/tests/storage-benchmark_test.py
python3 -W error::ResourceWarning tools/tests/efficacy-fixtures_test.py
python3 -W error::ResourceWarning tools/tests/agent-efficacy_test.py
python3 -W error::ResourceWarning tools/tests/efficacy-supersession_test.py
```

CI is configured for Rust tests on Linux, macOS, and Windows, plus release builds for Linux x64, Windows x64, macOS x64, and macOS arm64. Build artifacts include checksums and have 14-day retention. Generated benchmark logs and large local artifacts belong outside tracked source.

Storage and transport tests establish their measured contracts; they do not establish fewer agent mistakes. The [storage benchmark report](docs/reports/2026-09-12-storage-benchmark.md) records worktree reuse, disk reclamation, and latency measurements for identified builds. The [agent efficacy evaluation](docs/agent-efficacy.md) records paired-task evidence and outstanding acceptance gates. Full current paired efficacy, long-plan agent adoption, remote CI execution, and native Windows validation remain separate gates in the [execution tracker](docs/plans/2026-09-12-remediation-execution.md).

## License

MIT
