use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

const LEDGER_VERSION: u32 = 1;
const SECS_PER_DAY: u64 = 86_400;

#[derive(Debug, Clone)]
pub(crate) struct ToolCallSource {
    pub(crate) client: String,
    pub(crate) channel: String,
}

#[derive(Debug, Clone)]
pub(crate) struct ToolCallRecord {
    pub(crate) client: String,
    pub(crate) channel: String,
    pub(crate) tool: String,
    pub(crate) latency_ms: u64,
    pub(crate) follow_through_edit: bool,
}

#[derive(Debug)]
pub(crate) struct AdoptionMetricsStore {
    path: PathBuf,
    lock: Mutex<()>,
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

impl AdoptionMetricsStore {
    pub(crate) fn new(workspace_root: &Path) -> Self {
        Self {
            path: workspace_root
                .join(".lattice")
                .join("adoption_metrics.json"),
            lock: Mutex::new(()),
        }
    }

    pub(crate) fn record(&self, record: ToolCallRecord) -> Result<()> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))?;
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let mut ledger = self.read_ledger_unlocked()?;
        let day = today_key();
        let client = clean_key(&record.client, "unknown-client");
        let channel = clean_key(&record.channel, "unknown-channel");
        let tool = clean_key(&record.tool, "unknown-tool");
        let day_entry = ledger.days.entry(day).or_default();
        let counter = day_entry
            .entry(client.clone())
            .or_default()
            .entry(channel.clone())
            .or_default()
            .entry(tool.clone())
            .or_default();
        counter.calls += 1;
        counter.total_latency_ms += record.latency_ms;
        counter.max_latency_ms = counter.max_latency_ms.max(record.latency_ms);
        if record.follow_through_edit {
            counter.follow_through_edits += 1;
            credit_prior_assistance_follow_through(day_entry, &client, &channel);
        }
        let text = serde_json::to_string_pretty(&ledger)?;
        fs::write(&self.path, text)
            .with_context(|| format!("failed to write {}", self.path.display()))
    }

    pub(crate) fn render_table(&self, days: usize) -> Result<String> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))?;
        let ledger = self.read_ledger_unlocked()?;
        Ok(render_table_from_ledger(&ledger, days))
    }

    pub(crate) fn read_json(&self) -> Result<serde_json::Value> {
        let _guard = self
            .lock
            .lock()
            .map_err(|_| anyhow::anyhow!("adoption metrics lock poisoned"))?;
        let ledger = self.read_ledger_unlocked()?;
        serde_json::to_value(ledger).map_err(Into::into)
    }

    fn read_ledger_unlocked(&self) -> Result<AdoptionLedger> {
        if !self.path.exists() {
            return Ok(AdoptionLedger::default());
        }
        let text = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))?;
        let mut ledger: AdoptionLedger = serde_json::from_str(&text)
            .with_context(|| format!("failed to parse {}", self.path.display()))?;
        if ledger.version != LEDGER_VERSION {
            ledger.version = LEDGER_VERSION;
        }
        Ok(ledger)
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

pub(crate) fn follow_through_from_arguments(tool: &str, args: &serde_json::Value) -> bool {
    if tool != "remember" && tool != "record_workflow_outcome" {
        return false;
    }
    let content = args
        .get("content")
        .or_else(|| args.get("summary"))
        .or_else(|| args.get("task"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    content.contains("Session edited files:")
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

fn credit_prior_assistance_follow_through(
    day_entry: &mut BTreeMap<String, BTreeMap<String, BTreeMap<String, AdoptionCounter>>>,
    client: &str,
    channel: &str,
) {
    let Some(channel_entry) = day_entry
        .get_mut(client)
        .and_then(|channels| channels.get_mut(channel))
    else {
        return;
    };
    for tool in ["context", "impact"] {
        if let Some(counter) = channel_entry.get_mut(tool) {
            if counter.follow_through_edits < counter.calls {
                counter.follow_through_edits += 1;
            }
        }
    }
}

fn today_key() -> String {
    date_key_from_epoch_day(current_epoch_day())
}

fn current_epoch_day() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        / SECS_PER_DAY
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
    fn ledger_records_client_channel_tool_counts() {
        let root = std::env::temp_dir().join(format!("lattice-adoption-{}", std::process::id()));
        let store = AdoptionMetricsStore::new(&root);
        store
            .record(ToolCallRecord {
                client: "claude-code".to_string(),
                channel: "hook".to_string(),
                tool: "context".to_string(),
                latency_ms: 42,
                follow_through_edit: true,
            })
            .expect("record");
        let table = store.render_table(14).expect("render");
        assert!(table.contains("claude-code | hook | context | 1 | 42 | 42 | 100%"));
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn edited_file_reports_credit_prior_context_and_impact_calls() {
        let root = std::env::temp_dir().join(format!(
            "lattice-adoption-follow-through-{}",
            std::process::id()
        ));
        let store = AdoptionMetricsStore::new(&root);
        for tool in ["context", "impact"] {
            store
                .record(ToolCallRecord {
                    client: "claude-code".to_string(),
                    channel: "hook".to_string(),
                    tool: tool.to_string(),
                    latency_ms: 10,
                    follow_through_edit: false,
                })
                .expect("record assistance");
        }
        store
            .record(ToolCallRecord {
                client: "claude-code".to_string(),
                channel: "hook".to_string(),
                tool: "remember".to_string(),
                latency_ms: 5,
                follow_through_edit: true,
            })
            .expect("record edited files");

        let table = store.render_table(14).expect("render");
        assert!(table.contains("claude-code | hook | context | 1 | 10 | 10 | 100%"));
        assert!(table.contains("claude-code | hook | impact | 1 | 10 | 10 | 100%"));
        assert!(table.contains("claude-code | hook | remember | 1 | 5 | 5 | 100%"));
        let _ = std::fs::remove_dir_all(root);
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
    fn date_keys_round_trip_to_epoch_days() {
        let today = current_epoch_day();
        let key = date_key_from_epoch_day(today);
        assert_eq!(day_number_from_key(&key), Some(today));
    }
}
