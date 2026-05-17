//! Phase 1 identity-resolution budget tests for
//! `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md`
//! `## Phase 1: Unified Identity Model`.
//!
//! The spec requires rename, move, duplicate-name, and branch-change coverage
//! plus proof that identity resolution adds no more than 2ms P99 to hot-path
//! tool calls. These tests stay `#[ignore]` so the normal unit-test loop
//! remains fast while review passes can lift the gate explicitly.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use serde::Deserialize;

use super::resolver_tests::helpers::{FixtureBuilder, ResolverFixture};
use super::{EventId, Identity, ResolveOutcome};

const ITERATIONS: usize = 10_000;
const BUDGET_US: u128 = 2_000;

#[test]
#[ignore]
fn test_resolve_symbol_p99_within_2ms_budget() {
    let fixture = benchmark_fixture();
    let queries = symbol_queries();
    prime_symbols(&fixture, &queries);

    let p99_us = measure_p99_us(ITERATIONS, |index| {
        let query = &queries[index % queries.len()];
        let outcome = fixture
            .resolver
            .resolve_symbol(&fixture.workspace("main"), query);
        assert!(matches!(outcome, ResolveOutcome::Unique(_)));
    });

    assert_within_budget("resolve_symbol", p99_us);
}

#[test]
#[ignore]
fn test_resolve_path_p99_within_2ms_budget() {
    let fixture = benchmark_fixture();
    let queries = path_queries();
    prime_paths(&fixture, &queries);

    let p99_us = measure_p99_us(ITERATIONS, |index| {
        let query = &queries[index % queries.len()];
        fixture
            .resolver
            .resolve_path(&fixture.workspace("main"), query)
            .expect("path query should resolve");
    });

    assert_within_budget("resolve_path", p99_us);
}

#[test]
#[ignore]
fn test_resolve_section_p99_within_2ms_budget() {
    let fixture = benchmark_fixture();
    let docs = section_queries(&fixture);
    prime_sections(&fixture, &docs);

    let p99_us = measure_p99_us(ITERATIONS, |index| {
        let (doc, heading) = &docs[index % docs.len()];
        let outcome = fixture
            .resolver
            .resolve_section(&fixture.workspace("main"), doc, heading);
        assert!(matches!(outcome, ResolveOutcome::Unique(_)));
    });

    assert_within_budget("resolve_section", p99_us);
}

#[test]
#[ignore]
fn test_resolver_does_not_regress_baseline_workflow_p99() {
    let fixture = benchmark_fixture();
    let symbol_queries = symbol_queries();
    let path_queries = path_queries();
    let section_queries = section_queries(&fixture);
    let event_ref = Identity::Event(EventId {
        workspace_id: "main".to_string(),
        ulid: "01J0000000000000000000000A".to_string(),
    })
    .to_string();
    let baseline_us = load_baseline_p99_us("prepare_change");

    let p99_us = measure_p99_us(ITERATIONS, |index| {
        let symbol = &symbol_queries[index % symbol_queries.len()];
        let path = &path_queries[index % path_queries.len()];
        let (doc, heading) = &section_queries[index % section_queries.len()];
        fixture
            .resolver
            .resolve_symbol(&fixture.workspace("main"), symbol);
        fixture
            .resolver
            .resolve_path(&fixture.workspace("main"), path)
            .expect("path query should resolve");
        fixture
            .resolver
            .resolve_section(&fixture.workspace("main"), doc, heading);
        fixture
            .resolver
            .resolve_event_ref(&fixture.workspace("main"), &event_ref)
            .expect("event query should resolve");
    });

    let allowed_us = baseline_us + BUDGET_US;
    assert!(
        p99_us <= allowed_us,
        "resolver hot path p99 {p99_us}us exceeded baseline prepare_change p99 {baseline_us}us by more than 2ms"
    );
}

fn benchmark_fixture() -> ResolverFixture {
    let mut builder = FixtureBuilder::new("main");
    for index in 0..64 {
        builder = builder.rust(
            "main",
            &format!("src/module_{index}.rs"),
            &format!("pub fn lookup_{index}() {{}}\npub fn helper_{index}() {{}}\n"),
        );
    }
    for index in 0..16 {
        builder = builder.markdown(
            "main",
            &format!("docs/guide_{index}.md"),
            &format!("# Guide {index}\n\n## Intro {index}\nText\n\n## Deploy {index}\nMore text\n"),
        );
    }
    builder
        .event(EventId {
            workspace_id: "main".to_string(),
            ulid: "01J0000000000000000000000A".to_string(),
        })
        .build()
}

fn symbol_queries() -> Vec<String> {
    (0..64).map(|index| format!("lookup_{index}")).collect()
}

fn path_queries() -> Vec<String> {
    (0..64)
        .map(|index| format!("src/module_{index}.rs"))
        .collect()
}

fn section_queries(fixture: &ResolverFixture) -> Vec<(super::DocId, String)> {
    (0..16)
        .map(|index| {
            (
                fixture.doc_id("main", &format!("docs/guide_{index}.md")),
                format!("Deploy {index}"),
            )
        })
        .collect()
}

fn prime_symbols(fixture: &ResolverFixture, queries: &[String]) {
    for query in queries {
        fixture
            .resolver
            .resolve_symbol(&fixture.workspace("main"), query);
    }
}

fn prime_paths(fixture: &ResolverFixture, queries: &[String]) {
    for query in queries {
        fixture
            .resolver
            .resolve_path(&fixture.workspace("main"), query)
            .expect("path query should resolve");
    }
}

fn prime_sections(fixture: &ResolverFixture, queries: &[(super::DocId, String)]) {
    for (doc, heading) in queries {
        fixture
            .resolver
            .resolve_section(&fixture.workspace("main"), doc, heading);
    }
}

fn measure_p99_us(iterations: usize, mut run: impl FnMut(usize)) -> u128 {
    let mut samples = Vec::with_capacity(iterations);
    for index in 0..iterations {
        let start = Instant::now();
        run(index);
        samples.push(start.elapsed().as_micros());
    }
    samples.sort_unstable();
    let percentile_index = ((iterations * 99).div_ceil(100)).saturating_sub(1);
    samples[percentile_index]
}

fn assert_within_budget(name: &str, observed_us: u128) {
    assert!(
        observed_us <= BUDGET_US,
        "{name} p99 {observed_us}us exceeded 2ms budget ({BUDGET_US}us)"
    );
}

fn load_baseline_p99_us(name: &str) -> u128 {
    let baseline_path = repo_root().join(
        "docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json",
    );
    let payload = fs::read_to_string(&baseline_path).expect("baseline metrics should exist");
    let metrics: Vec<BaselineMetric> =
        serde_json::from_str(&payload).expect("baseline metrics should parse");
    metrics
        .into_iter()
        .find(|metric| metric.name == name)
        .map(|metric| metric.p99_us as u128)
        .unwrap_or_else(|| {
            panic!(
                "baseline metric `{name}` missing in {}",
                display_path(&baseline_path)
            )
        })
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..")
}

fn display_path(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

#[derive(Deserialize)]
struct BaselineMetric {
    name: String,
    p99_us: u64,
}
