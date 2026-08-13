//! Pure, bounded consumers of published Git-intelligence snapshots.
//!
//! This module deliberately knows nothing about Git traversal, storage, graph
//! queries, or daemon freshness tracking. Callers provide a snapshot together
//! with its publication freshness; every consumer then applies the same
//! completeness gate before exposing history-derived behavior.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::git_intelligence::{
    FileHistorySignal, GitIntelligenceSnapshot, MissingCoChangePartner, SymbolHistorySignal,
};

/// Maximum number of missing co-change partners exposed by an impact response.
pub const MAX_MISSING_CO_CHANGE_PARTNERS: usize = 10;
/// A co-change advisory needs at least this many distinct commits.
pub const MIN_CO_CHANGE_PARTNER_COMMITS: u32 = 2;
/// Maximum number of files included in one non-blocking hotspot warning.
pub const MAX_HOTSPOT_WARNINGS: usize = 5;
const PER_MILLE_SCALE: u64 = 1_000;

/// Whether a history snapshot may affect a consumer response.
///
/// `Stale` wins over `Degraded`: a stale snapshot must not be used even when
/// its aggregation report was complete when it was published.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitIntelligenceAvailability {
    Unavailable,
    Stale,
    Degraded,
    Available,
}

/// A snapshot paired with the publication freshness known by the caller.
///
/// The borrowed form prevents a consumer from accidentally retaining or
/// mutating the published aggregate while constructing an answer.
#[derive(Debug, Clone, Copy)]
pub struct GitIntelligenceView<'a> {
    snapshot: Option<&'a GitIntelligenceSnapshot>,
    availability: GitIntelligenceAvailability,
}

impl<'a> GitIntelligenceView<'a> {
    /// Represents a missing, unreadable, or otherwise unavailable snapshot.
    pub const fn unavailable() -> Self {
        Self {
            snapshot: None,
            availability: GitIntelligenceAvailability::Unavailable,
        }
    }

    /// Classifies a published snapshot using its aggregation report and the
    /// caller's freshness evidence.
    pub fn from_snapshot(snapshot: &'a GitIntelligenceSnapshot, is_fresh: bool) -> Self {
        let availability = if !is_fresh {
            GitIntelligenceAvailability::Stale
        } else if snapshot.report.is_complete() {
            GitIntelligenceAvailability::Available
        } else {
            GitIntelligenceAvailability::Degraded
        };
        Self {
            snapshot: Some(snapshot),
            availability,
        }
    }

    pub const fn availability(self) -> GitIntelligenceAvailability {
        self.availability
    }

    /// Returns the snapshot only when history signals are safe to consume.
    pub const fn usable_snapshot(self) -> Option<&'a GitIntelligenceSnapshot> {
        match self.availability {
            GitIntelligenceAvailability::Available => self.snapshot,
            GitIntelligenceAvailability::Unavailable
            | GitIntelligenceAvailability::Stale
            | GitIntelligenceAvailability::Degraded => None,
        }
    }

    /// Metadata suitable for a response even when signals are suppressed.
    pub fn metadata(self) -> GitIntelligenceMetadata {
        let Some(snapshot) = self.snapshot else {
            return GitIntelligenceMetadata {
                availability: self.availability,
                window_commits: 0,
                head_commit_id: None,
            };
        };
        GitIntelligenceMetadata {
            availability: self.availability,
            window_commits: u32::try_from(snapshot.processed_commits.len()).unwrap_or(u32::MAX),
            head_commit_id: snapshot.head_commit_id().map(str::to_owned),
        }
    }
}

/// Minimal history snapshot metadata shared by consumer outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIntelligenceMetadata {
    pub availability: GitIntelligenceAvailability,
    pub window_commits: u32,
    pub head_commit_id: Option<String>,
}

/// One bounded, normalized count contribution to secondary retrieval ranking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedHistoryCount {
    pub observed_commits: u32,
    pub normalized_per_mille: u16,
}

/// History-derived evidence that may be displayed with a retrieval result.
///
/// This is evidence only: it does not prescribe a combined score or permit
/// history to admit a candidate that primary retrieval rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecondaryRankingEvidence {
    pub metadata: GitIntelligenceMetadata,
    pub file_hotspot: Option<NormalizedHistoryCount>,
    pub symbol_hotspot: Option<NormalizedHistoryCount>,
    pub bug_fix_density_per_mille: Option<u16>,
}

/// Returns bounded secondary ranking evidence for an already eligible result.
///
/// Exact stable-symbol matching is intentional. File and symbol hotness are
/// normalized to the active window, so the returned values are always in
/// `0..=1000` and cannot dominate a caller's primary relevance score by scale.
pub fn secondary_ranking_evidence(
    view: GitIntelligenceView<'_>,
    file_path: &str,
    stable_symbol: Option<&str>,
) -> Option<SecondaryRankingEvidence> {
    let snapshot = view.usable_snapshot()?;
    let window_commits = window_commits(snapshot)?;
    if window_commits == 0 {
        return None;
    }

    let file = snapshot.file(file_path);
    let symbol = stable_symbol.and_then(|symbol| snapshot.symbol(symbol));
    let file_hotspot = file.and_then(|signal| normalized_hotspot(signal, window_commits));
    let symbol_hotspot =
        symbol.and_then(|signal| normalized_symbol_hotspot(signal, window_commits));
    let bug_fix_density_per_mille = file_hotspot
        .as_ref()
        .and_then(|_| file.and_then(valid_bug_fix_density));
    if file_hotspot.is_none() && symbol_hotspot.is_none() && bug_fix_density_per_mille.is_none() {
        return None;
    }

    Some(SecondaryRankingEvidence {
        metadata: view.metadata(),
        file_hotspot,
        symbol_hotspot,
        bug_fix_density_per_mille,
    })
}

/// A graph-dependent impact candidate plus the stable keys required to order it.
///
/// Call this only within one caller-defined graph-distance and severity tier.
/// `stable_key` is the caller's existing deterministic final tie-break.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImpactCandidate<T> {
    pub candidate: T,
    pub stable_key: String,
    pub file_path: Option<String>,
    pub stable_symbol: Option<String>,
}

/// Orders candidates only within the supplied caller-defined impact tier.
///
/// Exact symbol hotspot descends first, then file hotspot, then `stable_key`.
/// When history is unavailable every history score is zero, so this reduces to
/// the caller's deterministic tie-break without inventing an impact edge.
pub fn order_impact_within_tier<T>(
    view: GitIntelligenceView<'_>,
    candidates: &mut [ImpactCandidate<T>],
) {
    let snapshot = view.usable_snapshot();
    let window = snapshot.and_then(window_commits);
    candidates.sort_by(|left, right| {
        let left_symbol = impact_symbol_hotspot(snapshot, window, left.stable_symbol.as_deref());
        let right_symbol = impact_symbol_hotspot(snapshot, window, right.stable_symbol.as_deref());
        let left_file = impact_file_hotspot(snapshot, window, left.file_path.as_deref());
        let right_file = impact_file_hotspot(snapshot, window, right.file_path.as_deref());
        right_symbol
            .cmp(&left_symbol)
            .then_with(|| right_file.cmp(&left_file))
            .then_with(|| left.stable_key.cmp(&right.stable_key))
    });
}

/// The separately surfaced co-change advisory for an impact response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImpactHistoryAdvisory {
    pub metadata: GitIntelligenceMetadata,
    pub missing_cochange_partners: Vec<MissingCoChangePartnerAdvisory>,
}

/// A changed path's historically co-changed partner missing from the current diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingCoChangePartnerAdvisory {
    pub source_path: String,
    pub partner_path: String,
    pub commit_count: u32,
    pub window_commits: u32,
    pub head_commit_id: Option<String>,
}

/// Produces the deterministic, capped co-change advisory for a current diff.
///
/// A non-available view returns an empty section with its explicit availability
/// metadata. Invalid and duplicate diff paths are rejected or deduplicated by
/// the snapshot's canonical lookup before ranking.
pub fn missing_cochange_partners<I, S>(
    view: GitIntelligenceView<'_>,
    changed_paths: I,
) -> ImpactHistoryAdvisory
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let metadata = view.metadata();
    let Some(snapshot) = view.usable_snapshot() else {
        return ImpactHistoryAdvisory {
            metadata,
            missing_cochange_partners: Vec::new(),
        };
    };
    let partners = snapshot.missing_co_change_partners(
        changed_paths,
        MIN_CO_CHANGE_PARTNER_COMMITS,
        MAX_MISSING_CO_CHANGE_PARTNERS,
    );
    ImpactHistoryAdvisory {
        missing_cochange_partners: partners
            .into_iter()
            .filter(|partner| partner.commit_count <= metadata.window_commits)
            .map(|partner| advisory_partner(partner, &metadata))
            .collect(),
        metadata,
    }
}

/// One top-decile hotspot warning safe for a non-blocking hook response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HotspotWarning {
    pub path: String,
    pub hotspot_score: u32,
    pub window_commits: u32,
    pub head_commit_id: Option<String>,
}

/// Selects at most five unique edited paths at or above the file top-decile.
///
/// The caller is responsible for passing only successful, workspace-scoped edit
/// paths. This function performs no filesystem access and suppresses warnings
/// for unavailable, stale, degraded, empty, or corrupt history.
pub fn select_hotspot_warnings<I, S>(view: GitIntelligenceView<'_>, paths: I) -> Vec<HotspotWarning>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let Some(snapshot) = view.usable_snapshot() else {
        return Vec::new();
    };
    let Some(window_commits) = window_commits(snapshot) else {
        return Vec::new();
    };
    let Some(cutoff) = snapshot.top_decile_hotspot_cutoff() else {
        return Vec::new();
    };

    let mut warnings = BTreeSet::new();
    for path in paths {
        let Some(file) = snapshot.file(path.as_ref()) else {
            continue;
        };
        if file.hotspot_score <= window_commits && file.hotspot_score >= cutoff {
            warnings.insert((file.path.clone(), file.hotspot_score));
        }
    }
    let mut warnings: Vec<_> = warnings.into_iter().collect();
    warnings.sort_unstable_by(|(left_path, left_score), (right_path, right_score)| {
        right_score
            .cmp(left_score)
            .then_with(|| left_path.cmp(right_path))
    });
    warnings.truncate(MAX_HOTSPOT_WARNINGS);
    warnings
        .into_iter()
        .map(|(path, hotspot_score)| HotspotWarning {
            path,
            hotspot_score,
            window_commits,
            head_commit_id: snapshot.head_commit_id().map(str::to_owned),
        })
        .collect()
}

fn window_commits(snapshot: &GitIntelligenceSnapshot) -> Option<u32> {
    u32::try_from(snapshot.processed_commits.len())
        .ok()
        .filter(|count| *count > 0)
}

fn normalized_hotspot(
    signal: &FileHistorySignal,
    window_commits: u32,
) -> Option<NormalizedHistoryCount> {
    normalize_count(signal.hotspot_score, window_commits)
}

fn normalized_symbol_hotspot(
    signal: &SymbolHistorySignal,
    window_commits: u32,
) -> Option<NormalizedHistoryCount> {
    normalize_count(signal.hotspot_score, window_commits)
}

fn normalize_count(observed_commits: u32, window_commits: u32) -> Option<NormalizedHistoryCount> {
    if observed_commits > window_commits || window_commits == 0 {
        return None;
    }
    let normalized = u64::from(observed_commits)
        .checked_mul(PER_MILLE_SCALE)?
        .checked_div(u64::from(window_commits))?;
    Some(NormalizedHistoryCount {
        observed_commits,
        normalized_per_mille: u16::try_from(normalized).ok()?,
    })
}

fn valid_bug_fix_density(signal: &FileHistorySignal) -> Option<u16> {
    (signal.bug_fix_density_per_mille <= PER_MILLE_SCALE as u16)
        .then_some(signal.bug_fix_density_per_mille)
}

fn impact_symbol_hotspot(
    snapshot: Option<&GitIntelligenceSnapshot>,
    window: Option<u32>,
    symbol: Option<&str>,
) -> u32 {
    match (snapshot, window, symbol) {
        (Some(snapshot), Some(window), Some(symbol)) => snapshot
            .symbol(symbol)
            .filter(|signal| signal.hotspot_score <= window)
            .map_or(0, |signal| signal.hotspot_score),
        _ => 0,
    }
}

fn impact_file_hotspot(
    snapshot: Option<&GitIntelligenceSnapshot>,
    window: Option<u32>,
    path: Option<&str>,
) -> u32 {
    match (snapshot, window, path) {
        (Some(snapshot), Some(window), Some(path)) => snapshot
            .file(path)
            .filter(|signal| signal.hotspot_score <= window)
            .map_or(0, |signal| signal.hotspot_score),
        _ => 0,
    }
}

fn advisory_partner(
    partner: MissingCoChangePartner,
    metadata: &GitIntelligenceMetadata,
) -> MissingCoChangePartnerAdvisory {
    MissingCoChangePartnerAdvisory {
        source_path: partner.source_path,
        partner_path: partner.partner_path,
        commit_count: partner.commit_count,
        window_commits: metadata.window_commits,
        head_commit_id: metadata.head_commit_id.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_intelligence::{
        CoChangeSignal, FileHistorySignal, GitIntelligenceSnapshot, GitMiningLimits,
        GitMiningReport, SymbolHistorySignal, AGGREGATION_VERSION,
    };

    fn snapshot() -> GitIntelligenceSnapshot {
        GitIntelligenceSnapshot {
            processed_commits: vec!["head".into(), "two".into(), "three".into(), "four".into()],
            files: vec![
                file("src/a.rs", 4, 750),
                file("src/b.rs", 3, 500),
                file("src/c.rs", 2, 0),
                file("src/d.rs", 1, 0),
            ],
            symbols: vec![symbol("a::high", 4), symbol("b::medium", 3)],
            co_changes: vec![
                co_change("src/a.rs", "src/missing.rs", 3),
                co_change("src/b.rs", "src/also-missing.rs", 2),
                co_change("src/c.rs", "src/one-off.rs", 1),
            ],
            report: report(),
        }
    }

    fn report() -> GitMiningReport {
        GitMiningReport {
            aggregation_version: AGGREGATION_VERSION,
            limits: GitMiningLimits::default(),
            samples_seen: 4,
            sampled_commits: 4,
            included_commits: 4,
            duplicate_commits: 0,
            invalid_commit_ids: 0,
            invalid_path_entries: 0,
            path_overflow_commits: 0,
            symbol_overflow_commits: 0,
            co_change_width_exclusions: 0,
            co_changes_complete: true,
        }
    }

    fn file(path: &str, hotspot_score: u32, bug_fix_density_per_mille: u16) -> FileHistorySignal {
        FileHistorySignal {
            path: path.into(),
            hotspot_score,
            bug_fix_commits: 0,
            bug_fix_density_per_mille,
            author_count: 0,
            top_author_share_per_mille: None,
            bus_factor: None,
        }
    }

    fn symbol(symbol: &str, hotspot_score: u32) -> SymbolHistorySignal {
        SymbolHistorySignal {
            symbol: symbol.into(),
            hotspot_score,
            bug_fix_commits: 0,
            author_count: 0,
        }
    }

    fn co_change(left_path: &str, right_path: &str, commit_count: u32) -> CoChangeSignal {
        CoChangeSignal {
            left_path: left_path.into(),
            right_path: right_path.into(),
            commit_count,
        }
    }

    #[test]
    fn availability_requires_a_fresh_complete_snapshot() {
        let mut degraded = snapshot();
        degraded.report.invalid_path_entries = 1;
        assert_eq!(
            GitIntelligenceView::unavailable().availability(),
            GitIntelligenceAvailability::Unavailable
        );
        assert_eq!(
            GitIntelligenceView::from_snapshot(&snapshot(), false).availability(),
            GitIntelligenceAvailability::Stale
        );
        assert_eq!(
            GitIntelligenceView::from_snapshot(&degraded, true).availability(),
            GitIntelligenceAvailability::Degraded
        );
        assert_eq!(
            GitIntelligenceView::from_snapshot(&snapshot(), true).availability(),
            GitIntelligenceAvailability::Available
        );
    }

    #[test]
    fn secondary_evidence_is_exact_normalized_and_suppressed_when_unusable() {
        let history = snapshot();
        let evidence = secondary_ranking_evidence(
            GitIntelligenceView::from_snapshot(&history, true),
            "src/a.rs",
            Some("a::high"),
        )
        .expect("available history evidence");
        assert_eq!(evidence.file_hotspot.unwrap().normalized_per_mille, 1000);
        assert_eq!(evidence.symbol_hotspot.unwrap().observed_commits, 4);
        assert_eq!(evidence.bug_fix_density_per_mille, Some(750));
        assert_eq!(evidence.metadata.head_commit_id.as_deref(), Some("head"));
        assert!(secondary_ranking_evidence(
            GitIntelligenceView::from_snapshot(&history, false),
            "src/a.rs",
            Some("a::high"),
        )
        .is_none());
    }

    #[test]
    fn impact_ordering_only_breaks_ties_inside_its_supplied_tier() {
        let history = snapshot();
        let mut candidates = vec![
            ImpactCandidate {
                candidate: 1,
                stable_key: "z".into(),
                file_path: Some("src/b.rs".into()),
                stable_symbol: None,
            },
            ImpactCandidate {
                candidate: 2,
                stable_key: "a".into(),
                file_path: Some("src/a.rs".into()),
                stable_symbol: Some("a::high".into()),
            },
            ImpactCandidate {
                candidate: 3,
                stable_key: "b".into(),
                file_path: Some("src/a.rs".into()),
                stable_symbol: None,
            },
        ];
        order_impact_within_tier(
            GitIntelligenceView::from_snapshot(&history, true),
            &mut candidates,
        );
        assert_eq!(
            candidates
                .into_iter()
                .map(|candidate| candidate.candidate)
                .collect::<Vec<_>>(),
            vec![2, 3, 1]
        );

        let mut degraded_candidates = vec![
            ImpactCandidate {
                candidate: 1,
                stable_key: "z".into(),
                file_path: Some("src/a.rs".into()),
                stable_symbol: Some("a::high".into()),
            },
            ImpactCandidate {
                candidate: 2,
                stable_key: "a".into(),
                file_path: None,
                stable_symbol: None,
            },
        ];
        order_impact_within_tier(
            GitIntelligenceView::from_snapshot(&history, false),
            &mut degraded_candidates,
        );
        assert_eq!(
            degraded_candidates
                .into_iter()
                .map(|candidate| candidate.candidate)
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
    }

    #[test]
    fn missing_partners_are_minimum_supported_capped_and_include_metadata() {
        let history = snapshot();
        let advisory = missing_cochange_partners(
            GitIntelligenceView::from_snapshot(&history, true),
            ["src/b.rs", "src/a.rs", "src/a.rs", "../outside.rs"],
        );
        assert_eq!(
            advisory.metadata.availability,
            GitIntelligenceAvailability::Available
        );
        assert_eq!(advisory.missing_cochange_partners.len(), 2);
        assert_eq!(
            advisory.missing_cochange_partners[0].partner_path,
            "src/missing.rs"
        );
        assert_eq!(advisory.missing_cochange_partners[0].window_commits, 4);
        assert_eq!(
            advisory.missing_cochange_partners[1].partner_path,
            "src/also-missing.rs"
        );
        assert!(missing_cochange_partners(
            GitIntelligenceView::from_snapshot(&history, false),
            ["src/a.rs"]
        )
        .missing_cochange_partners
        .is_empty());
    }

    #[test]
    fn warnings_are_top_decile_deduplicated_and_never_emitted_from_degraded_history() {
        let history = snapshot();
        let warnings = select_hotspot_warnings(
            GitIntelligenceView::from_snapshot(&history, true),
            ["src/b.rs", "src/a.rs", "src/a.rs", "src/c.rs"],
        );
        assert_eq!(
            warnings
                .into_iter()
                .map(|warning| warning.path)
                .collect::<Vec<_>>(),
            vec!["src/a.rs"]
        );
        let mut degraded = history.clone();
        degraded.report.co_changes_complete = false;
        assert!(select_hotspot_warnings(
            GitIntelligenceView::from_snapshot(&degraded, true),
            ["src/a.rs"]
        )
        .is_empty());
    }

    #[test]
    fn consumer_output_limits_are_hard_caps() {
        let mut history = snapshot();
        history.files = (0..12)
            .map(|index| file(&format!("src/file-{index:02}.rs"), 4, 0))
            .collect();
        history.co_changes = (0..12)
            .map(|index| co_change("src/file-00.rs", &format!("src/partner-{index:02}.rs"), 2))
            .collect();
        let view = GitIntelligenceView::from_snapshot(&history, true);
        let paths: Vec<_> = history.files.iter().map(|file| file.path.clone()).collect();
        assert_eq!(
            select_hotspot_warnings(view, paths).len(),
            MAX_HOTSPOT_WARNINGS
        );
        let advisory = missing_cochange_partners(view, ["src/file-00.rs"]);
        assert_eq!(
            advisory.missing_cochange_partners.len(),
            MAX_MISSING_CO_CHANGE_PARTNERS
        );
        assert_eq!(
            advisory
                .missing_cochange_partners
                .last()
                .unwrap()
                .partner_path,
            "src/partner-09.rs"
        );
    }
}
