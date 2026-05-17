# T-followup-R10-A — Phase 1 identity substrate cleanup

**Phase:** 1 (follow-up from R10)
**Type:** backend cleanup + test coverage
**Model class:** balanced
**Depends on:** R10 (PASS-WITH-FOLLOWUP)
**Opened by:** R10 (Phase 1 identity substrate review)
**Spec anchor:** [§Phase 1: Unified Identity Model](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model)
**Standards:** Cadres coding standard §Hard limits, §Single source of truth, §No broken windows, §Tests as documentation; [MCP Compatibility Policy §`get_context_capsule` additive identity fields](../../../architecture/2026-05-16-mcp-compatibility-policy.md#get_context_capsule-additive-identity-fields).

## Finding context

R10 reviewed T06–T09 and verified that every Phase 1 spec deliverable is met,
every spec-mandated test passes, and the 2 ms P99 budget is satisfied with
three orders of magnitude of headroom (measured P99: 1 µs per primed primitive,
108 µs across the combined four-call hot path vs the 2 462 µs allowance). The
review verdict was **PASS-WITH-FOLLOWUP** to retire four minor findings before
Phase 2 work touches the resolver.

This task addresses findings F-1 through F-4 from
`docs/plans/2026-05-16-cognitive-workspace-fork-build/reviews/R10-identity.md`
`## Findings`. Finding F-5 (encoding ULID validator vs resolver ULID validator)
is informational only — it will be cleaned up naturally in Phase 2 when both
defer to the canonical ULID crate, and is therefore **not** included here.

## Goal

Land the four minor cleanups so Phase 2 begins from a clean Phase 1 substrate:

1. **F-1 — Remove duplicate resolver test module.**
   `daemon/crates/lattice-core/src/identity/resolver/tests.rs` (3 tests)
   restates behaviors already covered by the spec-mandated 10-case matrix in
   `daemon/crates/lattice-core/src/identity/resolver_tests.rs`. Delete the
   inner module and its `mod tests;` declaration.

2. **F-2 — Pre-emptively split `resolver.rs` to give Phase 2 headroom.**
   `daemon/crates/lattice-core/src/identity/resolver.rs` is 732 lines (68
   lines of 800-line headroom). Phase 2 adds event-log integration into the
   resolver, which will breach the limit on first touch. Split now.

3. **F-3 — Add behavioral tests for the public `resolve_test` method.**
   `IdentityResolver::resolve_test` is implemented and publicly exposed but
   has zero direct test coverage. The Phase 1 spec lists "tests" as one of
   the five resolution categories.

4. **F-4 — Document and regression-test the `legacy_name` contract.**
   `IdentityPayload::new` accepts `legacy_name: Option<String>`. Callers
   currently always pass `Some(...)`, but nothing enforces that. The MCP
   compatibility policy requires legacy fields to remain populated for one
   full phase cycle.

## Acceptable approach

### F-1

- Delete `daemon/crates/lattice-core/src/identity/resolver/tests.rs`.
- Remove the empty `daemon/crates/lattice-core/src/identity/resolver/` directory.
- Remove the trailing `mod tests;` declaration from
  `daemon/crates/lattice-core/src/identity/resolver.rs` (currently line 731).
- Verify `cargo test -p lattice-core --lib identity::resolver_tests` still
  passes 10/10 and that no other test module references
  `identity::resolver::tests`.

### F-2

Split `daemon/crates/lattice-core/src/identity/resolver.rs` into three
sibling files under `daemon/crates/lattice-core/src/identity/resolver/`:

- `resolver/mod.rs` — `IdentityResolver` struct, `ResolveError`, the six
  `resolve_*` methods, and the cache wiring. Public re-exports unchanged.
- `resolver/cache.rs` — `ResolverCache`, `CacheKey`, `CacheValue`,
  `DEFAULT_CACHE_CAPACITY`, `DEFAULT_CONTENT_HASH`. Private to the resolver
  module.
- `resolver/normalize.rs` — the bottom helper functions
  (`normalize_path`, `normalize_symbol_name`, `normalize_heading`,
  `is_test_node`, `is_canonical_ulid`, `split_symbol_target`, `looks_like_path`,
  `unique_or_ambiguous_symbol`). Private to the resolver module.

Target: each of the three files ≤ 400 lines. Keep the public API surface
exposed via `daemon/crates/lattice-core/src/identity/mod.rs` unchanged
(`pub use resolver::{IdentityResolver, ResolveError, WorkspaceId}`).

### F-3

Add two test functions to `daemon/crates/lattice-core/src/identity/resolver_tests.rs`:

- `test_resolve_test_unique_returns_symbolid` — fixture with one test
  function (e.g. `tests/auth_test.rs` containing `fn test_login()`),
  assertion that `resolver.resolve_test(workspace, "test_login")` returns
  `ResolveOutcome::Unique(symbol_id)` with
  `symbol_id.kind == "test"` and
  `symbol_id.file.repo_relative_path == "tests/auth_test.rs"`.
- `test_resolve_test_duplicate_name_returns_ambiguous_with_test_hint` —
  fixture with two test files exposing the same test name (e.g.
  `tests/auth_test.rs` and `tests/session_test.rs` both containing
  `fn test_login()`), assertion that the outcome is
  `ResolveOutcome::Ambiguous(report)` with
  `report.candidates.len() == 2` and `report.disambiguation_hint`
  contains `"test file path"` (per `ambiguity::test_disambiguation_hint`).

These two cases extend the spec-mandated matrix from 10 to 12 entries —
update the doc comment at the top of `resolver_tests.rs` to reflect the new
count.

### F-4

In `daemon/crates/lattice-core/src/identity/serialization.rs`:

- Add a doc comment on `IdentityPayload::new` citing
  `docs/architecture/2026-05-16-mcp-compatibility-policy.md`
  `## get_context_capsule additive identity fields` and stating that
  callers SHOULD pass `Some(legacy_name)` for the full phase-cycle
  compatibility window.
- Add a regression test
  `identity::serialization::tests::missing_legacy_name_omits_field_in_wire_shape`
  that constructs an `IdentityPayload` with `legacy_name: None` and asserts
  the serialized JSON does **not** contain a `legacy_name` key. This pins
  the `if let Some(...)` branch at `serialization.rs:62-64` so any future
  change that always-emits the field will trip the test.

## Files expected after this task

Modified:

- `daemon/crates/lattice-core/src/identity/resolver.rs` — `mod tests;` line removed.
- `daemon/crates/lattice-core/src/identity/resolver_tests.rs` — two new tests for `resolve_test`, doc comment updated to "twelve spec-mandated test cases".
- `daemon/crates/lattice-core/src/identity/serialization.rs` — doc comment on `IdentityPayload::new`, new `missing_legacy_name_omits_field_in_wire_shape` test.

Created (replacing `resolver.rs`):

- `daemon/crates/lattice-core/src/identity/resolver/mod.rs`
- `daemon/crates/lattice-core/src/identity/resolver/cache.rs`
- `daemon/crates/lattice-core/src/identity/resolver/normalize.rs`

Deleted:

- `daemon/crates/lattice-core/src/identity/resolver/tests.rs` (the F-1 duplicate).
- `daemon/crates/lattice-core/src/identity/resolver.rs` (replaced by `resolver/mod.rs`).

## Verification

- `cd daemon && cargo build --release` — clean rebuild, zero warnings.
- `cd daemon && cargo test -p lattice-core --lib identity::tests` — still 10/10 passes.
- `cd daemon && cargo test -p lattice-core --lib identity::resolver_tests` — now **12/12** passes (added `test_resolve_test_unique_returns_symbolid` and `test_resolve_test_duplicate_name_returns_ambiguous_with_test_hint`).
- `cd daemon && cargo test -p lattice-core --lib identity::serialization` — now includes `missing_legacy_name_omits_field_in_wire_shape`.
- `cd daemon && cargo test -p lattice-core --lib identity::budget_tests -- --include-ignored` — still 4/4 passes; resolver split must not introduce regression.
- `cd daemon && cargo test --workspace` — full sweep stays green (currently 253 passed).
- File line count check after split: `wc -l daemon/crates/lattice-core/src/identity/resolver/*.rs` shows each ≤ 400 lines.

## Definition of done

- [ ] F-1: `daemon/crates/lattice-core/src/identity/resolver/tests.rs` deleted; `mod tests;` removed from former `resolver.rs`.
- [ ] F-2: `resolver.rs` split into `resolver/mod.rs` + `resolver/cache.rs` + `resolver/normalize.rs`; each ≤ 400 lines; public API surface preserved.
- [ ] F-3: Two new `resolve_test` test cases land in `resolver_tests.rs`; doc comment updated to twelve cases.
- [ ] F-4: `IdentityPayload::new` doc-cites the compatibility policy heading; `missing_legacy_name_omits_field_in_wire_shape` test added.
- [ ] All verification commands pass.
- [ ] Coding-standard checks: zero new `#[allow(...)]`, zero new `TODO/FIXME/XXX`, zero new suppressions; resolver subdirectory files all ≤ 800 lines (target ≤ 400).
- [ ] No deferred work: every finding addressed in this task; no new follow-ups opened unless a fresh issue surfaces during the cleanup.
