use super::*;

use crate::health::complexity_facts::compute_file_complexity_facts;
use crate::health::config::HEALTH_CONFIG_VERSION;

const SAMPLE_A: &str = r#"pub fn alpha(a: u32) -> u32 {
    if a > 0 {
        1
    } else {
        0
    }
}
"#;

const SAMPLE_B: &str = r#"pub fn beta(a: u32, b: u32) -> u32 {
    match a {
        0 => b,
        1 => b + 1,
        _ => 0,
    }
}
"#;

fn store() -> HealthComplexityFactsStore {
    HealthComplexityFactsStore::open_in_memory().expect("store opens")
}

fn facts(file: &str, source: &str) -> FileComplexityFacts {
    compute_file_complexity_facts(file, source)
}

#[test]
fn nothing_is_readable_before_the_first_publish() {
    let store = store();
    let generation = store
        .begin_generation(HEALTH_CONFIG_VERSION)
        .expect("generation opens");
    store
        .write_file_facts(generation, &facts("src/alpha.rs", SAMPLE_A))
        .expect("facts write");

    assert_eq!(store.active_generation().expect("read pointer"), None);
    assert_eq!(store.file_facts("src/alpha.rs").expect("read"), None);
    assert!(store.active_file_facts().expect("read all").is_empty());
    // The unpublished generation still holds the write.
    assert!(store
        .file_facts_in(generation, "src/alpha.rs")
        .expect("read generation")
        .is_some());
}

#[test]
fn publishing_swaps_the_pointer_and_round_trips_facts() {
    let store = store();
    let generation = store
        .begin_generation(HEALTH_CONFIG_VERSION)
        .expect("generation opens");
    let written = facts("src/alpha.rs", SAMPLE_A);
    store
        .write_file_facts(generation, &written)
        .expect("facts write");
    let status = store.publish_generation(generation).expect("publish");

    assert!(status.published);
    assert_eq!(status.files_total, 1);
    assert_eq!(status.files_available, 1);
    assert_eq!(status.files_degraded, 0);
    assert_eq!(status.files_unavailable, 0);
    assert!(status.is_complete());
    assert_eq!(status.config_version, HEALTH_CONFIG_VERSION);
    assert_eq!(
        store.active_generation().expect("pointer"),
        Some(generation)
    );

    let read = store
        .file_facts("src/alpha.rs")
        .expect("read")
        .expect("facts present");
    assert_eq!(read, written, "stored facts must round trip exactly");
}

#[test]
fn a_new_generation_is_isolated_until_it_is_published() {
    let store = store();
    let first = store
        .begin_generation(HEALTH_CONFIG_VERSION)
        .expect("gen 1");
    store
        .write_file_facts(first, &facts("src/alpha.rs", SAMPLE_A))
        .expect("write");
    store.publish_generation(first).expect("publish 1");

    let second = store
        .begin_generation(HEALTH_CONFIG_VERSION)
        .expect("gen 2");
    assert!(second > first, "generations are monotonic");
    store
        .write_file_facts(second, &facts("src/beta.rs", SAMPLE_B))
        .expect("write");

    // Readers still see only generation one.
    assert_eq!(store.active_generation().expect("pointer"), Some(first));
    assert!(store.file_facts("src/beta.rs").expect("read").is_none());
    assert_eq!(store.active_file_facts().expect("read all").len(), 1);

    store.publish_generation(second).expect("publish 2");

    // The swap moves readers wholesale to generation two.
    assert_eq!(store.active_generation().expect("pointer"), Some(second));
    assert!(store.file_facts("src/beta.rs").expect("read").is_some());
    assert!(
        store.file_facts("src/alpha.rs").expect("read").is_none(),
        "generation two never contained alpha, so alpha is now unknown"
    );
    // Generation one is untouched and still readable by number.
    assert!(store
        .file_facts_in(first, "src/alpha.rs")
        .expect("read")
        .is_some());
}

#[test]
fn symbol_facts_round_trip_and_stay_ordered() {
    let store = store();
    let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
    let written = facts("src/beta.rs", SAMPLE_B);
    store.write_file_facts(generation, &written).expect("write");
    store.publish_generation(generation).expect("publish");

    let read = store
        .file_facts("src/beta.rs")
        .expect("read")
        .expect("present");
    assert_eq!(read.symbols, written.symbols);
    let unit = read.symbols.first().expect("one unit");
    assert_eq!(unit.symbol, "beta");
    // `0 =>` and `1 =>` count; `_ =>` is the structural default.
    assert_eq!(unit.cyclomatic_complexity, 3);
    assert_eq!(unit.param_count, Some(2));

    let offsets: Vec<usize> = read.symbols.iter().map(|unit| unit.byte_offset).collect();
    let mut sorted = offsets.clone();
    sorted.sort_unstable();
    assert_eq!(offsets, sorted);
}

#[test]
fn rewriting_a_file_replaces_only_that_file() {
    let store = store();
    let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
    store
        .write_file_facts(generation, &facts("src/alpha.rs", SAMPLE_A))
        .expect("write alpha");
    store
        .write_file_facts(generation, &facts("src/beta.rs", SAMPLE_B))
        .expect("write beta");
    store.publish_generation(generation).expect("publish");

    // Re-index alpha alone with a simpler body.
    let refreshed = facts("src/alpha.rs", "pub fn alpha(a: u32) -> u32 {\n    a\n}\n");
    store
        .write_file_facts(generation, &refreshed)
        .expect("rewrite alpha");

    let alpha = store
        .file_facts("src/alpha.rs")
        .expect("read")
        .expect("present");
    assert_eq!(alpha, refreshed);
    assert_eq!(alpha.symbols.len(), 1);
    assert_eq!(alpha.symbols[0].cyclomatic_complexity, 1);

    let beta = store
        .file_facts("src/beta.rs")
        .expect("read")
        .expect("present");
    assert_eq!(
        beta.symbols[0].cyclomatic_complexity, 3,
        "beta is untouched"
    );
}

#[test]
fn unavailable_facts_persist_their_reason_and_no_rollup() {
    let store = store();
    let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
    let exempt = facts("docs/guide.md", "# Title\n");
    assert_eq!(exempt.availability, FactAvailability::Unavailable);
    store.write_file_facts(generation, &exempt).expect("write");
    let status = store.publish_generation(generation).expect("publish");

    assert_eq!(status.files_unavailable, 1);
    assert!(
        !status.is_complete(),
        "an unavailable file is incompleteness"
    );

    let read = store
        .file_facts("docs/guide.md")
        .expect("read")
        .expect("present");
    assert_eq!(read.availability, FactAvailability::Unavailable);
    assert_eq!(
        read.unavailable_reason,
        Some(ComplexityUnavailableReason::NoExecutableControlFlow)
    );
    assert!(read.exemption_reason.is_some());
    assert!(
        read.rollup.is_none(),
        "an unavailable file must not persist a zero rollup"
    );
}

#[test]
fn degraded_files_are_counted_as_incomplete() {
    let store = store();
    let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
    let broken = facts(
        "src/broken.rs",
        "pub fn broken(a: u32) -> u32 {\n    if a > 0 {\n",
    );
    assert_eq!(broken.availability, FactAvailability::Degraded);
    store.write_file_facts(generation, &broken).expect("write");
    let status = store.publish_generation(generation).expect("publish");

    assert_eq!(status.files_degraded, 1);
    assert_eq!(status.files_available, 0);
    assert!(!status.is_complete());
}

#[test]
fn writes_to_a_missing_generation_are_refused() {
    let store = store();
    let error = store
        .write_file_facts(99, &facts("src/alpha.rs", SAMPLE_A))
        .expect_err("write must fail");
    assert!(error.to_string().contains("does not exist"));
    assert!(store.publish_generation(99).is_err());
}

#[test]
fn non_canonical_paths_are_refused() {
    let store = store();
    let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");

    for path in [
        "",
        "/abs/path.rs",
        "./src/alpha.rs",
        "src/../src/alpha.rs",
        "src//alpha.rs",
        "src\\alpha.rs",
        "src/",
        "C:/src/alpha.rs",
    ] {
        let mut candidate = facts("src/alpha.rs", SAMPLE_A);
        candidate.file = path.to_string();
        assert!(
            store.write_file_facts(generation, &candidate).is_err(),
            "path {path:?} must be rejected as non-canonical"
        );
    }

    assert!(validate_canonical_path("src/alpha.rs").is_ok());
    assert!(validate_canonical_path("a.rs").is_ok());
}

#[test]
fn pruning_keeps_recent_generations_and_never_the_active_one() {
    let store = store();
    let mut generations = Vec::new();
    for _ in 0..4 {
        let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
        store
            .write_file_facts(generation, &facts("src/alpha.rs", SAMPLE_A))
            .expect("write");
        generations.push(generation);
    }
    // Publish the oldest, then prune down to the two newest.
    store.publish_generation(generations[0]).expect("publish");
    let pruned = store.prune_generations(2).expect("prune");

    assert_eq!(pruned, 1, "only generation two is expendable");
    assert!(store
        .generation_status(generations[0])
        .expect("status")
        .is_some());
    assert!(store
        .generation_status(generations[1])
        .expect("status")
        .is_none());
    assert!(store
        .generation_status(generations[3])
        .expect("status")
        .is_some());
    assert!(
        store
            .file_facts_in(generations[1], "src/alpha.rs")
            .expect("read")
            .is_none(),
        "pruning removes the generation's facts too"
    );

    // A new generation never reuses a pruned number.
    let next = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
    assert!(next > *generations.last().expect("generations"));
}

#[test]
fn file_backed_store_persists_across_reopen() {
    let directory = tempfile::tempdir().expect("temp dir");
    let path = directory.path().join("health.db");

    {
        let store = HealthComplexityFactsStore::open(&path).expect("open");
        let generation = store.begin_generation(HEALTH_CONFIG_VERSION).expect("gen");
        store
            .write_file_facts(generation, &facts("src/alpha.rs", SAMPLE_A))
            .expect("write");
        store.publish_generation(generation).expect("publish");
        assert_eq!(store.path(), Some(path.as_path()));
    }

    let reopened = HealthComplexityFactsStore::open(&path).expect("reopen");
    let status = reopened
        .active_generation_status()
        .expect("status")
        .expect("published generation survives");
    assert!(status.published);
    assert_eq!(status.files_available, 1);
    assert!(reopened.file_facts("src/alpha.rs").expect("read").is_some());
}
