//! Latency-budget tests for hot-path event capture.
//!
//! These ignored tests measure the in-process append path for `EventWriter::append`
//! without host-filesystem jitter dominating the result. The exact CPU model is
//! captured into the emitted JSON artifact under `daemon/target/event_budget/`,
//! while the writer semantics are stable here in code: SQLite, append-only rows,
//! and batched flush (`FlushPolicy::Batched`) so no per-call fsync should land on
//! `prepare_change` or `get_context_capsule`.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::{
    Actor, BranchRef, CompactSummary, EventPayload, EventStore, EventWriter, FlushPolicy,
    PartialEnvelope, SessionId, ToolCalledPayload,
};
use serde::Serialize;
use serde_json::json;

const HOT_PATH_P99_BUDGET_MICROS: u128 = 5_000;
const WARMUP_ITERATIONS: usize = 200;
static BUDGET_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

#[test]
#[ignore]
fn prepare_change_payload_p99_stays_within_budget() {
    init_tracing();
    let report = measure_case("prepare_change", 10_000, 4_096, || {
        tool_call_envelope("prepare_change", &prepare_change_input_summary(false))
    });
    assert!(
        report.p99_micros <= HOT_PATH_P99_BUDGET_MICROS,
        "prepare_change P99 {}us exceeded budget {}us",
        report.p99_micros,
        HOT_PATH_P99_BUDGET_MICROS
    );
}

#[test]
#[ignore]
fn get_context_capsule_payload_p99_stays_within_budget() {
    init_tracing();
    let report = measure_case("get_context_capsule", 10_000, 4_096, || {
        tool_call_envelope(
            "get_context_capsule",
            &get_context_capsule_input_summary(false),
        )
    });
    assert!(
        report.p99_micros <= HOT_PATH_P99_BUDGET_MICROS,
        "get_context_capsule P99 {}us exceeded budget {}us",
        report.p99_micros,
        HOT_PATH_P99_BUDGET_MICROS
    );
}

#[test]
#[ignore]
fn spilled_payload_p99_stays_within_budget() {
    init_tracing();
    let report = measure_case("prepare_change_spill", 2_000, 256, || {
        tool_call_envelope("prepare_change", &prepare_change_input_summary(true))
    });
    assert!(
        report.p99_micros <= HOT_PATH_P99_BUDGET_MICROS,
        "spill P99 {}us exceeded budget {}us",
        report.p99_micros,
        HOT_PATH_P99_BUDGET_MICROS
    );
}

#[derive(Serialize)]
struct BudgetReport {
    case_name: String,
    iterations: usize,
    inline_ceiling_bytes: usize,
    payload_bytes: usize,
    p50_micros: u128,
    p95_micros: u128,
    p99_micros: u128,
    max_micros: u128,
    cpu_model: String,
    filesystem: String,
    sqlite_mode: serde_json::Value,
}

fn measure_case(
    case_name: &str,
    iterations: usize,
    inline_ceiling_bytes: usize,
    make_envelope: impl Fn() -> PartialEnvelope,
) -> BudgetReport {
    let _guard = BUDGET_TEST_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("budget lock acquires");
    let store = Arc::new(EventStore::open_in_memory().expect("event store opens"));
    let writer = EventWriter::new(
        store.clone(),
        "workspace-main".to_string(),
        inline_ceiling_bytes,
    )
    .with_flush_policy(FlushPolicy::Batched { interval_ms: 250 });
    let payload_bytes = serde_json::to_vec(&make_envelope().payload)
        .expect("payload serializes")
        .len();
    let mut samples = Vec::with_capacity(iterations);

    for _ in 0..WARMUP_ITERATIONS {
        writer
            .append(make_envelope())
            .expect("warmup append succeeds");
    }
    for _ in 0..iterations {
        let envelope = make_envelope();
        let started = Instant::now();
        writer.append(envelope).expect("append succeeds");
        samples.push(started.elapsed().as_micros());
    }
    store.checkpoint_wal().expect("wal checkpoint succeeds");

    let report = BudgetReport {
        case_name: case_name.to_string(),
        iterations,
        inline_ceiling_bytes,
        payload_bytes,
        p50_micros: percentile(&samples, 50),
        p95_micros: percentile(&samples, 95),
        p99_micros: percentile(&samples, 99),
        max_micros: *samples.iter().max().expect("samples exist"),
        cpu_model: cpu_model(),
        filesystem: "in-memory sqlite".to_string(),
        sqlite_mode: json!({
            "journal_mode": "MEMORY",
            "synchronous": "NORMAL",
            "flush_policy": "batched"
        }),
    };
    write_report(&report);
    report
}

fn tool_call_envelope(tool_name: &str, input_summary: &str) -> PartialEnvelope {
    let payload = EventPayload::ToolCalled(ToolCalledPayload {
        call_id: format!("call-{tool_name}"),
        tool_name: tool_name.to_string(),
        context_handle_id: None,
        source_event_id: None,
        input_summary: input_summary.to_string(),
    });
    PartialEnvelope {
        workspace_id: Some("workspace-main".to_string()),
        branch: BranchRef {
            name: "main".to_string(),
        },
        session_id: SessionId {
            value: "session-budget".to_string(),
        },
        task_id: None,
        actor: Actor::Tool {
            name: tool_name.to_string(),
        },
        kind: payload.kind(),
        references: Vec::new(),
        summary: CompactSummary::new(format!("budget {tool_name}")).expect("summary"),
        payload,
    }
}

fn prepare_change_input_summary(spill: bool) -> String {
    let extra = if spill {
        "x".repeat(96)
    } else {
        "x".repeat(1_024)
    };
    json!({
        "query": "Fix event ordering regressions in replay path and verify corrupted snapshot fallback",
        "entry_files": ["daemon/crates/lattice-core/src/events/reader.rs", "daemon/crates/lattice-core/src/events/writer.rs"],
        "entry_symbols": ["EventReader::stream", "EventWriter::append"],
        "budget": "compact",
        "notes": extra
    })
    .to_string()
}

fn get_context_capsule_input_summary(_spill: bool) -> String {
    json!({
        "query": "Explain how event-log replay, scoping, and corruption detection work together",
        "mode": "full",
        "render": "hybrid",
        "anchors": ["events/mod.rs", "events/reader.rs", "events/compaction.rs"]
    })
    .to_string()
}

fn percentile(samples: &[u128], percentile: usize) -> u128 {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let rank = ((sorted.len() * percentile).div_ceil(100)).saturating_sub(1);
    sorted[rank]
}

fn write_report(report: &BudgetReport) {
    let dir = budget_output_dir();
    fs::create_dir_all(&dir).expect("budget output dir creates");
    let path = dir.join(format!("{}.json", report.case_name));
    let body = serde_json::to_vec_pretty(report).expect("report serializes");
    fs::write(path, body).expect("report writes");
}

fn budget_output_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/event_budget")
        .canonicalize()
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/event_budget")
        })
}

fn init_tracing() {
    let _ = tracing_subscriber::fmt::try_init();
}

fn cpu_model() -> String {
    fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|cpuinfo| {
            cpuinfo
                .lines()
                .find_map(|line| line.strip_prefix("model name\t: ").map(str::to_string))
        })
        .unwrap_or_else(|| "unknown".to_string())
}
