//! Measurement gate for C3 incremental graph maintenance.
//!
//! The production indexer currently rebuilds the complete graph after every
//! change. This benchmark compares that implementation with a scoped-update
//! candidate on deterministic 609-file and 5,000-file corpora. The candidate
//! models the indexes a production implementation would retain: the changed
//! file's replacement nodes and the already-resolved edges whose source or
//! target belongs to that file.
//!
//! This is deliberately a benchmark-local candidate, not a second runtime
//! graph implementation. Each fixture first proves that applying its update
//! produces the same nodes and edges as a full rebuild. If that assertion
//! stops holding as graph semantics evolve, the benchmark fails before it
//! records misleading timings.

use std::collections::BTreeSet;
use std::hint::black_box;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use lattice_core::graph::builder::GraphBuilder;
use lattice_core::graph::{CodeGraph, EdgeKind};
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
    let actual_nodes = actual
        .all_nodes()
        .into_iter()
        .map(|node| {
            (
                node.id.file.clone(),
                node.id.name.clone(),
                node.id.byte_offset,
                node.signature.to_string(),
                node.body.to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
    let expected_nodes = expected
        .all_nodes()
        .into_iter()
        .map(|node| {
            (
                node.id.file.clone(),
                node.id.name.clone(),
                node.id.byte_offset,
                node.signature.to_string(),
                node.body.to_string(),
            )
        })
        .collect::<BTreeSet<_>>();
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
            BenchmarkId::new("scoped_candidate_one_file", file_count),
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

criterion_group!(benches, bench_incremental_graph_maintenance);
criterion_main!(benches);
