//! Score bands, and the calibration evidence that placed their cutoffs.
//!
//! # Where the cutoffs come from
//!
//! `docs/reports/health-backtest/2026-08-14.md`, § "Calibration by decile",
//! reports the observed rate of later fix-shaped commits per score decile,
//! pooled across five repositories under held-out derived weights. For the
//! `graph+git+complexity` family — the one this engine ships — that table reads:
//!
//! | Decile | Observed rate |
//! | --- | ---: |
//! | 0.000 – 0.099 | 0.1% |
//! | 0.100 – 0.199 | 1.0% |
//! | 0.200 – 0.299 | 2.1% |
//! | 0.300 – 0.399 | 3.8% |
//! | 0.400 – 0.499 | 8.3% |
//! | 0.500 – 0.599 | 12.7% |
//! | 0.600 – 0.699 | 25.7% |
//! | 0.700 – 0.799 | 27.2% |
//! | 0.800 – 0.899 | 41.6% |
//! | 0.900 – 1.000 | 15.4% |
//!
//! The corpus prevalence is 2.5%. A band boundary is placed at each decile edge
//! where the observed rate takes a step, so that the four bands are four
//! genuinely different observed rates rather than four equal slices of the
//! range:
//!
//! * **300** — 2.1% to 3.8%, the point where the observed rate first exceeds
//!   prevalence. Below it, a file is not distinguishable from an average file.
//! * **600** — 12.7% to 25.7%, the largest single step in the table: the rate
//!   roughly doubles, to ten times prevalence.
//! * **800** — 27.2% to 41.6%, the top of the table's monotone run, at about
//!   seventeen times prevalence.
//!
//! The final decile (0.900–1.000, 15.4%) is **not** given a band of its own.
//! It holds 13 files, and the report's own § "What H3 may and may not conclude"
//! warns that differences smaller than the spread across cut points are not
//! evidence; a band resting on 13 observations would be exactly that. It is
//! folded into `critical`, whose evidence is the 423-file decile below it.
//!
//! # What a band is not
//!
//! A band is a rank, established by correlation on a subject-line ground truth
//! with a measured 15.9% recall gap. It ranks files by evidence; it does not
//! predict that any particular file will fail. Nothing rendered from a band may
//! say otherwise.

use serde::{Deserialize, Serialize};

/// The score at or above which a file leaves [`Band::Low`].
pub const MODERATE_CUTOFF_PER_MILLE: u16 = 300;
/// The score at or above which a file reaches [`Band::High`].
pub const HIGH_CUTOFF_PER_MILLE: u16 = 600;
/// The score at or above which a file reaches [`Band::Critical`].
pub const CRITICAL_CUTOFF_PER_MILLE: u16 = 800;

/// A calibrated score band.
///
/// Ordered, so bands compare and a range can be checked for width.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Band {
    /// Below the corpus's own defect prevalence: not distinguishable from an
    /// average file on this evidence.
    Low,
    /// Above prevalence, up to roughly five times it.
    Moderate,
    /// Roughly ten times prevalence.
    High,
    /// Roughly seventeen times prevalence.
    Critical,
}

/// Every band, weakest first.
pub const ALL_BANDS: [Band; 4] = [Band::Low, Band::Moderate, Band::High, Band::Critical];

impl Band {
    /// The band a score falls in.
    pub fn from_score_per_mille(score: u16) -> Self {
        if score >= CRITICAL_CUTOFF_PER_MILLE {
            Self::Critical
        } else if score >= HIGH_CUTOFF_PER_MILLE {
            Self::High
        } else if score >= MODERATE_CUTOFF_PER_MILLE {
            Self::Moderate
        } else {
            Self::Low
        }
    }

    /// Stable identifier for payloads and report text.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Moderate => "moderate",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// Read a band back from its stable identifier.
    pub fn from_code(value: &str) -> Option<Self> {
        ALL_BANDS.iter().copied().find(|band| band.as_str() == value)
    }

    /// The observed rate of later fix-shaped commits in this band's deciles,
    /// in per-mille, as reported in § "Calibration by decile".
    ///
    /// Reported to consumers as the calibration evidence behind a band, never
    /// as a probability attached to the individual file being scored.
    pub fn observed_defect_rate_per_mille(&self) -> u32 {
        // Files-weighted mean of the deciles the band spans, from the
        // graph+git+complexity table: e.g. moderate spans the 0.300, 0.400 and
        // 0.500 deciles, which are 243/6479, 265/3177 and 323/2547.
        match self {
            // (64 + 104 + 249) / (45223 + 10362 + 11950)
            Self::Low => 6,
            // (243 + 265 + 323) / (6479 + 3177 + 2547)
            Self::Moderate => 68,
            // (403 + 271) / (1570 + 998)
            Self::High => 262,
            // (176 + 2) / (423 + 13)
            Self::Critical => 408,
        }
    }
}

/// A band, or a range of bands when inputs were missing.
///
/// Spec H3.2: a score over incomplete inputs "widens its band to a range rather
/// than faking precision". When every input was available the range is a single
/// band and [`BandRange::is_exact`] is true.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BandRange {
    /// The band the score reaches if every missing input is at its lowest.
    pub floor: Band,
    /// The band the score reaches if every missing input is at its highest.
    pub ceiling: Band,
}

impl BandRange {
    /// A range spanning one band.
    pub fn exact(band: Band) -> Self {
        Self {
            floor: band,
            ceiling: band,
        }
    }

    /// A range between two bands, in either order.
    pub fn new(floor: Band, ceiling: Band) -> Self {
        Self {
            floor: floor.min(ceiling),
            ceiling: floor.max(ceiling),
        }
    }

    /// Whether the missing inputs could not change the band.
    pub fn is_exact(&self) -> bool {
        self.floor == self.ceiling
    }

    /// Renders as `"high"` when exact and `"moderate-critical"` when not.
    pub fn label(&self) -> String {
        if self.is_exact() {
            self.floor.as_str().to_string()
        } else {
            format!("{}-{}", self.floor.as_str(), self.ceiling.as_str())
        }
    }
}

#[cfg(test)]
#[path = "bands_tests.rs"]
mod tests;
