use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const LEDGER_VERSION: u32 = 2;
const RETENTION_DAYS: u64 = 90;
const FOLLOW_THROUGH_WINDOW_SECS: u64 = 60 * 60;
const MAX_SUGGESTED_FILES: usize = 32;
const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone)]
pub(crate) struct ToolCallSource {
    pub(crate) client: String,
    pub(crate) channel: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallRecord {
    pub(crate) session_id: String,
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) tool: String,
    pub(crate) latency_ms: u64,
    /// Files the tool actually returned as relevant. Follow-through is only
    /// credited after the watcher subsequently observes one of these files.
    pub(crate) suggested_files: Vec<String>,
}

#[derive(Debug)]
pub(crate) struct AdoptionMetricsStore {
    path: PathBuf,
    lock: Mutex<StoreState>,
}

#[derive(Debug, Default)]
struct StoreState {
    last_compaction_day: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct AdoptionCounter {
    pub(crate) calls: u64,
    pub(crate) total_latency_ms: u64,
    pub(crate) max_latency_ms: u64,
    pub(crate) follow_through_edits: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AdoptionLedger {
    version: u32,
    days: BTreeMap<String, BTreeMap<String, BTreeMap<String, BTreeMap<String, AdoptionCounter>>>>,
}

impl Default for AdoptionLedger {
    fn default() -> Self {
        Self {
            version: LEDGER_VERSION,
            days: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum AdoptionEvent {
    ToolCall {
        timestamp_secs: u64,
        session_id: String,
        client: String,
        channel: String,
        tool: String,
        latency_ms: u64,
        suggested_files: Vec<String>,
    },
    ObservedEdit {
        timestamp_secs: u64,
        session_id: String,
        file: String,
    },
}

#[derive(Debug, Clone)]
struct PendingAssistance {
    timestamp_secs: u64,
    session_id: String,
    suggested_files: BTreeSet<String>,
    counter_key: CounterKey,
    credited: bool,
}

#[derive(Debug, Clone)]
struct CounterKey {
    day: String,
    client: String,
    channel: String,
    tool: String,
}

impl AdoptionMetricsStore {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        Self {
            path: workspace_root
                .join(".lattice")
                .join("adoption_metrics.jsonl"),
            lock: Mutex::new(StoreState::default()),
        }
    }

    pub(crate) fn record(&self, record: ToolCallRecord) -> Result<()> {
        self.append_event(AdoptionEvent::ToolCall {
            timestamp_secs: now_secs(),
            session_id: clean_key(&record.session_id, "unknown-session"),
            client: clean_key(&record.client, "unknown-client"),
            channel: clean_key(&record.channel, "unknown-channel"),
            tool: clean_key(&record.tool, "unknown-tool"),
            latency_ms: record.latency_ms,
            suggested_files: normalize_suggested_files(record.suggested_files),
        })
    }

    /// Records an actual filesystem change emitted by the watcher. Attribution
    /// is resolved by replaying events, never by caller-supplied prose.
    pub(crate) fn record_observed_edit(&self, session_id: &str, file: &str) -> Result<()> {
        self.append_event(AdoptionEvent::ObservedEdit {
            timestamp_secs: now_secs(),
            session_id: clean_key(session_id, "unknown-session"),
            file: clean_file(file).unwrap_or_default(),
        })
    }

    pub(crate) fn render_table(&self, days: usize) -> Result<String> {
        let ledger = self.read_ledger()?;
        Ok(render_table_from_ledger(&ledger, days))
    }

    pub(crate) fn read_json(&self) -> Result<serde_json::Value> {
        let ledger = self.read_ledger()?;
        serde_json::to_value(ledger).map_err(Into::into)
    }

    fn append_event(&self, event: AdoptionEvent) -> Result<()> {
        let mut state = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))?;
        self.ensure_parent_dir()?;
        self.compact_if_due(&mut state)?;
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("failed to open {}", self.path.display()))?;
        // Keep an event and its delimiter in one append operation. O_APPEND
        // then prevents separate daemon processes from interleaving records.
        file.write_all(line.as_bytes())?;
        file.flush()
            .with_context(|| format!("failed to append {}", self.path.display()))
    }

    fn read_ledger(&self) -> Result<AdoptionLedger> {
        let mut state = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))?;
        self.ensure_parent_dir()?;
        self.compact_if_due(&mut state)?;
        let events = self.read_events_unlocked()?;
        Ok(ledger_from_events(&events))
    }

    fn ensure_parent_dir(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        Ok(())
    }

    fn compact_if_due(&self, state: &mut StoreState) -> Result<()> {
        let today = current_epoch_day();
        if state.last_compaction_day == Some(today) {
            return Ok(());
        }
        state.last_compaction_day = Some(today);
        if !self.path.exists() {
            return Ok(());
        }

        let cutoff = now_secs().saturating_sub(RETENTION_DAYS * SECS_PER_DAY);
        let retained = self
            .read_events_unlocked()?
            .into_iter()
            .filter(|event| event_timestamp(event) >= cutoff)
            .collect::<Vec<_>>();
        let temp = self.path.with_extension("jsonl.compacting");
        let mut file = fs::File::create(&temp)
            .with_context(|| format!("failed to create {}", temp.display()))?;
        for event in retained {
            serde_json::to_writer(&mut file, &event)?;
            file.write_all(b"\n")?;
        }
        file.flush()?;
        fs::rename(&temp, &self.path).with_context(|| {
            format!(
                "failed to replace adoption metrics {} with compacted ledger",
                self.path.display()
            )
        })
    }

    fn read_events_unlocked(&self) -> Result<Vec<AdoptionEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let mut events = Vec::new();
        for (line_number, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to parse adoption metrics {} line {}",
                    self.path.display(),
                    line_number + 1
                )
            })?;
            events.push(event);
        }
        Ok(events)
    }
}

pub(crate) fn source_from_arguments(
    args: &serde_json::Value,
    default_client: &str,
    default_channel: &str,
) -> ToolCallSource {
    ToolCallSource {
        client: args
            .get("_lattice_client")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_client)
            .to_string(),
        channel: args
            .get("_lattice_channel")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .unwrap_or(default_channel)
            .to_string(),
    }
}

/// Extracts only file references explicitly present in a successful tool
/// result. Free-form query text is deliberately ignored: follow-through must
/// be tied to files Lattice returned, not to strings the caller happened to
/// send us.
pub(crate) fn suggested_files_from_tool_result(
    tool: &str,
    result: &serde_json::Value,
) -> Vec<String> {
    if !matches!(
        tool,
        "context" | "impact" | "get_context_capsule" | "impact_from_diff"
    ) {
        return Vec::new();
    }
    let mut files = BTreeSet::new();
    collect_response_files(result, &mut files);
    files.into_iter().take(MAX_SUGGESTED_FILES).collect()
}

pub(crate) fn render_metrics_for_workspace(
    workspace: &Path,
    days: usize,
    json: bool,
) -> Result<String> {
    let store = AdoptionMetricsStore::new(workspace);
    if json {
        return Ok(serde_json::to_string_pretty(&store.read_json()?)?);
    }
    store.render_table(days)
}

fn ledger_from_events(events: &[AdoptionEvent]) -> AdoptionLedger {
    let mut ledger = AdoptionLedger::default();
    let mut pending = Vec::new();
    for event in events {
        match event {
            AdoptionEvent::ToolCall {
                timestamp_secs,
                session_id,
                client,
                channel,
                tool,
                latency_ms,
                suggested_files,
            } => {
                let day = date_key_from_epoch_day(*timestamp_secs / SECS_PER_DAY);
                let counter = ledger
                    .days
                    .entry(day.clone())
                    .or_default()
                    .entry(client.clone())
                    .or_default()
                    .entry(channel.clone())
                    .or_default()
                    .entry(tool.clone())
                    .or_default();
                counter.calls += 1;
                counter.total_latency_ms += latency_ms;
                counter.max_latency_ms = counter.max_latency_ms.max(*latency_ms);
                if matches!(tool.as_str(), "context" | "impact") && !suggested_files.is_empty() {
                    pending.push(PendingAssistance {
                        timestamp_secs: *timestamp_secs,
                        session_id: session_id.clone(),
                        suggested_files: suggested_files.iter().cloned().collect(),
                        counter_key: CounterKey {
                            day,
                            client: client.clone(),
                            channel: channel.clone(),
                            tool: tool.clone(),
                        },
                        credited: false,
                    });
                }
            }
            AdoptionEvent::ObservedEdit {
                timestamp_secs,
                session_id,
                file,
            } => {
                let Some(candidate) = pending.iter_mut().rev().find(|candidate| {
                    !candidate.credited
                        && candidate.session_id == *session_id
                        && *timestamp_secs >= candidate.timestamp_secs
                        && timestamp_secs.saturating_sub(candidate.timestamp_secs)
                            <= FOLLOW_THROUGH_WINDOW_SECS
                        && candidate.suggested_files.contains(file)
                }) else {
                    continue;
                };
                candidate.credited = true;
                if let Some(counter) = ledger
                    .days
                    .get_mut(&candidate.counter_key.day)
                    .and_then(|clients| clients.get_mut(&candidate.counter_key.client))
                    .and_then(|channels| channels.get_mut(&candidate.counter_key.channel))
                    .and_then(|tools| tools.get_mut(&candidate.counter_key.tool))
                {
                    counter.follow_through_edits += 1;
                }
            }
        }
    }
    ledger
}

fn event_timestamp(event: &AdoptionEvent) -> u64 {
    match event {
        AdoptionEvent::ToolCall { timestamp_secs, .. }
        | AdoptionEvent::ObservedEdit { timestamp_secs, .. } => *timestamp_secs,
    }
}

fn normalize_suggested_files(files: Vec<String>) -> Vec<String> {
    files
        .into_iter()
        .filter_map(|file| clean_file(&file))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .take(MAX_SUGGESTED_FILES)
        .collect()
}

fn collect_response_files(value: &serde_json::Value, files: &mut BTreeSet<String>) {
    match value {
        serde_json::Value::Object(object) => {
            for (key, child) in object {
                match (key.as_str(), child) {
                    (
                        "file" | "from_file" | "to_file" | "path" | "f",
                        serde_json::Value::String(file),
                    ) => {
                        if let Some(file) = clean_file(file) {
                            files.insert(file);
                        }
                    }
                    ("files", serde_json::Value::Array(values)) => {
                        for value in values {
                            if let Some(file) = value.as_str().and_then(clean_file) {
                                files.insert(file);
                            }
                        }
                    }
                    ("text", serde_json::Value::String(text)) => {
                        if let Ok(payload) = serde_json::from_str(text) {
                            collect_response_files(&payload, files);
                        }
                    }
                    _ => collect_response_files(child, files),
                }
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_response_files(value, files);
            }
        }
        _ => {}
    }
}

fn clean_file(file: &str) -> Option<String> {
    let file = file.trim().replace('\\', "/");
    (!file.is_empty()
        && !file.starts_with('/')
        && !file
            .split('/')
            .any(|segment| segment == ".." || segment.is_empty()))
    .then_some(file)
}

fn render_table_from_ledger(ledger: &AdoptionLedger, days: usize) -> String {
    let mut rows = Vec::new();
    let min_day = current_epoch_day().saturating_sub(days.saturating_sub(1) as u64);
    for (day, clients) in &ledger.days {
        let day_number = day_number_from_key(day);
        if day_number.is_some_and(|value| value < min_day) {
            continue;
        }
        for (client, channels) in clients {
            for (channel, tools) in channels {
                for (tool, counter) in tools {
                    let avg = if counter.calls == 0 {
                        0
                    } else {
                        counter.total_latency_ms / counter.calls
                    };
                    let rate = if counter.calls == 0 {
                        0.0
                    } else {
                        counter.follow_through_edits as f64 / counter.calls as f64
                    };
                    rows.push(format!(
                        "{day} | {client} | {channel} | {tool} | {} | {avg} | {} | {:.0}%",
                        counter.calls,
                        counter.max_latency_ms,
                        rate * 100.0
                    ));
                }
            }
        }
    }
    let mut out =
        String::from("day | client | channel | tool | calls | avg_ms | max_ms | follow_through\n");
    out.push_str("--- | --- | --- | --- | ---: | ---: | ---: | ---:\n");
    if rows.is_empty() {
        out.push_str("_no adoption metrics recorded_\n");
    } else {
        for row in rows {
            out.push_str(&row);
            out.push('\n');
        }
    }
    out
}

fn clean_key(value: &str, fallback: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        fallback.to_string()
    } else {
        trimmed
            .chars()
            .map(|ch| if ch.is_control() { '_' } else { ch })
            .collect()
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn current_epoch_day() -> u64 {
    now_secs() / SECS_PER_DAY
}

fn day_number_from_key(day: &str) -> Option<u64> {
    if let Some(raw) = day.strip_prefix("day-") {
        return raw.parse().ok();
    }
    let mut parts = day.split('-');
    let year = parts.next()?.parse::<i32>().ok()?;
    let month = parts.next()?.parse::<u32>().ok()?;
    let day = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(year, month, day)? as u64)
}

fn date_key_from_epoch_day(epoch_day: u64) -> String {
    let (year, month, day) = civil_from_days(epoch_day as i64);
    format!("{year:04}-{month:02}-{day:02}")
}

fn days_from_civil(year: i32, month: u32, day: u32) -> Option<i64> {
    let year = year as i64 - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let month = month as i64;
    let day = day as i64;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    if doy < 0 || doy > 365 {
        return None;
    }
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

fn civil_from_days(days_since_epoch: i64) -> (i32, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = mp + if mp < 10 { 3 } else { -9 };
    let year = y + if m <= 2 { 1 } else { 0 };
    (year as i32, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watcher_edit_credits_only_same_session_and_suggested_file() {
        let events = vec![
            call(10, "session-a", "context", &["src/auth.rs"]),
            call(11, "session-b", "impact", &["src/auth.rs"]),
            edit(12, "session-b", "src/auth.rs"),
            edit(13, "session-a", "src/other.rs"),
            edit(14, "session-a", "src/auth.rs"),
        ];
        let ledger = ledger_from_events(&events);
        let tools = today_tools(&ledger);

        assert_eq!(tools["context"].follow_through_edits, 1);
        assert_eq!(tools["impact"].follow_through_edits, 1);
    }

    #[test]
    fn observed_edit_does_not_credit_outside_bounded_session_window() {
        let events = vec![
            call(10, "session-a", "context", &["src/auth.rs"]),
            edit(
                10 + FOLLOW_THROUGH_WINDOW_SECS + 1,
                "session-a",
                "src/auth.rs",
            ),
        ];
        let ledger = ledger_from_events(&events);
        assert_eq!(today_tools(&ledger)["context"].follow_through_edits, 0);
    }

    #[test]
    fn append_only_records_and_compacts_entries_past_retention() {
        let root = unique_root("retention");
        let store = AdoptionMetricsStore::new(&root);
        let old = AdoptionEvent::ObservedEdit {
            timestamp_secs: now_secs().saturating_sub((RETENTION_DAYS + 1) * SECS_PER_DAY),
            session_id: "old".to_string(),
            file: "src/old.rs".to_string(),
        };
        store.ensure_parent_dir().expect("metrics parent");
        fs::write(
            &store.path,
            format!("{}\n", serde_json::to_string(&old).unwrap()),
        )
        .expect("seed old event");
        store
            .record(call_record("session-a", "context", &["src/auth.rs"]))
            .expect("append event");

        let contents = fs::read_to_string(&store.path).expect("read log");
        assert_eq!(
            contents.lines().count(),
            1,
            "old record should be compacted"
        );
        assert!(contents.contains("tool_call"));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn hidden_arguments_override_default_source() {
        let source = source_from_arguments(
            &serde_json::json!({
                "_lattice_client": "lattice-cli",
                "_lattice_channel": "cli"
            }),
            "mcp",
            "mcp",
        );
        assert_eq!(source.client, "lattice-cli");
        assert_eq!(source.channel, "cli");
    }

    #[test]
    fn result_file_extraction_ignores_caller_text_and_keeps_returned_files() {
        let files = suggested_files_from_tool_result(
            "context",
            &serde_json::json!({
                "content": [{
                    "text": "{\"pivots\":[{\"file\":\"src/auth.rs\"}],\"query\":\"do not credit src/untrusted.rs\"}"
                }]
            }),
        );

        assert_eq!(files, vec!["src/auth.rs".to_string()]);
    }

    fn call(timestamp: u64, session: &str, tool: &str, files: &[&str]) -> AdoptionEvent {
        AdoptionEvent::ToolCall {
            timestamp_secs: timestamp,
            session_id: session.to_string(),
            client: "codex".to_string(),
            channel: "mcp".to_string(),
            tool: tool.to_string(),
            latency_ms: 10,
            suggested_files: files.iter().map(ToString::to_string).collect(),
        }
    }

    fn edit(timestamp: u64, session: &str, file: &str) -> AdoptionEvent {
        AdoptionEvent::ObservedEdit {
            timestamp_secs: timestamp,
            session_id: session.to_string(),
            file: file.to_string(),
        }
    }

    fn call_record(session: &str, tool: &str, files: &[&str]) -> ToolCallRecord {
        ToolCallRecord {
            session_id: session.to_string(),
            client: "codex".to_string(),
            channel: "mcp".to_string(),
            tool: tool.to_string(),
            latency_ms: 10,
            suggested_files: files.iter().map(ToString::to_string).collect(),
        }
    }

    fn today_tools(ledger: &AdoptionLedger) -> &BTreeMap<String, AdoptionCounter> {
        ledger
            .days
            .get(&date_key_from_epoch_day(0))
            .expect("event day")
            .get("codex")
            .expect("client")
            .get("mcp")
            .expect("channel")
    }

    fn unique_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "lattice-adoption-{name}-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }
}
