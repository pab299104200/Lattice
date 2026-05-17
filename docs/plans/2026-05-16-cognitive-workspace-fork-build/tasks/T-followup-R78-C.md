# T-followup-R78-C — Phase 10 error-handling tightening + minor cleanup

**Phase:** 10 (follow-up from R78)
**Type:** frontend hardening + minor cleanup
**Model class:** balanced
**Depends on:** R78 (PASS-WITH-FOLLOWUP)
**Opened by:** R78 (Frontend review — Phase 10 human review UI)
**Spec anchor:** [§10. Human Review Surface](../../2026-05-16-cognitive-workspace-fork-plan.md#10-human-review-surface)
**Standards:** [Cadres coding standard](../../../../../shared/templates/coding.md) §Error handling, §Single source of truth; [UI specification](../../../../../shared/templates/ui-specification.md) §8.5 Error display on API failure, §14 i18n, §16 Telemetry-friendly markup

## Finding context

R78 reviewed T71–T77 and identified five minor issues that are not blockers but should be cleaned up before the surface accumulates more callers:

- **F-6** — `ReviewPanelProvider.refreshState` (`reviewPanel.ts:421-437`) only handles the `Result.ok=false` branch from the bridge; if `bridge.getOverview()` *throws*, the error propagates uncaught. Same pattern in `renderActiveRoute`.
- **F-7** — `extension/src/review/memoryInbox.ts:376` uses `data-testid="memory-inbox-filter-{key}"` rather than the spec-mandated `review-memory-inbox-filter-{key}` convention.
- **F-8** — `extension/src/review/i18n/index.ts` supports `{name}` placeholder replacement only; UI spec §14 requires ICU MessageFormat for plural rules. Single-locale English today; impact is deferred until a second locale is added.
- **F-9** — `retryConsolidationJob` (`reviewPanel.ts:1244-1275`) has `try { ... } finally { ... }` without an explicit catch. Bridge returns `Result<T, E>`, so the practical impact is small, but a thrown error would bypass the finally cleanup before propagating.
- **F-10** — `escapeHtml` is redefined in nine modules (`grep -c "function escapeHtml" extension/src/review/*.ts` → 9). Coding standard §Single-source-of-truth threshold for extraction is 2 duplicates.

## Goal

Close the five minor findings so the Phase 10 surface is fully compliant with UI spec §8.5, §14, §16 and coding standard §Error handling and §Single source of truth.

## Scope

1. **F-6 — Defensive `refreshState` + `renderActiveRoute`.** Wrap the bridge `await` calls in try/catch in both methods. On throw, post `{ type: 'error', message: error.message }` to the webview and short-circuit. Add a unit test that stubs the bridge to throw on `getOverview()` and asserts the webview receives an error message.

2. **F-7 — Normalize memoryInbox filter testid.** Rename `memory-inbox-filter-{key}` → `review-memory-inbox-filter-{key}` in `extension/src/review/memoryInbox.ts:376`. Update any test selector that references the old id.

3. **F-8 — ICU MessageFormat readiness.** Decide between (a) swap `interpolate()` for `i18next-icu` now while only one locale exists OR (b) leave the helper alone but document the constraint in `extension/src/review/i18n/index.ts` as an inline JSDoc comment so future locale work catches it. Either path is acceptable; if (a), add `i18next-icu` to `package.json` and update at least one catalog entry to use a plural rule.

4. **F-9 — Add explicit catch to `retryConsolidationJob`.** Insert `catch (error) { await vscode.window.showErrorMessage(errorMessage(error)); }` before the existing `finally`. Add a smoke test that stubs the bridge to throw on `retryConsolidationSession` and asserts the user-visible error is shown.

5. **F-10 — Extract canonical `escapeHtml` / `escapeAttr`.** Move both helpers to `extension/src/review/components/html.ts` (which already exists at 12 lines). Replace each local `function escapeHtml` redefinition with an import. Confirm `grep -c "function escapeHtml" extension/src/review/**/*.ts` → 1 (just the canonical helper).

## Constraints

- **No behavior change for end users.** Every existing smoke test must still pass.
- **i18n discipline.** Any new error string introduced by F-6 must be wrapped via `i18n.t(...)` and added to `extension/src/review/i18n/en.json` (`reviewPanel.bridgeError` is a reasonable key).
- **Stable testid for downstream tasks.** F-7's rename is a breaking change for any external tooling that queries by the old testid. Confirm no test/script outside `extension/src/test/` references the old id (grep the whole repo).

## Verification commands

- `cd extension && npm run compile` → exit 0.
- `cd extension && npm run lint` → exit 0.
- `cd extension && npm test` → exit 0; new tests for F-6 and F-9 pass.
- `grep -c "function escapeHtml" extension/src/review/**/*.ts` → 1.
- `grep -n 'data-testid="review-memory-inbox-filter' extension/src/review/memoryInbox.ts` → ≥ 1 hit.
- `grep -n 'data-testid="memory-inbox-filter' extension/src/review/memoryInbox.ts` → 0 hits.
- `grep -rn "memory-inbox-filter-" extension/src/ docs/` → 0 hits outside the rename site.

## Definition of done

- [ ] `refreshState` and `renderActiveRoute` wrap bridge calls in try/catch and post error messages to the webview.
- [ ] `retryConsolidationJob` has an explicit catch in addition to finally.
- [ ] `memoryInbox` filter testid follows `review-memory-inbox-filter-{key}` convention.
- [ ] Decision on F-8 is committed (either i18next-icu integration OR documented constraint).
- [ ] `escapeHtml` / `escapeAttr` live in a single shared module and every view imports them.
- [ ] R78 review findings F-6, F-7, F-8, F-9, F-10 marked addressed.
