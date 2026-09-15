//! Pure, bounded consumers of published Git-intelligence snapshots.
//!
//! This module deliberately knows nothing about Git traversal, storage, graph
//! queries, or daemon freshness tracking. Callers provide a snapshot together
//! with its publication freshness; every consumer then applies the same
//! completeness gate before exposing history-derived behavior.

use serde::{Deserialize, Serialize};

use crate::git_intelligence::{
    FileHistorySignal, GitIntelligenceSnapshot, MissingCoChangePartner, SymbolHistorySignal,
};

/// Maximum number of missing co-change partners exposed by an impact response.
pub const MAX_MISSING_CO_CHANGE_PARTNERS: usize = 10;
/// A co-change advisory needs at least this many distinct commits.
pub const MIN_CO_CHANGE_PARTNER_COMMITS: u32 = 2;
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

    fn family_availability(
        self,
        complete: impl FnOnce(&GitIntelligenceSnapshot) -> bool,
    ) -> GitIntelligenceAvailability {
        match self.availability {
            GitIntelligenceAvailability::Available | GitIntelligenceAvailability::Degraded => {
                if self.snapshot.is_some_and(complete) {
                    GitIntelligenceAvailability::Available
                } else {
                    GitIntelligenceAvailability::Degraded
                }
            }
            GitIntelligenceAvailability::Unavailable | GitIntelligenceAvailability::Stale => {
                self.availability
            }
        }
    }

    pub fn file_history_availability(self) -> GitIntelligenceAvailability {
        self.family_availability(|snapshot| snapshot.report.file_history_complete())
    }

    pub fn symbol_history_availability(self) -> GitIntelligenceAvailability {
        self.family_availability(|snapshot| snapshot.report.symbol_history_complete())
    }

    pub fn co_change_availability(self) -> GitIntelligenceAvailability {
        self.family_availability(|snapshot| snapshot.report.co_change_history_complete())
    }

    pub fn usable_file_history_snapshot(self) -> Option<&'a GitIntelligenceSnapshot> {
        (self.file_history_availability() == GitIntelligenceAvailability::Available)
            .then_some(self.snapshot)
            .flatten()
    }

    pub fn usable_co_change_snapshot(self) -> Option<&'a GitIntelligenceSnapshot> {
        (self.co_change_availability() == GitIntelligenceAvailability::Available)
            .then_some(self.snapshot)
            .flatten()
    }

    /// Metadata suitable for a response even when signals are suppressed.
    pub fn metadata(self) -> GitIntelligenceMetadata {
        let Some(snapshot) = self.snapshot else {
            return GitIntelligenceMetadata {
                availability: self.availability,
                file_history_availability: self.file_history_availability(),
                symbol_history_availability: self.symbol_history_availability(),
                co_change_availability: self.co_change_availability(),
                co_change_width_exclusions: 0,
                window_commits: 0,
                head_commit_id: None,
            };
        };
        GitIntelligenceMetadata {
            availability: self.availability,
            file_history_availability: self.file_history_availability(),
            symbol_history_availability: self.symbol_history_availability(),
            co_change_availability: self.co_change_availability(),
            co_change_width_exclusions: snapshot.report.co_change_width_exclusions,
            window_commits: u32::try_from(snapshot.processed_commits.len()).unwrap_or(u32::MAX),
            head_commit_id: snapshot.head_commit_id().map(str::to_owned),
        }
    }
}

/// Minimal history snapshot metadata shared by consumer outputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitIntelligenceMetadata {
    pub availability: GitIntelligenceAvailability,
    pub file_history_availability: GitIntelligenceAvailability,
    pub symbol_history_availability: GitIntelligenceAvailability,
    pub co_change_availability: GitIntelligenceAvailability,
    pub co_change_width_exclusions: u32,
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
    let snapshot = view.usable_file_history_snapshot()?;
    let window_commits = window_commits(snapshot)?;
    if window_commits == 0 {
        return None;
    }

    let file = snapshot.file(file_path);
    let symbol = stable_symbol
        .filter(|_| view.symbol_history_availability() == GitIntelligenceAvailability::Available)
        .and_then(|symbol| snapshot.symbol(symbol));
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
    let Some(snapshot) = view.usable_co_change_snapshot() else {
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
            lines_added: 0,
            lines_deleted: 0,
            line_churn: 0,
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
    fn broad_commits_keep_file_history_without_fabricating_co_change_evidence() {
        use crate::git_intelligence::{CommitSample, GitHistoryMiner, PathChange};
        let commits = (0..500).map(|index| {
            let mut changes = vec![PathChange {
                path: "src/a.rs".into(),
                lines_added: 1,
                lines_deleted: 0,
                symbols: vec!["a::run".into()],
            }];
            if index < 4 {
                changes.extend((0..256).map(|path| PathChange {
                    path: format!("src/wide-{path}.rs"),
                    lines_added: 1,
                    lines_deleted: 0,
                    symbols: vec![],
                }));
            }
            CommitSample {
                id: format!("commit-{index}"),
                author: Some("A".into()),
                subject: if index % 2 == 0 { "fix bug" } else { "feature" }.into(),
                changes,
            }
        });
        let snapshot = GitHistoryMiner::default().mine(commits);
        assert_eq!(snapshot.report.co_change_width_exclusions, 4);
        let view = GitIntelligenceView::from_snapshot(&snapshot, true);
        assert_eq!(view.availability(), GitIntelligenceAvailability::Degraded);
        assert_eq!(
            view.file_history_availability(),
            GitIntelligenceAvailability::Available
        );
        let evidence = secondary_ranking_evidence(view, "src/a.rs", Some("a::run")).unwrap();
        assert_eq!(evidence.file_hotspot.unwrap().observed_commits, 500);
        assert_eq!(evidence.bug_fix_density_per_mille, Some(500));
        assert!(view.usable_co_change_snapshot().is_none());
        assert!(missing_cochange_partners(view, ["src/a.rs"])
            .missing_cochange_partners
            .is_empty());
        assert!(GitIntelligenceView::from_snapshot(&snapshot, false)
            .usable_file_history_snapshot()
            .is_none());
    }

    #[test]
    fn symbol_overflow_only_suppresses_symbol_evidence() {
        let mut history = snapshot();
        history.report.symbol_overflow_commits = 1;
        let view = GitIntelligenceView::from_snapshot(&history, true);
        let evidence = secondary_ranking_evidence(view, "src/a.rs", Some("a::high")).unwrap();
        assert!(evidence.file_hotspot.is_some());
        assert!(evidence.symbol_hotspot.is_none());
        assert!(view.usable_co_change_snapshot().is_some());
    }

    #[test]
    fn invalid_or_incomplete_file_observations_never_feed_a_family() {
        for kind in 0..4 {
            let mut history = snapshot();
            match kind {
                0 => history.report.invalid_path_entries = 1,
                1 => history.report.invalid_commit_ids = 1,
                2 => history.report.path_overflow_commits = 1,
                _ => history.report.included_commits = 0,
            }
            let view = GitIntelligenceView::from_snapshot(&history, true);
            assert!(view.usable_file_history_snapshot().is_none());
            assert!(view.usable_co_change_snapshot().is_none());
            assert!(secondary_ranking_evidence(view, "src/a.rs", Some("a::high")).is_none());
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
    fn consumer_output_limits_are_hard_caps() {
        let mut history = snapshot();
        history.files = (0..12)
            .map(|index| file(&format!("src/file-{index:02}.rs"), 4, 0))
            .collect();
        history.co_changes = (0..12)
            .map(|index| co_change("src/file-00.rs", &format!("src/partner-{index:02}.rs"), 2))
            .collect();
        let view = GitIntelligenceView::from_snapshot(&history, true);
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
