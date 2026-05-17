# R10 — Backend review — Phase 1 identity substrate

**Review date:** 2026-05-17
**Reviewer:** R10 task executor (advanced model class)
**Spec anchor:** [Phase 1: Unified Identity Model](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model)
**Tasks reviewed:** T06, T07, T08, T09

This review certifies that the Phase 1 unified-identity substrate satisfies the
spec deliverables and the Cadres coding standard so Phase 2 (event log) can
build on stable references that survive renames, moves, branch changes, and
legacy compatibility paths.

## Spec alignment

The Phase 1 spec lists five deliverables and three definition-of-done items.
Each row cites the artifact and the literal symbol or test name that proves it.

| Spec deliverable | Evidence (file:line — symbol / test) |
|---|---|
| Stable ids — File | `daemon/crates/lattice-core/src/identity/kinds.rs:8` — `pub struct FileId { workspace_id, repo_relative_path, content_hash }` |
| Stable ids — Symbol | `daemon/crates/lattice-core/src/identity/kinds.rs:15` — `pub struct SymbolId { file, qualified_name, byte_offset, kind }` |
| Stable ids — Doc | `daemon/crates/lattice-core/src/identity/kinds.rs:23` — `pub struct DocId { workspace_id, repo_relative_path, content_hash }` |
| Stable ids — Section | `daemon/crates/lattice-core/src/identity/kinds.rs:30` — `pub struct SectionId { doc, heading_path, byte_offset }` |
| Stable ids — Event | `daemon/crates/lattice-core/src/identity/kinds.rs:37` — `pub struct EventId { workspace_id, ulid }` |
| Stable ids — Memory | `daemon/crates/lattice-core/src/identity/kinds.rs:43` — `pub struct MemoryId { workspace_id, ulid }` |
| Stable ids — ContextHandle | `daemon/crates/lattice-core/src/identity/kinds.rs:49` — `pub struct ContextHandleId { workspace_id, session_id, ulid }` |
| Identity resolver — paths | `daemon/crates/lattice-core/src/identity/resolver.rs:140` — `pub fn resolve_path(...)` |
| Identity resolver — symbols | `daemon/crates/lattice-core/src/identity/resolver.rs:157` — `pub fn resolve_symbol(...)` |
| Identity resolver — sections (headings) | `daemon/crates/lattice-core/src/identity/resolver.rs:174` — `pub fn resolve_section(...)` |
| Identity resolver — tests | `daemon/crates/lattice-core/src/identity/resolver.rs:196` — `pub fn resolve_test(...)` |
| Identity resolver — event refs | `daemon/crates/lattice-core/src/identity/resolver.rs:213` — `pub fn resolve_event_ref(...)` |
| Ambiguity diagnostics — three-arm outcome | `daemon/crates/lattice-core/src/identity/ambiguity.rs:12-16` — `enum ResolveOutcome { Unique, Ambiguous, NotFound }` |
| Ambiguity diagnostics — disambiguation hint | `daemon/crates/lattice-core/src/identity/ambiguity.rs:20-24` — `struct AmbiguityReport { query, candidates, disambiguation_hint }` |
| Ambiguity diagnostics — typed hint helpers | `daemon/crates/lattice-core/src/identity/ambiguity.rs:52,60,64` — `symbol_disambiguation_hint`, `section_disambiguation_hint`, `test_disambiguation_hint` |
| MCP serialization — payload struct | `daemon/crates/lattice-core/src/identity/serialization.rs:11-16` — `struct IdentityPayload { id, fields, legacy_name }` |
| MCP serialization — three-state envelope | `daemon/crates/lattice-core/src/identity/serialization.rs:27-42` — `enum IdentityPayloadOrAmbiguity { Resolved, Ambiguous, NotFound }` (tagged `status`) |
| MCP serialization — outcome serializer | `daemon/crates/lattice-core/src/identity/serialization.rs:97-122` — `pub fn serialize_outcome(...)` |
| Legacy compatibility shim — `SymbolId` migration | `daemon/crates/lattice-core/src/identity/kinds.rs:133-146` — `impl From<crate::symbols::SymbolId> for SymbolId` (default workspace `"legacy"`, default hash `"00000000"`) |
| Legacy compatibility shim — file rename via content hash | `daemon/crates/lattice-core/src/identity/resolver.rs:612-654` — `resolve_file_identity_compat` + `find_current_file_by_hash` |
| Legacy compatibility shim — `legacy_name` field preserved | `daemon/crates/lattice-core/src/identity/serialization.rs:15` + `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs:30-34,81-87,95-105` |
| MCP wiring — `get_context_capsule` decoration | `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs:15-50` — `pub fn decorate_get_context_capsule_payload` |
| MCP wiring — integration into RPC server | `daemon/crates/lattice-daemon/src/rpc/mcp.rs:1445-1451` — call site within `get_context_capsule` workflow |
| Test category 1 — file rename | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:30,44` — `test_resolve_path_after_file_rename_returns_new_id`, `test_resolve_path_after_file_rename_legacy_name_returns_via_compat_shim` |
| Test category 2 — section move | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:101,121` — `test_resolve_section_after_heading_rename_returns_new_id`, `test_resolve_section_after_section_move_returns_new_id` |
| Test category 3 — duplicate symbol names | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:85` — `test_resolve_symbol_duplicate_name_returns_ambiguous_with_disambiguation_hint` |
| Test category 4 — branch changes | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:141` — `test_resolve_symbol_on_different_branch_returns_branch_scoped_id` |
| Test category 5 — legacy compatibility | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:44,176` — `test_resolve_path_after_file_rename_legacy_name_returns_via_compat_shim`, `test_resolve_legacy_symbol_name_routes_via_default_workspace` |
| Test category 6 — event-ref round-trip | `daemon/crates/lattice-core/src/identity/resolver_tests.rs:157` — `test_resolve_event_ref_round_trips_through_encoding` |
| DoD — every workflow output expandable via stable identity | `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs:81-105,116-122` — `file_identity`/`symbol_identity`/`context_handle_identity` decorated onto pivots, context, stats, and handle of the `get_context_capsule` payload |
| DoD — ambiguous names return diagnostics | `daemon/crates/lattice-core/src/identity/resolver.rs:657-677` — `unique_or_ambiguous_symbol` returns `ResolveOutcome::Ambiguous` with hint; verified by `test_resolve_symbol_duplicate_name_returns_ambiguous_with_disambiguation_hint` |
| DoD — ≤ 2 ms P99 on hot-path tool calls | See `## P99 budget evidence` below — all three primitives at 1 µs P99, combined resolver hot path at 108 µs vs 2 462 µs allowed |
| T02 compatibility policy — `get_context_capsule` classification updated | `docs/architecture/2026-05-16-mcp-compatibility-policy.md:119-130` — dedicated `### get_context_capsule additive identity fields` subsection enumerates the four new fields and the legacy-preservation rule |

The seven `IdentityKind` variants are enumerated centrally at
`daemon/crates/lattice-core/src/identity/kinds.rs:55-64` (`enum IdentityKind`)
and the canonical sum type at `kinds.rs:66-75` (`enum Identity`). Encoding /
decoding for all seven kinds is exercised by
`identity::tests::round_trip_encoding_for_every_identity_kind`
(`tests.rs:13-38`).

## Coding-standard alignment

### Hard limits (file size)

| File | Lines | Limit | Status |
|---|---|---|---|
| `daemon/crates/lattice-core/src/identity/ambiguity.rs` | 97 | 800 | within |
| `daemon/crates/lattice-core/src/identity/budget_tests.rs` | 235 | 800 | within |
| `daemon/crates/lattice-core/src/identity/encoding.rs` | 329 | 800 | within |
| `daemon/crates/lattice-core/src/identity/kinds.rs` | 174 | 800 | within |
| `daemon/crates/lattice-core/src/identity/mod.rs` | 32 | 800 | within |
| `daemon/crates/lattice-core/src/identity/resolver.rs` | 732 | 800 | within (largest — 68 lines of headroom) |
| `daemon/crates/lattice-core/src/identity/resolver_tests.rs` | 366 | 800 | within |
| `daemon/crates/lattice-core/src/identity/serialization.rs` | 319 | 800 | within |
| `daemon/crates/lattice-core/src/identity/tests.rs` | 179 | 800 | within |
| `daemon/crates/lattice-core/src/identity/resolver/tests.rs` | 141 | 800 | within (see Findings F-1) |
| `daemon/crates/lattice-daemon/src/rpc/identity_payload.rs` | 505 | 800 | within |
| **Total** | **3 109** | — | — |

`resolver.rs` is the only file approaching the limit at 732 lines. The file is
internally coherent (resolver struct, `ResolveError`, LRU cache, six `resolve_*`
methods, helpers) but is close enough that the next non-trivial feature should
trigger an extraction (e.g., move the cache and normalization helpers into
sibling modules). Recorded as Finding F-2.

### Forbidden tokens

Greps run across the eleven Phase 1 source files:

| Pattern | Hits | Notes |
|---|---|---|
| `TODO` / `FIXME` / `XXX` | 0 | clean |
| `#[allow(...)]` | 0 | no rule suppressions |
| commented-out code (e.g., `// let `, `// fn `, `// pub `) | 0 | clean |
| `panic!` / `todo!` / `unimplemented!` | 7 total — all inside `#[cfg(test)]` test modules (assertion helpers and `match` exhaustion in test bodies); zero in production paths | acceptable |
| `unwrap()` / `expect(` in production paths | 1 — `resolver.rs:453` `self.latest_event_id.clone().expect("checked above")` after an explicit guard (`event_index.values().any(...) && self.event_ref_appears_ahead_of_index(...)` ensures `latest_event_id` is `Some`). The `.expect` documents the proof of non-emptiness. | acceptable |

### Suppression audit

Zero `#[allow(...)]`, `#[expect(...)]`, `#[cfg_attr(..., allow(...))]`, or other
suppressions across the eleven Phase 1 files. The full workspace builds clean
under `cargo build --release` after a forced rebuild (`touch crates/lattice-core/src/lib.rs`)
— zero warnings, zero errors, zero notes.

### Error typing (public API)

The only `Result<_, String>` returns are two **private** helpers inside
`serialization.rs` (`decode_identity_fields` line 146 and `decode_struct` line
164). Both are wrapped at the trait boundary with `.map_err(de::Error::custom)`,
so no bare `String` errors leak through the public API. The public errors are:

- `IdentityDecodeError` (`encoding.rs:7-18`) — four typed variants
  (`Empty`, `MissingPrefix`, `MalformedField{ field, reason }`).
- `ResolveError` (`resolver.rs:40-56`) — four typed variants
  (`WorkspaceNotIndexed`, `Malformed`, `NotFound`, `IndexLagBehind`).

Both derive `thiserror::Error`, expose structured fields, and round-trip into
the MCP envelope via `not_found_kind` / `not_found_query` in
`serialization.rs:171-187`.

### Naming audit (public API surface)

All 31 public items follow the Cadres conventions (PascalCase types,
snake_case fns, intent-first names, no generic containers):

- Types: `FileId`, `SymbolId`, `DocId`, `SectionId`, `EventId`, `MemoryId`,
  `ContextHandleId`, `Identity`, `IdentityKind`, `AmbiguityReport`,
  `ResolveOutcome`, `IdentityPayload`, `IdentityAmbiguityPayload`,
  `IdentityPayloadOrAmbiguity`, `IdentityDecodeError`, `IdentityResolver`,
  `ResolveError`, `WorkspaceId` — all noun-shaped, intent-clear.
- Fns: `encode_identity`, `decode_identity`, `serialize_outcome`,
  `resolve_path`, `resolve_symbol`, `resolve_section`, `resolve_test`,
  `resolve_event_ref`, `resolve_legacy_symbol_name`,
  `symbol_disambiguation_hint`, `section_disambiguation_hint`,
  `test_disambiguation_hint` — verbs that read as actions.
- Booleans and constructors not present in this surface.

No abbreviation drift, no `data`/`info`/`result`/`obj` placeholders.

### Documentation citations

Module-level doc comments in `mod.rs`, `ambiguity.rs`, `resolver.rs`,
`resolver_tests.rs`, and `budget_tests.rs` all cite
`docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## Phase 1: Unified
Identity Model`. The plan file renders the heading as `### Phase 1: Unified
Identity Model` (line 593); the `##`/`###` hash count is the markdown level
indicator, not part of the heading text, so the citation text matches exactly.
Anchor lookup (`#phase-1-unified-identity-model`) resolves correctly under the
plan's H3.

`resolver.rs` and `mod.rs` additionally cite
`docs/architecture/2026-04-11-stable-follow-up-handles.md` `## Contract` and
`## Assistant-Facing Surfaces`, both of which exist in that doc.

### Tests as documentation

Each spec-mandated test name reads as a behavioral sentence
(`test_resolve_path_after_file_rename_legacy_name_returns_via_compat_shim`,
`test_resolve_symbol_duplicate_name_returns_ambiguous_with_disambiguation_hint`,
…), and bodies are short — the longest is 21 lines (well inside the 30-line
ceiling). The fixture builder in `resolver_tests::helpers` keeps individual
tests focused on the assertion rather than setup, satisfying the standard.

## P99 budget evidence

Spec DoD requires identity resolution to add no more than 2 ms P99 to hot-path
tool calls. Tests were re-run in `--release` with `--include-ignored` on
10 000 iterations per call. To capture measured µs values, a temporary
`println!` was added inside `assert_within_budget` and the combined-workflow
assertion, runs were recorded, and the print was reverted (final test bytes
unchanged from T09's drop, re-verified by a second `--release` run).

| Metric | Measured P99 | 2 ms target | T04 baseline reference | Verdict |
|---|---|---|---|---|
| `identity::budget_tests::test_resolve_symbol_p99_within_2ms_budget` | **1 µs** | 2 000 µs | n/a (new) | within budget |
| `identity::budget_tests::test_resolve_path_p99_within_2ms_budget` | **1 µs** | 2 000 µs | n/a (new) | within budget |
| `identity::budget_tests::test_resolve_section_p99_within_2ms_budget` | **1 µs** | 2 000 µs | n/a (new) | within budget |
| `identity::budget_tests::test_resolver_does_not_regress_baseline_workflow_p99` (4-call combined hot path: symbol + path + section + event ref) | **108 µs** | baseline `prepare_change` p99 = **462 µs** + 2 000 µs budget = **2 462 µs** allowed | `baselines/baseline_metrics.json` `prepare_change` p99 = 462 µs (commit `b85dee7`) | within budget — combined hot path is below the baseline workflow itself |

Run command:
```
cd daemon && cargo test --release -p lattice-core --lib identity::budget_tests \
    -- --include-ignored --nocapture --test-threads=1
```

Note: the single-primitive numbers (1 µs) are cache-hit measurements — the
budget tests prime queries before measurement, which models the realistic case
where the resolver is hit repeatedly during a workflow. Cold-path performance
is implicit in the combined-workflow test (108 µs across four distinct calls,
~27 µs per call including cache miss for the rotating index).

Baseline reference: `docs/plans/2026-05-16-cognitive-workspace-fork-build/baselines/baseline_metrics.json` lines 53-60 (`prepare_change`).

### Other verification commands

| Command | Result |
|---|---|
| `cd daemon && cargo build --release` (after `touch crates/lattice-core/src/lib.rs`) | 0 warnings, 0 errors — clean rebuild in 59.82 s |
| `cd daemon && cargo test -p lattice-core --lib identity::tests` | 10 passed, 0 failed, 0 ignored |
| `cd daemon && cargo test -p lattice-core --lib identity::resolver_tests` | 10 passed, 0 failed, 0 ignored |
| `cd daemon && cargo test -p lattice-core --lib identity::budget_tests -- --include-ignored` | 4 passed, 0 failed |
| `cd daemon && cargo test --workspace -- identity::serialization rpc::identity_payload` | 7 passed (5 serialization + 2 rpc), 0 failed |
| `cd daemon && cargo test --workspace` (full regression sweep) | 253 passed, 0 failed, 18 ignored across both crates |

Named tests, per spec procedure step 2:

- `identity::tests::round_trip_encoding_for_every_identity_kind` — PASS
- `identity::tests::identity_kind_reports_wrapped_variant` — PASS
- `identity::tests::display_uses_compact_wire_encoding` — PASS
- `identity::tests::equality_semantics_include_stability_fields` — PASS
- `identity::tests::hash_is_deterministic_for_same_inputs` — PASS
- `identity::tests::decode_rejects_empty_identity_string` — PASS
- `identity::tests::decode_rejects_missing_prefix` — PASS
- `identity::tests::decode_rejects_malformed_hash` — PASS
- `identity::tests::decode_rejects_malformed_ulid` — PASS
- `identity::tests::legacy_symbol_id_migrates_to_unified_symbol_id` — PASS
- `identity::resolver_tests::test_resolve_path_unique_returns_fileid` — PASS
- `identity::resolver_tests::test_resolve_path_after_file_rename_returns_new_id` — PASS
- `identity::resolver_tests::test_resolve_path_after_file_rename_legacy_name_returns_via_compat_shim` — PASS
- `identity::resolver_tests::test_resolve_symbol_unique_returns_symbolid` — PASS
- `identity::resolver_tests::test_resolve_symbol_duplicate_name_returns_ambiguous_with_disambiguation_hint` — PASS
- `identity::resolver_tests::test_resolve_section_after_heading_rename_returns_new_id` — PASS
- `identity::resolver_tests::test_resolve_section_after_section_move_returns_new_id` — PASS
- `identity::resolver_tests::test_resolve_symbol_on_different_branch_returns_branch_scoped_id` — PASS
- `identity::resolver_tests::test_resolve_event_ref_round_trips_through_encoding` — PASS
- `identity::resolver_tests::test_resolve_legacy_symbol_name_routes_via_default_workspace` — PASS

## Findings

### F-1 (minor) — Duplicate resolver test module

**Severity:** minor (single-source-of-truth violation; not a correctness risk).

`daemon/crates/lattice-core/src/identity/resolver/tests.rs` (141 lines, 3 test
fns) is declared via `mod tests;` at the foot of `resolver.rs:731`. Its three
tests (`resolve_path_returns_stable_file_id`,
`resolve_symbol_reports_ambiguity_for_duplicate_names`,
`resolve_legacy_symbol_name_uses_default_workspace`) restate behaviors already
covered by the spec-mandated `resolver_tests.rs` (#1, #5, #10 in the test
matrix). It looks like an earlier T07 scaffold that was kept when T09 landed
the spec matrix.

The Cadres standard ("duplicate detection before writing", "if you catch
yourself duplicating, stop and extract") asks us to dedupe. Cleanup is small:
delete the `resolver/tests.rs` file and remove the `mod tests;` declaration
from `resolver.rs:731`. No production code references the inner module.

Follow-up filed as **T-followup-R10-A** in this build's `tasks/` directory (see Verdict).

### F-2 (minor) — `resolver.rs` is the only file approaching the 800-line ceiling

**Severity:** minor (heuristic, not a violation).

732 lines today, 68 lines of headroom. The file is internally coherent —
resolver struct, error type, LRU cache, six `resolve_*` methods, normalization
helpers — but Phase 2 will add event-log integration paths to the resolver
(per spec line 619 "MCP/tool-call event capture using stable identities from
Phase 1"). The natural split is:

- `resolver/cache.rs` for `ResolverCache` + `CacheKey` + `CacheValue` (~55 lines)
- `resolver/normalize.rs` for the bottom helpers (`normalize_path`,
  `normalize_symbol_name`, `normalize_heading`, `is_test_node`,
  `is_canonical_ulid`, `split_symbol_target`, `looks_like_path`,
  `unique_or_ambiguous_symbol`) (~80 lines)

Pre-emptive split is not required for PASS; flagging so Phase 2 doesn't blow
the limit on first touch. Captured in **T-followup-R10-A**.

### F-3 (minor) — `resolve_test` is public but untested

**Severity:** minor (test-coverage gap on a public API surface explicitly
listed in the spec).

`IdentityResolver::resolve_test` is implemented at `resolver.rs:196-211` and
exposed transitively via `IdentityResolver` in `mod.rs:29`. The spec lists
"identity resolver for paths, symbols, headings, **tests**, and event refs" as
a Phase 1 deliverable. The 10-case `resolver_tests.rs` matrix covers path,
symbol, section, event — but no case exercises `resolve_test` directly.

The method's behavior (filtering for test-shaped names against the graph, then
returning `Unique` / `Ambiguous(test_disambiguation_hint)` / `NotFound`) is
not regression-protected. The standard ("every bug gets a regression test in
the same commit" — and by extension, every public method gets a behavior
test) is breached.

Add two cases in **T-followup-R10-A**: `test_resolve_test_unique_returns_symbolid` and
`test_resolve_test_duplicate_name_returns_ambiguous_with_test_hint`.

### F-4 (minor) — `legacy_name` is best-effort, not contract-enforced

**Severity:** minor (documentation gap, not a behavioral bug).

`IdentityPayload::new` (`serialization.rs:45`) accepts
`legacy_name: Option<String>`. Callers in `identity_payload.rs` always pass
`Some(...)` for `get_context_capsule` decoration (good), but nothing in the
type system or a doctest enforces "legacy fields remain populated for one full
phase cycle" from the compatibility policy. A future caller could pass `None`
and silently break the contract.

Lightweight remediation: add a `#[must_use]` doc-comment on
`IdentityPayload::new` referencing the compatibility-policy heading, plus a
single negative test that constructs a payload **without** `legacy_name` and
asserts the wire shape lacks the field (so any future change to the wire
contract trips the assertion). Captured in **T-followup-R10-A**.

### F-5 (informational) — `encoding.rs` ULID validator vs `resolver.rs::is_canonical_ulid`

**Severity:** informational, no action needed in Phase 1.

`encoding.rs:261-270` (`validate_ulid`) and `resolver.rs:724-729`
(`is_canonical_ulid`) implement Crockford-base32 ULID validation with slightly
different acceptance sets:

- `encoding::validate_ulid` is restrictive: it accepts only Crockford's official
  base32 alphabet (`0-9`, `A-H`, `J-K`, `M-N`, `P-T`, `V-Z`).
- `resolver::is_canonical_ulid` is permissive: it accepts `0-9`, `A-Z` minus
  `I`, `L`, `O`, `U`. This admits `H`, `J`, `K`, `M`, `N`, `P`, `Q`, `R`, `S`,
  `T`, `V`, `W`, `X`, `Y`, `Z` plus a few extras that Crockford forbids
  (notably `Q` and `S` only differ subtly).

They are functionally equivalent on canonical ULIDs but differ at the edges.
Phase 2 will write ULIDs through the canonical ULID crate (per the spec event
log), at which point both validators should defer to that crate. Not a Phase 1
blocker — flagged so Phase 2 reviewers don't introduce a third copy.

## Verdict

**PASS-WITH-FOLLOWUP-TASK-T-followup-R10-A**

All Phase 1 spec deliverables are implemented, every spec-mandated test passes,
the 2 ms P99 budget is met with three orders of magnitude of headroom (1 µs on
primed primitives, 108 µs on the combined hot path vs the 2 462 µs allowance),
and the eleven Phase 1 source files satisfy every hard limit, suppression rule,
naming convention, and citation requirement in the Cadres coding standard.

The findings (F-1 through F-4) are minor — duplicate test module, near-ceiling
file, one untested public method, and a soft `legacy_name` contract. None
blocks Phase 2 (event log) from beginning. F-5 is informational only.

A follow-up task **T-followup-R10-A — Phase 1 identity substrate cleanup** is
opened in this build at
`docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/T-followup-R10-A.md`
to land the four minor items before Phase 2 work touches the resolver,
satisfying the "never defer outside this build" rule in the R10 task spec
(`docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/R10.md` line 54).
The task ID follows the `T-followup-<review>-<letter>` convention established
by the prior follow-up at
`docs/plans/2026-05-16-cognitive-workspace-fork-build/tasks/T-followup-R05-A.md`
because the canonical numeric slot `T11` is already assigned to a Phase 2
event-model task in the build.
