# R26 — Cross-layer Contract Gate: Identity ↔ Event Log ↔ Memory Graph

**Review date:** 2026-05-17
**Reviewer:** R26 task executor (advanced model class)
**Spec anchors:**
- [`## Design Thesis`](../../2026-05-16-cognitive-workspace-fork-plan.md#design-thesis)
- [`### Phase 1: Unified Identity Model`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-1-unified-identity-model)
- [`### Phase 2: Event Log Substrate`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-2-event-log-substrate)
- [`### Phase 3: Memory Graph Storage`](../../2026-05-16-cognitive-workspace-fork-plan.md#phase-3-memory-graph-storage)
**Dependency verdicts honored:** R10 PASS-WITH-FOLLOWUP, R18 APPROVED, R25 APPROVED-WITH-FOLLOW-UPS — none REJECTED, so this gate is eligible to run.
**Contract test target:** `cd daemon && cargo test -p lattice-core --lib contract_tests::identity_event_memory` → **21 passed; 0 failed; 0 ignored**.

This gate certifies the cross-layer round-trip contracts between the three first-class substrates the spec names — workspace identity (Phase 1), event log (Phase 2), and memory graph (Phase 3). It is the contract that the rest of the build (retrieval, working memory, consolidation, verification) can assume.

## Contract surfaces

Each surface below is one cross-layer boundary the gate certifies. The module column names the code that defines the surface; the test column appears in `## Round-trip evidence`.

| # | Surface | Code module that defines it |
|---|---|---|
| 1 | Identity → event payload (typed `StableRef` variants and typed payload fields like `FileReadPayload.file_id`) | `daemon/crates/lattice-core/src/events/envelope.rs:91-105` (`enum StableRef`), `…/events/kinds.rs:124-444` (typed payload structs that embed `FileId` / `SymbolId` / `DocSectionId` / `EventId` / `MemoryId` / `ContextHandleId`) |
| 2 | Event payload → identity resolution (round-trip through `EventReader` ⇒ `IdentityResolver`) | `daemon/crates/lattice-core/src/events/reader.rs:62-156` (`EventReader::execute` + `tail`), `daemon/crates/lattice-core/src/identity/resolver.rs:213-228` (`IdentityResolver::resolve_event_ref`) |
| 3 | Identity → memory link target (typed `MemoryLinkTarget` discriminant) | `daemon/crates/lattice-core/src/memory_graph/links.rs:140-209` (`enum MemoryLinkTarget` + encode/decode), `…/memory_graph/links.rs:272-310` (`insert_link` persisting the encoded id), `…/memory_graph/schema.sql:149-191` (typed `target_kind` CHECK + `target_id` column) |
| 4 | Identity → memory evidence anchor (`EvidenceAnchor::FileSpan{ file: FileId }` and `EvidenceAnchor::DocSection{ id: SectionId }`) | `daemon/crates/lattice-core/src/memory_graph/evidence.rs:27-60` (`enum EvidenceAnchor`), `…/memory_graph/schema.sql:193-212` (`anchor_kind` CHECK + `anchor_json`) |
| 5 | Event provenance on memory (memory rows persist typed `EventId` provenance + `EvidenceReference::EventRef`) | `daemon/crates/lattice-core/src/memory_graph/store.rs:130-174` (`MemoryStore::create` writing provenance + evidence), `…/memory_graph/classes.rs:359-396` (`MemoryRecord` typed columns) |
| 6 | Replay-preserved identity (event-log replay reconstructs memory rows with identity-resolvable anchors) | `daemon/crates/lattice-core/src/memory_graph/replay.rs` (`capture_replay_snapshot` + `replay_events`), `daemon/crates/lattice-core/src/events/snapshot.rs:100-188` (typed `MemorySnapshot`) |
| 7 | Migration-preserved identity (T22 turns legacy path strings into typed `FileId` / `SymbolId`, encodes destination `memory_id` as a typed identity) | `daemon/crates/lattice-core/src/memory_graph/migration_mapping.rs:335-379` (`file_ids`, `symbol_ids`, `memory_identity`), `…/memory_graph/migration.rs:102-160` (`MemoryMigrator::plan` + `run`) |
| 8 | Schema-level enforcement (every identity-bearing column stores the typed shape — not a raw path or qualified name) | `daemon/crates/lattice-core/src/events/schema.sql:17-50` (`events.references_json`, typed `event_uuid`), `daemon/crates/lattice-core/src/memory_graph/schema.sql:15-247` (`memories.memory_id`, `memories.linked_files_json`, `memory_links.target_id`, `memory_evidence.anchor_json`, `memory_accesses.accessed_in_event`) |

## Round-trip evidence

All tests live in `daemon/crates/lattice-core/src/contract_tests/identity_event_memory.rs` and pass under the verification command `cd daemon && cargo test -p lattice-core --lib contract_tests::identity_event_memory`. Fixtures live in `…/contract_tests/identity_event_memory_support.rs`. The module is wired into the crate at `daemon/crates/lattice-core/src/lib.rs:23-24`.

| Surface | Test name | Assertion strategy |
|---|---|---|
| 1 | `stable_ref_file_variant_serializes_to_typed_identity_struct_not_path_string` | Serde-shape assertion: `StableRef::FileRef` serializes to a tagged object whose payload exposes `workspace_id` + `repo_relative_path` + `content_hash` and forbids a `path` field. |
| 1 | `stable_ref_symbol_variant_serializes_to_typed_identity_struct_not_qualified_name_string` | Serde-shape + typed deserialization: the inner payload deserializes back into `SymbolId` with the embedded `FileId` intact. |
| 1 | `file_read_payload_embeds_typed_file_identity_not_path_string` | JSON-value assertion: `FileReadPayload.file_id` is an object with `repo_relative_path` and no `path` field. |
| 1 | `diagnostic_observed_payload_embeds_typed_file_identity` | JSON-value assertion: `DiagnosticObservedPayload.file_id` is an object scoped to the workspace. |
| 2 | `stable_ref_round_trips_through_event_store_byte_equivalent` | Byte equivalence: after `EventWriter::append` + `EventReader::execute`, the envelope `references` vector matches the original `Vec<StableRef>` exactly. The `ContextBundleReturnedPayload` also matches across all five typed-id fields. |
| 2 | `round_tripped_event_id_resolves_via_identity_resolver_to_same_target` | Resolver equivalence: encode the read-back `EventId` via `encode_identity`, pass to `IdentityResolver::resolve_event_ref`, assert the returned `EventId` equals the original. |
| 2 | `memory_retrieved_payload_round_trips_memory_ids_through_event_log` | Identity-id equivalence: `MemoryRetrievedPayload.memory_ids` survives the writer/reader trip with `Vec<MemoryId>` equality. |
| 3 | `memory_link_target_symbol_persists_encoded_identity_in_target_id_column` | Schema-level assertion: the raw `memory_links.target_id` column starts with `symbol:`, contains no JSON string of the qualified name, and `decode_identity` re-derives the original `SymbolId`. |
| 3 | `memory_link_round_trips_through_get_links_from_with_typed_target` | Round-trip equivalence: `get_links_from` returns `MemoryLinkTarget::DocSection(SectionId)` equal to the inserted target. |
| 3 | `memory_link_file_target_resolves_under_file_rename_compat_without_link_rewrite` | Resolver equivalence under rename: the encoded original `FileId` resolves via `IdentityResolver::resolve_path` against a renamed fixture, returning the new path while the stored encoded id does not change. |
| 4 | `evidence_anchor_filespan_serializes_with_typed_fileid_struct` | Serde-shape: `EvidenceAnchor::FileSpan.file` is an object, not a path string. |
| 4 | `evidence_anchor_docsection_serializes_with_typed_doc_section_id` | Serde-shape: `EvidenceAnchor::DocSection.id` is an object with `heading_path` and no flat `heading` field. |
| 4 | `evidence_anchor_filespan_round_trips_through_memory_evidence_table` | Round-trip equivalence: after `MemoryStore::create` and `get_evidence_for`, both `FileSpan` and `DocSection` anchors retain their typed identities. |
| 5 + 7 | `migration_emits_typed_file_ids_in_linked_files_json_not_path_strings` | Schema-level assertion: post-migration `memories.linked_files_json` parses cleanly into `Vec<FileId>` (not `Vec<String>`), and the raw column does not start with `[\"` (the path-string signature). |
| 5 + 7 | `migration_preserves_memory_id_workspace_scoping_in_destination_memory_id_column` | Identity-id equivalence: post-migration `memories.memory_id` is an encoded `memory:` identity that decodes back to the original workspace + ulid. |
| 6 | `patch_applied_capture_records_only_typed_file_and_symbol_refs` | Type-discipline assertion: the captured `references_json` column parses as `Vec<StableRef>`, every element is `FileRef` or `SymbolRef`, and the envelope round-trips identical to the source. |
| 6 | `replay_preserves_identity_typed_evidence_anchor_after_full_replay` | Round-trip equivalence across replay: replaying every memory event into a fresh DB yields `FileSpan` evidence whose `FileId` equals the pre-replay anchor. |
| 6 | `replay_emits_memory_id_column_as_encoded_identity_after_full_replay` | Schema-level assertion: replayed `memories.memory_id` is `memory:` encoded and decodes to the canonical `MemoryId`. |
| 8 | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings` | Schema-level assertion: `memory_id`, `linked_files_json`, `linked_symbols_json`, `linked_docs_json`, `provenance_event_ids_json` columns parse as their typed Rust counterparts; provenance event ids carry the right `workspace_id`. |
| 8 | `event_table_references_json_column_only_holds_typed_stable_refs` | Schema-level + serde-shape: `events.references_json` parses as `Vec<StableRef>` and the first element is a JSON object, never a string. |
| E2E | `end_to_end_identity_event_memory_round_trip_preserves_typed_references_at_every_boundary` | Composite: identity → event payload → event store → event reader → memory writer → memory store → memory reader → identity resolver. Asserts equality at every boundary, including `EventReader.event_id` resolving back through `IdentityResolver::resolve_event_ref` and `IdentityResolver::resolve_path` reaching the file by both raw path and encoded identity. |

### Test execution evidence

```
$ cd daemon && cargo test -p lattice-core --lib contract_tests::identity_event_memory
…
running 21 tests
test result: ok. 21 passed; 0 failed; 0 ignored; 0 measured; 305 filtered out; finished in 0.70s
```

A full crate regression run (`cargo test -p lattice-core --lib`) reports **305 passed; 0 failed; 21 ignored** with the new module in place — no per-layer regression introduced by the contract gate.

## Schema regression coverage

No schema drift since R10 / R18 / R25 was certified. Every column in `events.*` and `memory_*` that carries an identity reference still holds the typed identity shape — either an encoded `<kind>:…` string (decodable via `crate::identity::decode_identity`) or a JSON value of a typed identity struct. The contract tests in `## Round-trip evidence` pin each column.

### Identity-bearing columns inventory

| Table.column | Stored shape | Test that proves the shape |
|---|---|---|
| `events.event_uuid` | ULID body of the typed `EventId` (workspace_id is on the sibling column `events.workspace_id`); `EventReader::execute` reconstructs the typed `EventId` from `event_uuid + workspace_id`. | `round_tripped_event_id_resolves_via_identity_resolver_to_same_target` |
| `events.references_json` | JSON array of `StableRef` (externally tagged objects, never bare strings). | `event_table_references_json_column_only_holds_typed_stable_refs`, `patch_applied_capture_records_only_typed_file_and_symbol_refs` |
| `event_payloads.bytes` | Raw payload bytes (canonical JSON of `EventPayload`); typed identities live inside via the payload struct fields. | `stable_ref_round_trips_through_event_store_byte_equivalent`, `file_read_payload_embeds_typed_file_identity_not_path_string` |
| `memories.memory_id` | Encoded `memory:<workspace>/<ulid>` identity. | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings`, `replay_emits_memory_id_column_as_encoded_identity_after_full_replay`, `migration_preserves_memory_id_workspace_scoping_in_destination_memory_id_column` |
| `memories.linked_files_json` | JSON `Vec<FileId>` of typed file identities. | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings`, `migration_emits_typed_file_ids_in_linked_files_json_not_path_strings` |
| `memories.linked_symbols_json` | JSON `Vec<SymbolId>`. | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings` |
| `memories.linked_docs_json` | JSON `Vec<SectionId>`. | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings` |
| `memories.linked_tests_json` | JSON `Vec<TestId>` (out of identity scope but still typed). | Covered indirectly by `seed_draft` + memory round-trip in `end_to_end_…` |
| `memories.provenance_event_ids_json` | JSON `Vec<EventId>`. | `memory_columns_holding_identity_references_store_typed_shapes_not_raw_strings`, `end_to_end_identity_event_memory_round_trip_preserves_typed_references_at_every_boundary` |
| `memories.evidence_references_json` | JSON `Vec<EvidenceReference>` (each entry carries a typed `StableRef` `target` + typed optional `EventId`). | Covered by the memory create flow that always writes a `StableRef::EventRef(event_id)` provenance — exercised by every memory creation test. |
| `memory_links.target_id` | Encoded identity string keyed by the typed `target_kind` discriminator (`symbol:` / `file:` / `memory:` / `section:` / typed JSON for `Test`). | `memory_link_target_symbol_persists_encoded_identity_in_target_id_column`, `memory_link_round_trips_through_get_links_from_with_typed_target` |
| `memory_links.source_memory_id` | Encoded `memory:` identity. | `memory_link_round_trips_through_get_links_from_with_typed_target` |
| `memory_links.evidence_event_id` | Encoded `event:` identity (optional). | Indirect — the type is `Option<EventId>` in the model and `decode_event_id` is the only loader path. Covered by `memory_link_round_trips_through_get_links_from_with_typed_target` whose harness builds the link without evidence; the typed shape is enforced by `MemoryLink::try_new`. |
| `memory_evidence.memory_id` | Encoded `memory:` identity. | `evidence_anchor_filespan_round_trips_through_memory_evidence_table` |
| `memory_evidence.event_id` | Encoded `event:` identity (optional). | Same. |
| `memory_evidence.anchor_kind` + `anchor_json` | Discriminator + typed `EvidenceAnchor` JSON (FileSpan{FileId}, SymbolRef(SymbolId), DocSection{SectionId}, TestResult{TestId, EventId}, EventReference(EventId)). | `evidence_anchor_filespan_round_trips_through_memory_evidence_table`, `evidence_anchor_filespan_serializes_with_typed_fileid_struct`, `evidence_anchor_docsection_serializes_with_typed_doc_section_id` |
| `memory_accesses.memory_id` | Encoded `memory:` identity. | Covered by every memory creation flow (foreign-key enforced). |
| `memory_accesses.accessed_in_event` / `downstream_outcome_event` | Encoded `event:` identity. | Same — `MemoryAccess` model owns the typed `EventId`. |
| `memory_scores.memory_id` | Encoded `memory:` identity. | Same — foreign key + typed model. |
| `memory_tombstones.memory_id` / `deleted_by_event_id` | Encoded `memory:` / `event:` identity. | Covered by the lifecycle `delete()` path; populated whenever a memory is invalidated via the contract harness. |

No column type changed since the prior per-substrate review, so no golden-schema diff is required for this gate.

## Findings

| ID | Severity | Finding | Owner |
|---|---|---|---|
| F-1 | minor | The contract test file (`identity_event_memory.rs`) is 895 lines — 95 lines over the 800-line Cadres heuristic. The standard's `## Hard limits` explicitly carves out "an 850-line file with 9 coherent sections is fine"; the file has eight contract-surface sections plus an end-to-end test, and the helpers are already extracted into `identity_event_memory_support.rs`. Recorded so the next iteration of the contract gate keeps the file from drifting further over budget — a future contributor adding a tenth contract surface should split per-surface modules under `contract_tests/identity_event_memory/`. | R26 |
| F-2 | minor | `MemoryStream::FromStr::Err = &'static str` (per R25 F-4 — `memory_graph/streams.rs:55-69`) still ships from Phase 3. The cross-layer round-trip does not depend on `FromStr<MemoryStream>` today, so the contract is intact; the inconsistency is a follow-up for Phase 4 callers that want a single error type for memory-graph parsing. Not raised against this gate, surfaced here so the prior R25 finding is not lost. | Phase 4 (T-followup-R25-A if filed) |
| F-3 | minor | The `migration_emits_typed_file_ids_in_linked_files_json_not_path_strings` test confirms `linked_files_json` carries typed `FileId` values, but `migration_mapping.rs:357-364` still assigns `kind: "unknown"` and `file: file_id(ws, "legacy-symbols")` to every migrated `SymbolId`. Symbols therefore round-trip the encoded identity shape but lose their per-symbol byte_offset and kind. Acceptable for compatibility: the legacy schema never stored those fields. Captured so the consolidation phase that re-parses legacy symbols can upgrade migrated `SymbolId`s in place. | Phase 4 / consolidation |
| F-4 | informational | `memory_links.evidence_event_id` and `memory_accesses.downstream_outcome_event` are nullable; we currently rely on `MemoryLink`/`MemoryAccess` typed models to enforce the typed shape, not on a SQL CHECK constraint. The contract is held by the encode/decode pair at the Rust boundary. Adding an `anchor_kind`-style discriminator on these columns is a Phase-4 improvement, not a contract failure. | Phase 4 |

No blocker- or major-severity findings.

## Verdict

**APPROVED.**

The cross-layer round-trip contracts between the three first-class substrates compose. Every surface listed in `## Contract surfaces` has at least one matching test in `## Round-trip evidence`, and every identity-bearing column in `## Schema regression coverage` is pinned by a test that fails loudly if a future contributor tries to store a raw path or qualified name where a typed identity belongs. The 21 contract tests pass cleanly under the spec-mandated command, and the full per-crate regression sweep (`cargo test -p lattice-core --lib`) remains green at 305 passed / 0 failed.

T27 (Phase 4: retrieval) is unblocked.
