//! The score engine: a pure function from facts and weights to an explainable
//! bundle.
//!
//! # The scoring rule
//!
//! A score is the **weight-weighted mean of the rank percentiles of the facts
//! that were available**, in per-mille:
//!
//! ```text
//! score = Σ(weight_f × percentile_f) / Σ(weight_f)   over available facts f
//! ```
//!
//! This is not a choice made here. It is the rule the H1 harness used when it
//! measured the weights and built the calibration table
//! (`health::backtest::features::score`), so it is the only rule under which
//! `docs/reports/health-backtest/2026-08-14.md` describes the scores this
//! engine emits. Integer arithmetic throughout, rounding through
//! [`crate::health::arithmetic`], so the same facts give the same bundle byte
//! for byte on every platform.
//!
//! # Missing inputs widen the answer instead of faking it
//!
//! "Unknown is never zero" (spec design decision 4) is enforced by reporting a
//! *range*, not by substituting a value. Every fact absent from the bundle has
//! some true percentile in `[0, 1000]`, so the score has a floor (every unknown
//! at its lowest) and a ceiling (every unknown at its highest):
//!
//! ```text
//! floor   = Σ(weight_f × percentile_f)                    / Σ(weight_all)
//! ceiling = (Σ(weight_f × percentile_f) + 1000 × missing) / Σ(weight_all)
//! ```
//!
//! where `missing` is the total weight of inputs with no value. When every
//! input is present, `floor == score == ceiling` and the band is exact. When
//! inputs are missing, the band widens to a [`BandRange`] and every absent fact
//! is named in [`AxisScore::inputs_missing`], so a consumer can see precisely
//! what the engine did not know.
//!
//! # What is monotone here, and what is not
//!
//! Removing a fact can only widen the interval: the floor can only fall and the
//! ceiling can only rise, because the removed fact's weight moves from the
//! numerator into the unknown mass at both ends. That invariant is the honest
//! statement of "less evidence never strengthens a claim", and
//! `property_tests` proves it exhaustively.
//!
//! The headline `score_per_mille` is a *mean* over available facts, not a sum,
//! so it is not monotone under removal: dropping a fact that sat below the
//! file's other percentiles raises the mean of what remains. That is inherent
//! to the harness's own scoring rule, and changing it would mean shipping
//! scores the report never measured. It is why the floor, the ceiling, and
//! `inputs_missing` are part of the bundle rather than decoration: a consumer
//! comparing two files with different available inputs must compare their
//! ranges, not their point estimates alone.

use serde::{Deserialize, Serialize};

use crate::health::arithmetic::round_div;
use crate::health::config::HEALTH_CONFIG_VERSION;

use super::bands::{Band, BandRange};
use super::facts::{
    FactAvailability, FactFamily, FactKind, FactSourceRange, FactValue, FactWindow, FileFacts,
};
use super::weights::{Axis, WeightTable};

/// One fact's part in a score.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactContribution {
    /// Which fact this is.
    pub kind: FactKind,
    /// Which producer published it.
    pub family: FactFamily,
    /// The producer's raw value, as published.
    pub value: u64,
    /// Rank percentile of the raw value within the repository, per-mille.
    pub percentile_per_mille: u32,
    /// The weight this fact carries on this axis, per-mille.
    pub weight_per_mille: u32,
    /// How many per-mille of the score this fact accounts for. The
    /// contributions of all facts sum to the score, up to integer rounding.
    pub contribution_per_mille: u32,
    /// How complete the producing family's facts were.
    pub availability: FactAvailability,
    /// Whether a committed backtest report measured this fact's weight.
    pub backtested: bool,
    /// Where the fact was observed, when it has a location.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_range: Option<FactSourceRange>,
    /// The history window, for git-derived facts.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<FactWindow>,
}

impl FactContribution {
    /// The fact as a reader should see it: what was measured, and how it ranks.
    ///
    /// Descriptive only — a rank, never a prediction.
    pub fn describe(&self) -> String {
        format!(
            "{} (p{})",
            self.kind.describe_value(self.value),
            self.percentile_per_mille
        )
    }
}

/// A score on one axis, with the evidence it was computed from.
///
/// Spec design decision 3: a score is an ordered bundle, never a bare number. A
/// consumer can render every ranked entry from this struct alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AxisScore {
    /// Which axis was scored.
    pub axis: Axis,
    /// Weighted mean of the available facts' percentiles, per-mille. Only
    /// meaningful when `availability` is not `unavailable`.
    pub score_per_mille: u16,
    /// The band `score_per_mille` falls in.
    pub band: Band,
    /// The band range implied by the missing inputs; exact when none are
    /// missing.
    pub band_range: BandRange,
    /// The score with every missing input at its lowest possible value.
    pub score_floor_per_mille: u16,
    /// The score with every missing input at its highest possible value.
    pub score_ceiling_per_mille: u16,
    /// Contributing facts, heaviest contribution first.
    pub facts: Vec<FactContribution>,
    /// Inputs to this axis that had no value, in canonical fact order.
    pub inputs_missing: Vec<FactKind>,
    /// Worst availability of any family the score drew on, or `unavailable`
    /// when it drew on nothing.
    pub availability: FactAvailability,
    /// Version of the weight table the score was computed under.
    pub weights_version: u32,
    /// Version of the health config the facts were produced under.
    pub config_version: u32,
}

impl AxisScore {
    /// A score for an axis with no usable input at all.
    ///
    /// The whole range is possible, so the band range spans every band and the
    /// point estimate is not to be read: `availability` says so.
    pub fn unavailable(axis: Axis, weights: &WeightTable) -> Self {
        Self {
            axis,
            score_per_mille: 0,
            band: Band::Low,
            band_range: BandRange::new(Band::Low, Band::Critical),
            score_floor_per_mille: 0,
            score_ceiling_per_mille: 1000,
            facts: Vec::new(),
            inputs_missing: weights.inputs(axis),
            availability: FactAvailability::Unavailable,
            weights_version: weights.version,
            config_version: HEALTH_CONFIG_VERSION,
        }
    }

    /// Whether the missing inputs could not change the band.
    pub fn is_exact(&self) -> bool {
        self.band_range.is_exact()
    }

    /// The band as text: `"high"`, or `"moderate-critical"` when inputs were
    /// missing, or `"unknown"` when there were none.
    pub fn band_label(&self) -> String {
        if self.availability == FactAvailability::Unavailable {
            return "unknown".to_string();
        }
        self.band_range.label()
    }

    /// The heaviest contributing facts, most first.
    pub fn top_facts(&self, limit: usize) -> &[FactContribution] {
        let end = limit.min(self.facts.len());
        &self.facts[..end]
    }

    /// One line of evidence: the band, the score, and the heaviest facts.
    ///
    /// Ranking language only. The engine ranks files by measured evidence and
    /// never claims a specific future failure
    /// (`docs/reports/health-backtest/2026-08-14.md`, § "What H3 may and may
    /// not conclude").
    pub fn summary(&self, fact_limit: usize) -> String {
        if self.availability == FactAvailability::Unavailable {
            return format!("{} unknown: no facts available", self.axis.as_str().replace('_', " "));
        }

        let evidence: Vec<String> = self
            .top_facts(fact_limit)
            .iter()
            .map(FactContribution::describe)
            .collect();
        let mut summary = format!(
            "{} {} ({}/1000)",
            self.axis.as_str().replace('_', " "),
            self.band_label(),
            self.score_per_mille
        );
        if !evidence.is_empty() {
            summary.push_str(": ");
            summary.push_str(&evidence.join(", "));
        }
        if !self.inputs_missing.is_empty() {
            summary.push_str(&format!(
                "; scored without {}",
                self.inputs_missing
                    .iter()
                    .map(|kind| kind.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        summary
    }
}

/// Score one axis from one file's facts under one weight table.
///
/// Pure: the same `(facts, axis, weights)` always produce the same bundle. See
/// the module header for the rule, the interval, and what is monotone.
pub fn score_axis(facts: &FileFacts, axis: Axis, weights: &WeightTable) -> AxisScore {
    let inputs = weights.inputs(axis);
    let total_weight = weights.total_weight(axis);

    let mut weighted_sum: u128 = 0;
    let mut available_weight: u128 = 0;
    let mut contributions: Vec<(FactKind, u32, &FactValue)> = Vec::new();
    let mut inputs_missing: Vec<FactKind> = Vec::new();

    for kind in inputs {
        let weight = weights.weight(axis, kind);
        match facts.get(kind) {
            Some(value) => {
                let percentile = u128::from(value.percentile_per_mille.min(1000));
                weighted_sum += u128::from(weight) * percentile;
                available_weight += u128::from(weight);
                contributions.push((kind, weight, value));
            }
            None => inputs_missing.push(kind),
        }
    }

    if available_weight == 0 {
        return AxisScore::unavailable(axis, weights);
    }

    let score = round_div(weighted_sum, available_weight) as u16;
    let missing_weight = u128::from(total_weight).saturating_sub(available_weight);
    let floor = round_div(weighted_sum, u128::from(total_weight)) as u16;
    let ceiling = round_div(weighted_sum + missing_weight * 1000, u128::from(total_weight)) as u16;

    let mut facts_out: Vec<FactContribution> = contributions
        .into_iter()
        .map(|(kind, weight, value)| FactContribution {
            kind,
            family: kind.family(),
            value: value.value,
            percentile_per_mille: value.percentile_per_mille.min(1000),
            weight_per_mille: weight,
            contribution_per_mille: round_div(
                u128::from(weight) * u128::from(value.percentile_per_mille.min(1000)),
                available_weight,
            ) as u32,
            availability: value.availability,
            backtested: kind.is_backtested(),
            source_range: value.source_range.clone(),
            window: value.window.clone(),
        })
        .collect();

    // Heaviest contribution first; canonical fact order breaks ties so the
    // bundle is byte-identical for identical input.
    facts_out.sort_by(|left, right| {
        right
            .contribution_per_mille
            .cmp(&left.contribution_per_mille)
            .then_with(|| left.kind.index().cmp(&right.kind.index()))
    });

    let fact_availability = facts_out
        .iter()
        .map(|fact| fact.availability)
        .max()
        .unwrap_or(FactAvailability::Available);
    let availability = if inputs_missing.is_empty() {
        fact_availability
    } else {
        // An absent input is itself a degradation, however complete the facts
        // that did arrive were.
        fact_availability.worst(FactAvailability::Degraded)
    };

    AxisScore {
        axis,
        score_per_mille: score,
        band: Band::from_score_per_mille(score),
        band_range: BandRange::new(
            Band::from_score_per_mille(floor),
            Band::from_score_per_mille(ceiling),
        ),
        score_floor_per_mille: floor,
        score_ceiling_per_mille: ceiling,
        facts: facts_out,
        inputs_missing,
        availability,
        weights_version: weights.version,
        config_version: HEALTH_CONFIG_VERSION,
    }
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
