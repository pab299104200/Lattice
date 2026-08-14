use super::*;

use crate::health::churn_facts;
use crate::health::complexity_facts;
use crate::health::dead_symbol_facts;
use crate::health::graph_facts;
use crate::health::test_proximity_facts;

fn value(kind: FactKind, raw: u64) -> FactValue {
    FactValue::new(kind, raw, 500, FactAvailability::Available)
}

#[test]
fn facts_stay_in_canonical_order_however_they_arrive() {
    let mut facts = FileFacts::new("src/lib.rs");
    facts.insert(value(FactKind::FunctionCount, 4));
    facts.insert(value(FactKind::FanIn, 2));
    facts.insert(value(FactKind::HotspotScore, 9));

    let order: Vec<FactKind> = facts.values.iter().map(|entry| entry.kind).collect();

    assert_eq!(
        order,
        vec![
            FactKind::FanIn,
            FactKind::HotspotScore,
            FactKind::FunctionCount
        ]
    );
}

#[test]
fn inserting_the_same_fact_twice_replaces_rather_than_duplicates() {
    let mut facts = FileFacts::new("src/lib.rs");
    facts.insert(value(FactKind::FanIn, 2));
    facts.insert(value(FactKind::FanIn, 7));

    assert_eq!(facts.values.len(), 1);
    assert_eq!(facts.get(FactKind::FanIn).map(|fact| fact.value), Some(7));
}

#[test]
fn removing_a_fact_leaves_no_trace_of_it() {
    let mut facts = FileFacts::new("src/lib.rs");
    facts.insert(value(FactKind::FanIn, 2));

    assert!(facts.remove(FactKind::FanIn).is_some());
    assert!(!facts.has(FactKind::FanIn));
    assert!(facts.remove(FactKind::FanIn).is_none());
}

#[test]
fn a_bundle_with_no_facts_is_unavailable_rather_than_available() {
    assert_eq!(
        FileFacts::new("src/lib.rs").availability(),
        FactAvailability::Unavailable
    );
}

#[test]
fn a_bundles_availability_is_its_worst_facts() {
    let mut facts = FileFacts::new("src/lib.rs");
    facts.insert(value(FactKind::FanIn, 2));
    facts.insert(FactValue::new(
        FactKind::FanOut,
        3,
        500,
        FactAvailability::Degraded,
    ));

    assert_eq!(facts.availability(), FactAvailability::Degraded);
}

#[test]
fn a_percentile_can_never_exceed_the_scale() {
    let fact = FactValue::new(FactKind::FanIn, 2, 4_000, FactAvailability::Available);

    assert_eq!(fact.percentile_per_mille, 1000);
}

#[test]
fn every_producers_availability_maps_onto_the_scoring_one() {
    assert_eq!(
        FactAvailability::from(graph_facts::FactAvailability::Degraded),
        FactAvailability::Degraded
    );
    assert_eq!(
        FactAvailability::from(complexity_facts::FactAvailability::Unavailable),
        FactAvailability::Unavailable
    );
    assert_eq!(
        FactAvailability::from(test_proximity_facts::FactAvailability::Available),
        FactAvailability::Available
    );
    assert_eq!(
        FactAvailability::from(dead_symbol_facts::FactAvailability::Degraded),
        FactAvailability::Degraded
    );
    // The churn producer has no `Unavailable` variant: a window that saw a path
    // always produced a signal for it.
    assert_eq!(
        FactAvailability::from(churn_facts::FactAvailability::Degraded),
        FactAvailability::Degraded
    );
}

#[test]
fn availability_composes_to_the_worst_of_its_inputs() {
    assert_eq!(
        FactAvailability::Available.worst(FactAvailability::Degraded),
        FactAvailability::Degraded
    );
    assert_eq!(
        FactAvailability::Degraded.worst(FactAvailability::Unavailable),
        FactAvailability::Unavailable
    );
    assert_eq!(
        FactAvailability::Available.worst(FactAvailability::Available),
        FactAvailability::Available
    );
}

#[test]
fn fact_codes_round_trip_and_stay_unique() {
    let mut seen: Vec<&str> = Vec::new();
    for kind in ALL_FACT_KINDS {
        assert_eq!(FactKind::from_code(kind.as_str()), Some(kind));
        assert!(!seen.contains(&kind.as_str()), "duplicate fact code");
        seen.push(kind.as_str());
    }
    assert_eq!(FactKind::from_code("duplication"), None);
}

#[test]
fn fact_indexes_match_their_position_in_the_canonical_list() {
    for (position, kind) in ALL_FACT_KINDS.iter().enumerate() {
        assert_eq!(kind.index(), position);
    }
}

#[test]
fn only_bus_factor_reads_low_as_risky() {
    for kind in ALL_FACT_KINDS {
        let expected = if kind == FactKind::BusFactor {
            RiskDirection::LowerIsRiskier
        } else {
            RiskDirection::HigherIsRiskier
        };
        assert_eq!(kind.direction(), expected, "{}", kind.as_str());
    }
}

#[test]
fn a_fact_describes_itself_without_predicting_anything() {
    assert_eq!(FactKind::FanIn.describe_value(23), "fan-in 23");
    assert_eq!(
        FactKind::BugFixCommits.describe_value(8),
        "8 bug-fix commits in window"
    );
    assert_eq!(FactKind::CycleMember.describe_value(1), "cycle member");
    assert_eq!(
        FactKind::CycleMember.describe_value(0),
        "not in a dependency cycle"
    );
    assert_eq!(
        FactKind::UntestedChange.describe_value(1),
        "no edge-linked tests"
    );
    assert_eq!(
        FactKind::DeadExportedSymbols.describe_value(2),
        "2 exported symbols with no indexed dependents"
    );
}

#[test]
fn a_source_range_never_ends_before_it_starts() {
    let range = FactSourceRange::new("src/lib.rs", 40, 12);

    assert_eq!(range.start_line, 40);
    assert_eq!(range.end_line, 40);
}
