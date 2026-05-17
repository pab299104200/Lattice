use super::{RankedCandidate, SignalKind, SignalScore};

const MAX_REASON_CHARS: usize = 120;
const MAX_SIGNAL_COUNT: usize = 3;

pub fn compact_inclusion_reason(candidate: &RankedCandidate) -> String {
    let mut parts = Vec::new();
    parts.push(compact_text(
        &candidate.candidate.preliminary_inclusion_reason,
        52,
    ));

    let signal_summary = top_signal_summary(&candidate.signal_scores);
    if !signal_summary.is_empty() {
        parts.push(signal_summary);
    }

    clamp_reason(&parts.join("; "))
}

fn top_signal_summary(signal_scores: &[SignalScore]) -> String {
    let mut positive = signal_scores
        .iter()
        .filter(|score| score.raw > 0.0)
        .collect::<Vec<_>>();
    positive.sort_by(|left, right| {
        right
            .weighted
            .partial_cmp(&left.weighted)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| signal_label(left.signal).cmp(signal_label(right.signal)))
    });
    positive.dedup_by(|left, right| left.signal == right.signal);

    let labels = positive
        .into_iter()
        .take(MAX_SIGNAL_COUNT)
        .map(|score| signal_label(score.signal))
        .collect::<Vec<_>>();
    if labels.is_empty() {
        String::new()
    } else {
        format!("signals: {}", labels.join(", "))
    }
}

fn clamp_reason(value: &str) -> String {
    let single_line = value.split_whitespace().collect::<Vec<_>>().join(" ");
    compact_text(&single_line, MAX_REASON_CHARS)
}

fn compact_text(value: &str, limit: usize) -> String {
    let trimmed = value.trim();
    let char_count = trimmed.chars().count();
    if char_count <= limit {
        return trimmed.to_string();
    }
    let keep = limit.saturating_sub(1);
    let prefix = trimmed.chars().take(keep).collect::<String>();
    format!("{prefix}…")
}

fn signal_label(signal: SignalKind) -> &'static str {
    match signal {
        SignalKind::TaskTypeCompatibility => "task fit",
        SignalKind::GraphProximity => "graph proximity",
        SignalKind::ExactIdentifierMatch => "exact id",
        SignalKind::SemanticSimilarity => "semantic match",
        SignalKind::VerificationStatus => "verification",
        SignalKind::Freshness => "freshness",
        SignalKind::Scope => "scope",
        SignalKind::EvidenceStrength => "evidence",
        SignalKind::ContradictionOrSupersessionState => "consistency",
        SignalKind::PastUsefulness => "past usefulness",
        SignalKind::RecentSuccessfulReuse => "recent reuse",
        SignalKind::UserPreferenceCompatibility => "user preference",
        SignalKind::TokenCost => "token cost",
    }
}
