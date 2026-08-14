//! Properties the engine must hold for every fact set, not only the fixtures.
//!
//! No property-test dependency is added: the fact space is small and bounded
//! (21 facts, two axes), so the sweeps below enumerate it directly — every
//! single removal, every pair, every triple, plus a deterministic
//! pseudo-random walk over larger subsets. Deterministic enumeration is
//! preferable here to a randomized harness: a failure is reproducible without a
//! seed, which is the same discipline the rest of the health engine follows.

use super::engine::score_axis;
use super::facts::{FactKind, FileFacts, ALL_FACT_KINDS};
use super::test_support::{hot_cyclic_file, quiet_leaf_file, weights};
use super::weights::ALL_AXES;

/// The fixtures every property is checked against.
fn bases() -> Vec<FileFacts> {
    vec![hot_cyclic_file(), quiet_leaf_file(), mixed_file()]
}

/// A fixture with percentiles deliberately spread across the range, so that
/// removing a fact can move the mean in either direction.
fn mixed_file() -> FileFacts {
    let mut facts = hot_cyclic_file();
    for (index, kind) in ALL_FACT_KINDS.iter().enumerate() {
        if let Some(existing) = facts.remove(*kind) {
            let percentile = ((index as u32 * 137) % 11) * 100;
            facts.insert(super::facts::FactValue::new(
                *kind,
                existing.value,
                percentile,
                existing.availability,
            ));
        }
    }
    facts
}

/// Every subset of facts removed from a base, as the sweeps below enumerate
/// them.
fn removal_sets() -> Vec<Vec<FactKind>> {
    let mut sets: Vec<Vec<FactKind>> = Vec::new();
    for first in ALL_FACT_KINDS {
        sets.push(vec![first]);
        for second in ALL_FACT_KINDS {
            if second.index() <= first.index() {
                continue;
            }
            sets.push(vec![first, second]);
            for third in ALL_FACT_KINDS {
                if third.index() <= second.index() {
                    continue;
                }
                sets.push(vec![first, second, third]);
            }
        }
    }

    // Larger subsets, walked deterministically: a 21-bit mask stepped by a
    // fixed linear congruential generator covers wide removals without a seed
    // anyone has to record.
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    for _ in 0..4096 {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let mask = state >> 32;
        let removed: Vec<FactKind> = ALL_FACT_KINDS
            .iter()
            .copied()
            .filter(|kind| mask >> kind.index() & 1 == 1)
            .collect();
        if !removed.is_empty() {
            sets.push(removed);
        }
    }
    sets
}

#[test]
fn removing_facts_never_raises_the_score_floor() {
    let sets = removal_sets();
    for base in bases() {
        for axis in ALL_AXES {
            let before = score_axis(&base, axis, weights());
            for removal in &sets {
                let mut reduced = base.clone();
                for kind in removal {
                    reduced.remove(*kind);
                }
                let after = score_axis(&reduced, axis, weights());
                assert!(
                    after.score_floor_per_mille <= before.score_floor_per_mille,
                    "{axis:?}: removing {removal:?} raised the floor from {} to {}",
                    before.score_floor_per_mille,
                    after.score_floor_per_mille
                );
                assert!(
                    after.band_range.floor <= before.band_range.floor,
                    "{axis:?}: removing {removal:?} raised the band floor"
                );
            }
        }
    }
}

#[test]
fn removing_facts_never_lowers_the_score_ceiling() {
    let sets = removal_sets();
    for base in bases() {
        for axis in ALL_AXES {
            let before = score_axis(&base, axis, weights());
            for removal in &sets {
                let mut reduced = base.clone();
                for kind in removal {
                    reduced.remove(*kind);
                }
                let after = score_axis(&reduced, axis, weights());
                assert!(
                    after.score_ceiling_per_mille >= before.score_ceiling_per_mille,
                    "{axis:?}: removing {removal:?} lowered the ceiling from {} to {}",
                    before.score_ceiling_per_mille,
                    after.score_ceiling_per_mille
                );
                assert!(
                    after.band_range.ceiling >= before.band_range.ceiling,
                    "{axis:?}: removing {removal:?} lowered the band ceiling"
                );
            }
        }
    }
}

#[test]
fn the_point_estimate_always_lies_inside_its_own_interval() {
    let sets = removal_sets();
    for base in bases() {
        for axis in ALL_AXES {
            for removal in &sets {
                let mut reduced = base.clone();
                for kind in removal {
                    reduced.remove(*kind);
                }
                let score = score_axis(&reduced, axis, weights());
                assert!(
                    score.score_floor_per_mille <= score.score_per_mille
                        && score.score_per_mille <= score.score_ceiling_per_mille,
                    "{axis:?}: {} outside [{}, {}]",
                    score.score_per_mille,
                    score.score_floor_per_mille,
                    score.score_ceiling_per_mille
                );
            }
        }
    }
}

#[test]
fn lowering_a_facts_rank_never_raises_the_score() {
    for base in bases() {
        for axis in ALL_AXES {
            let before = score_axis(&base, axis, weights());
            for kind in ALL_FACT_KINDS {
                let Some(existing) = base.get(kind) else {
                    continue;
                };
                for lowered in [0, existing.percentile_per_mille / 2] {
                    let mut reduced = base.clone();
                    reduced.insert(super::facts::FactValue::new(
                        kind,
                        existing.value,
                        lowered,
                        existing.availability,
                    ));
                    let after = score_axis(&reduced, axis, weights());
                    assert!(
                        after.score_per_mille <= before.score_per_mille,
                        "{axis:?}: lowering {kind:?} to {lowered} raised the score from {} to {}",
                        before.score_per_mille,
                        after.score_per_mille
                    );
                    assert!(after.score_floor_per_mille <= before.score_floor_per_mille);
                }
            }
        }
    }
}

#[test]
fn a_score_is_always_within_the_per_mille_range() {
    let sets = removal_sets();
    for base in bases() {
        for axis in ALL_AXES {
            for removal in &sets {
                let mut reduced = base.clone();
                for kind in removal {
                    reduced.remove(*kind);
                }
                let score = score_axis(&reduced, axis, weights());
                assert!(score.score_per_mille <= 1000);
                assert!(score.score_ceiling_per_mille <= 1000);
            }
        }
    }
}

#[test]
fn missing_inputs_are_always_named_and_never_double_counted() {
    let sets = removal_sets();
    for base in bases() {
        for axis in ALL_AXES {
            for removal in &sets {
                let mut reduced = base.clone();
                for kind in removal {
                    reduced.remove(*kind);
                }
                let score = score_axis(&reduced, axis, weights());
                let inputs = weights().inputs(axis);
                assert_eq!(
                    score.facts.len() + score.inputs_missing.len(),
                    inputs.len(),
                    "{axis:?}: every input is either contributing or missing"
                );
                for kind in &score.inputs_missing {
                    assert!(!reduced.has(*kind));
                    assert!(weights().weight(axis, *kind) > 0);
                }
                let mut sorted = score.inputs_missing.clone();
                sorted.sort_by_key(FactKind::index);
                assert_eq!(sorted, score.inputs_missing, "missing inputs stay ordered");
            }
        }
    }
}

#[test]
fn scoring_is_deterministic_for_identical_input() {
    for base in bases() {
        for axis in ALL_AXES {
            assert_eq!(
                score_axis(&base, axis, weights()),
                score_axis(&base, axis, weights())
            );
        }
    }
}
