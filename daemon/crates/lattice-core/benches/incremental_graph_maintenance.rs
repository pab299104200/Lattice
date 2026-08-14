//! Measurement gate for C3 incremental graph maintenance.
//!
//! The production [`Indexer`](lattice_core::indexer::Indexer) now performs a
//! bounded rebuild of the affected dependency component. This benchmark keeps
//! two deliberately lower-level measurements on deterministic 609-file and
//! 5,000-file corpora: a complete [`GraphBuilder`] rebuild and the graph
//! mutation portion of a one-file scoped update. The latter is a lower bound,
//! not a timing claim for the public indexer; it excludes parsing, dependency
//! closure discovery, and the component-local `GraphBuilder` pass performed
//! by `Indexer::index_file_content`.
//!
//! Keeping the lower-bound fixture is useful for detecting graph-mutation
//! regressions without making a benchmark-sized synthetic parser workload a
//! requirement. Each fixture first proves that applying its update produces
//! the same nodes and edges as a full rebuild. If that assertion stops holding
//! as graph semantics evolve, the benchmark fails before it records
//! misleading timings.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use lattice_core::git_intelligence::GitIntelligenceSnapshot;
use lattice_core::graph::builder::GraphBuilder;
use lattice_core::graph::{CodeGraph, EdgeKind};
use lattice_core::health::churn_facts::line_churn_fact;
use lattice_core::health::complexity_facts::compute_file_complexity_facts;
use lattice_core::health::complexity_facts::FileComplexityFacts;
use lattice_core::health::dead_symbol_facts::{
    DeadSymbolExclusionInputs, DeadSymbolFactProducer, DeadSymbolFactsSnapshot,
};
use lattice_core::health::graph_facts::{GraphFactProducer, GraphFactsSnapshot};
use lattice_core::health::scoring::HealthFactIndex;
use lattice_core::health::test_proximity_facts::{
    TestProximityFactProducer, TestProximitySnapshot,
};
use std::collections::BTreeMap;
use lattice_core::symbols::{Language, ParsedFile, Symbol, SymbolId, SymbolKind};

const REPO_FILE_COUNT: usize = 609;
const LARGE_FILE_COUNT: usize = 5_000;
const CHANGED_FILE_INDEX: usize = 307;
const SYMBOLS_PER_FILE: usize = 6;

#[derive(Clone)]
struct ScopedUpdatePlan {
    changed_file: String,
    replacement: ParsedFile,
    touching_edges: Vec<(SymbolId, SymbolId, EdgeKind)>,
}

struct Fixture {
    files: Vec<ParsedFile>,
    baseline: CodeGraph,
    update: ScopedUpdatePlan,
}

impl Fixture {
    fn new(file_count: usize) -> Self {
        let files = synthetic_corpus(file_count);
        let baseline = GraphBuilder::build_from_files(files.iter());

        let changed_index = CHANGED_FILE_INDEX.min(file_count - 1);
        let mut updated_files = files.clone();
        let changed_file = updated_files[changed_index].file.clone();
        let replacement =
            changed_file_version(&updated_files[changed_index], changed_index, file_count);
        updated_files[changed_index] = replacement.clone();

        // In the intended runtime design, the resolver retains endpoint
        // indexes and returns only edges touching changed files. Derive that
        // bounded plan once here so the timed update measures graph mutation,
        // not another hidden full rebuild.
        let expected = GraphBuilder::build_from_files(updated_files.iter());
        let touching_edges = expected
            .all_edges()
            .into_iter()
            .filter(|(from, to, _)| from.file == changed_file || to.file == changed_file)
            .map(|(from, to, kind)| (from.id.clone(), to.id.clone(), kind))
            .collect();

        let update = ScopedUpdatePlan {
            changed_file,
            replacement,
            touching_edges,
        };

        let mut candidate = baseline.clone();
        apply_scoped_update(&mut candidate, &update);
        assert_graph_equivalent(&candidate, &expected);

        Self {
            files: updated_files,
            baseline,
            update,
        }
    }
}

fn synthetic_corpus(file_count: usize) -> Vec<ParsedFile> {
    (0..file_count)
        .map(|index| {
            let file = format!("src/module_{index:05}.rs");
            let symbols = (0..SYMBOLS_PER_FILE)
                .map(|local_index| {
                    let name = format!("symbol_{index:05}_{local_index}");
                    let mut references = Vec::new();
                    if local_index > 0 {
                        references.push(format!("symbol_{index:05}_{}", local_index - 1));
                    }
                    if index > 0 && local_index == 0 {
                        references.push(format!("symbol_{:05}_0", index - 1));
                    }
                    synthetic_symbol(
                        file.clone(),
                        name,
                        references,
                        index,
                        local_index,
                        "initial",
                    )
                })
                .collect();

            ParsedFile {
                file: file.clone(),
                language: Language::Rust,
                symbols,
                imports: Vec::new(),
                links: Vec::new(),
            }
        })
        .collect()
}

fn changed_file_version(
    original: &ParsedFile,
    changed_index: usize,
    file_count: usize,
) -> ParsedFile {
    let mut replacement = original.clone();
    let symbol = &mut replacement.symbols[0];
    let new_target = (changed_index + 17) % file_count;
    symbol.signature = format!("pub fn {}() -> usize", symbol.name);
    symbol.body = format!("{{ symbol_{new_target:05}_0(); 1 }}");
    symbol.references = vec![format!("symbol_{new_target:05}_0")];
    replacement
}

fn synthetic_symbol(
    file: String,
    name: String,
    references: Vec<String>,
    file_index: usize,
    local_index: usize,
    version: &str,
) -> Symbol {
    Symbol {
        id: SymbolId {
            file: file.clone(),
            name: name.clone(),
            byte_offset: file_index * 1_000 + local_index * 16,
        },
        kind: SymbolKind::Function,
        name: name.clone(),
        signature: format!("pub fn {name}()"),
        body: format!("{{ /* {version} */ }}"),
        file,
        line: 1,
        end_line: 3,
        is_exported: true,
        language: Language::Rust,
        references,
        imports: Vec::new(),
    }
}

fn apply_scoped_update(graph: &mut CodeGraph, update: &ScopedUpdatePlan) {
    graph.remove_file_nodes(&update.changed_file);
    for symbol in &update.replacement.symbols {
        graph.add_node(
            symbol.id.clone(),
            symbol.kind,
            symbol.name.clone(),
            symbol.signature.clone(),
            symbol.body.clone(),
            symbol.file.clone(),
            symbol.line,
            symbol.end_line,
            symbol.is_exported,
            symbol.language,
        );
    }
    for (from, to, kind) in &update.touching_edges {
        graph.add_edge(from, to, *kind);
    }
}

fn assert_graph_equivalent(actual: &CodeGraph, expected: &CodeGraph) {
    let mut actual_nodes = actual.all_nodes().into_iter().cloned().collect::<Vec<_>>();
    let mut expected_nodes = expected
        .all_nodes()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    let by_id = |left: &lattice_core::graph::GraphNode, right: &lattice_core::graph::GraphNode| {
        (&left.id.file, &left.id.name, left.id.byte_offset).cmp(&(
            &right.id.file,
            &right.id.name,
            right.id.byte_offset,
        ))
    };
    actual_nodes.sort_by(by_id);
    expected_nodes.sort_by(by_id);
    assert_eq!(actual_nodes, expected_nodes, "scoped candidate node drift");

    let canonical_edges = |graph: &CodeGraph| {
        graph
            .all_edges()
            .into_iter()
            .map(|(from, to, kind)| {
                (
                    from.id.file.clone(),
                    from.id.name.clone(),
                    to.id.file.clone(),
                    to.id.name.clone(),
                    kind.short_code(),
                )
            })
            .collect::<BTreeSet<_>>()
    };
    assert_eq!(
        canonical_edges(actual),
        canonical_edges(expected),
        "scoped candidate edge drift"
    );
}

/// Budget for one single-file incremental update, from
/// `docs/plans/2026-02-25-lattice-design.md` § "Performance Targets"
/// ("Incremental update (single file) | < 200ms").
///
/// Phase H5 of `docs/plans/2026-08-13-health-engine.md` requires proof that
/// producing the H2 fact families fits inside that budget. Fact publishing is
/// only one part of an incremental update — parsing and the graph pass also
/// have to fit — so this benchmark holds fact production to a fraction of the
/// whole-update budget rather than to all of it. Exceeding the fraction is not
/// automatically a correctness failure, but it means health facts have started
/// consuming an unreasonable share of the deadline, which is the regression
/// this gate exists to catch.
const INCREMENTAL_UPDATE_BUDGET: Duration = Duration::from_millis(200);

/// Share of the whole-update budget that fact publishing may consume at this
/// repository's scale.
///
/// # Why the gate is scale-dependent
///
/// Every H2 producer except complexity is a *whole-graph* pass: an incremental
/// single-file update republishes facts for the entire corpus and then diffs
/// the snapshots. Publishing cost therefore tracks corpus size, not change
/// size, and the headroom under a fixed per-update budget narrows as a
/// repository grows. Measured on an Apple M-series machine, the full publish
/// takes ~10.8 ms over 609 files but ~96.4 ms over 5,000 — comfortably inside
/// the budget at this repository's scale and close to consuming it whole an
/// order of magnitude up.
///
/// `docs/plans/2026-08-13-health-engine.md` § "Phase H5 — Prove it stays
/// honest" asks for the publish to be "within the index deadline bounds on
/// this repo", so the strict half-budget gate is applied at repository scale.
/// The large corpus is held to the full update budget instead, and is here to
/// record the scaling curve rather than to certify a repository size Lattice
/// has not been measured against. Making the large-corpus publish incremental
/// rather than whole-graph is the fix if that number ever needs to come down;
/// it is deliberately not attempted here, because nothing measured says it is
/// needed yet.
const HEALTH_FACT_BUDGET_SHARE: u32 = 2;

/// Everything the health engine persists for one generation.
///
/// The H2 producers are whole-graph passes, so an incremental single-file
/// update republishes them against the updated graph and diffs the result
/// (`GraphFactsSnapshot::file_delta` and friends). Only complexity facts are
/// genuinely per-file, and only the changed file's are recomputed. This is the
/// same composition the H3 fact index is built from — see
/// `docs/architecture/2026-08-13-health-engine.md`.
fn publish_health_facts(
    graph: &CodeGraph,
    complexity: &BTreeMap<String, FileComplexityFacts>,
    git: &GitIntelligenceSnapshot,
) -> HealthFactIndex {
    HealthFactIndex::builder()
        .with_graph_facts(GraphFactProducer::default().produce(graph, true))
        .with_git_intelligence(git.clone())
        .with_complexity_facts(complexity.clone())
        .with_test_proximity_facts(TestProximityFactProducer::default().produce(graph, true))
        .with_dead_symbol_facts(DeadSymbolFactProducer::default().produce(
            graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        ))
        .build()
}

/// Synthetic source text for a corpus file, so complexity facts have something
/// to parse. Shaped to carry real branch and nesting structure rather than a
/// trivial body, so the complexity pass is not measured against an empty file.
fn synthetic_source(file_index: usize) -> String {
    let mut source = String::new();
    for local_index in 0..SYMBOLS_PER_FILE {
        source.push_str(&format!(
            "pub fn symbol_{file_index:05}_{local_index}(input: usize) -> usize {{\n\
             \x20   if input > {local_index} {{\n\
             \x20       for step in 0..input {{\n\
             \x20           if step % 2 == 0 && input > 3 {{\n\
             \x20               return step;\n\
             \x20           }}\n\
             \x20       }}\n\
             \x20   }}\n\
             \x20   match input {{\n\
             \x20       0 => 1,\n\
             \x20       _ => input,\n\
             \x20   }}\n\
             }}\n\n"
        ));
    }
    source
}

/// Baseline complexity facts for a whole corpus.
fn corpus_complexity(file_count: usize) -> BTreeMap<String, FileComplexityFacts> {
    (0..file_count)
        .map(|index| {
            let path = format!("src/module_{index:05}.rs");
            let facts = lattice_core::health::complexity_facts::compute_file_complexity_facts(
                &path,
                &synthetic_source(index),
            );
            (path, facts)
        })
        .collect()
}

/// The read path as it behaved before published generations were served: every
/// graph-derived family recomputed in process, per request.
///
/// This is exactly what `rpc::mcp::health_fact_index` did for every `impact`,
/// `context`, `prepare_change`, `diagnose` and `status` call, and it is the
/// number the store-backed path has to beat.
fn build_index_from_live_graph(
    graph: &CodeGraph,
    git: &GitIntelligenceSnapshot,
) -> HealthFactIndex {
    HealthFactIndex::builder()
        .with_graph_facts(GraphFactProducer::default().produce(graph, true))
        .with_git_intelligence(git.clone())
        .with_test_proximity_facts(TestProximityFactProducer::default().produce(graph, true))
        .with_dead_symbol_facts(DeadSymbolFactProducer::default().produce(
            graph,
            &DeadSymbolExclusionInputs::default(),
            true,
        ))
        .build()
}

/// The read path once a generation has been published: no whole-graph pass at
/// all, only `Arc` clones and the ranking join.
///
/// Complexity facts appear here and cannot appear above, because producing
/// them means re-parsing file contents, which request handling must not do.
/// The store-backed path is therefore both cheaper *and* strictly better
/// evidenced (`docs/architecture/2026-08-13-health-engine.md`).
fn build_index_from_published(
    published: &PublishedFixture,
    git: &GitIntelligenceSnapshot,
) -> HealthFactIndex {
    HealthFactIndex::builder()
        .with_shared_graph_facts(Arc::clone(&published.graph))
        .with_git_intelligence(git.clone())
        .with_shared_complexity_facts(Arc::clone(&published.complexity))
        .with_shared_test_proximity_facts(Arc::clone(&published.test_proximity))
        .with_shared_dead_symbol_facts(Arc::clone(&published.dead_symbols))
        .build()
}

struct PublishedFixture {
    graph: Arc<GraphFactsSnapshot>,
    complexity: Arc<BTreeMap<String, FileComplexityFacts>>,
    test_proximity: Arc<TestProximitySnapshot>,
    dead_symbols: Arc<DeadSymbolFactsSnapshot>,
}

/// What one verb request pays to obtain a fact index.
///
/// H5 measured *publication*, which this change does not make cheaper: the
/// three graph-derived families are still whole-graph passes, now coalesced to
/// the watcher's settling window instead of running per request. What changed
/// is the read path, so that is what this measures — the live-graph
/// recomputation every request used to perform against the `Arc`-clone the
/// published handoff serves instead.
fn bench_health_index_read_path(c: &mut Criterion) {
    let mut group = c.benchmark_group("health_index_read_path");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(20);

    for file_count in [REPO_FILE_COUNT, LARGE_FILE_COUNT] {
        let fixture = Fixture::new(file_count);
        let git = GitIntelligenceSnapshot::empty();
        let graph = fixture.baseline.clone();

        let published = PublishedFixture {
            graph: Arc::new(GraphFactProducer::default().produce(&graph, true)),
            complexity: Arc::new(corpus_complexity(file_count)),
            test_proximity: Arc::new(TestProximityFactProducer::default().produce(&graph, true)),
            dead_symbols: Arc::new(DeadSymbolFactProducer::default().produce(
                &graph,
                &DeadSymbolExclusionInputs::default(),
                true,
            )),
        };

        // Both paths must cover the same population, or the comparison would
        // be measuring two different amounts of work rather than two ways of
        // obtaining the same answer.
        assert_eq!(
            build_index_from_live_graph(&graph, &git).file_count(),
            build_index_from_published(&published, &git).file_count(),
            "both read paths must score the same file population"
        );

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("live_graph_recomputation", file_count),
            &(&graph, &git),
            |bencher, (graph, git)| {
                bencher.iter(|| black_box(build_index_from_live_graph(graph, git)));
            },
        );
        group.bench_with_input(
            BenchmarkId::new("published_generation", file_count),
            &(&published, &git),
            |bencher, (published, git)| {
                bencher.iter(|| black_box(build_index_from_published(published, git)));
            },
        );
    }

    group.finish();
}

fn bench_health_facts_publish(c: &mut Criterion) {
    let mut group = c.benchmark_group("health_facts_publish");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(20);

    for file_count in [REPO_FILE_COUNT, LARGE_FILE_COUNT] {
        // Strict at this repository's scale, which is what the spec asks to be
        // certified; the large corpus records the scaling curve against the
        // whole-update budget. See `HEALTH_FACT_BUDGET_SHARE`.
        let budget = if file_count == REPO_FILE_COUNT {
            INCREMENTAL_UPDATE_BUDGET / HEALTH_FACT_BUDGET_SHARE
        } else {
            INCREMENTAL_UPDATE_BUDGET
        };
        let fixture = Fixture::new(file_count);
        let git = GitIntelligenceSnapshot::empty();

        // The graph as it stands after the one-file scoped update, which is
        // the state an incremental republish actually sees.
        let mut updated_graph = fixture.baseline.clone();
        apply_scoped_update(&mut updated_graph, &fixture.update);

        let changed_index = CHANGED_FILE_INDEX.min(file_count - 1);
        let mut complexity = corpus_complexity(file_count);
        complexity.insert(
            fixture.update.changed_file.clone(),
            lattice_core::health::complexity_facts::compute_file_complexity_facts(
                &fixture.update.changed_file,
                &synthetic_source(changed_index),
            ),
        );

        // Prove the publish is within budget before recording any timing, in
        // the same spirit as the correctness assertions above: a benchmark
        // that silently records an over-budget number is worse than one that
        // fails. Measured after one warm run so the figure is not dominated by
        // first-touch allocation.
        let _ = publish_health_facts(&updated_graph, &complexity, &git);
        let started = std::time::Instant::now();
        let published = publish_health_facts(&updated_graph, &complexity, &git);
        let elapsed = started.elapsed();
        assert_eq!(
            published.file_count(),
            file_count,
            "health fact publish should cover every file in the corpus"
        );
        assert!(
            elapsed <= budget,
            "publishing all health fact families for a single-file incremental \
             update of a {file_count}-file corpus took {elapsed:?}, over the \
             {budget:?} share of the {INCREMENTAL_UPDATE_BUDGET:?} incremental-update \
             budget in docs/plans/2026-02-25-lattice-design.md"
        );

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("all_families_one_file_update", file_count),
            &(updated_graph, complexity, git),
            |bencher, (graph, complexity, git)| {
                bencher.iter(|| {
                    black_box(publish_health_facts(
                        black_box(graph),
                        black_box(complexity),
                        black_box(git),
                    ))
                });
            },
        );
    }

    group.finish();
}

fn bench_incremental_graph_maintenance(c: &mut Criterion) {
    let mut group = c.benchmark_group("incremental_graph_maintenance");
    group.warm_up_time(Duration::from_secs(1));
    group.measurement_time(Duration::from_secs(4));
    group.sample_size(20);

    for file_count in [REPO_FILE_COUNT, LARGE_FILE_COUNT] {
        let fixture = Fixture::new(file_count);
        group.throughput(Throughput::Elements(file_count as u64));

        group.bench_with_input(
            BenchmarkId::new("full_rebuild", file_count),
            &fixture,
            |bencher, fixture| {
                bencher.iter(|| {
                    black_box(GraphBuilder::build_from_files(black_box(
                        fixture.files.iter(),
                    )))
                });
            },
        );

        group.throughput(Throughput::Elements(1));
        group.bench_with_input(
            BenchmarkId::new("scoped_graph_mutation_lower_bound_one_file", file_count),
            &fixture,
            |bencher, fixture| {
                bencher.iter_batched(
                    || fixture.baseline.clone(),
                    |mut graph| {
                        apply_scoped_update(&mut graph, black_box(&fixture.update));
                        black_box(graph)
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_incremental_graph_maintenance,
    bench_health_facts_publish,
    bench_health_index_read_path
);
criterion_main!(benches);
