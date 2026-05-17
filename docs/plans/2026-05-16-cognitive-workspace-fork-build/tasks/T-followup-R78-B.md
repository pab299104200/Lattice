# T-followup-R78-B — Phase 10 pagination + loading skeleton coverage

**Phase:** 10 (follow-up from R78)
**Type:** frontend UX completion + tests
**Model class:** balanced
**Depends on:** R78 (PASS-WITH-FOLLOWUP)
**Opened by:** R78 (Frontend review — Phase 10 human review UI)
**Spec anchor:** [§10. Human Review Surface](../../2026-05-16-cognitive-workspace-fork-plan.md#10-human-review-surface)
**Standards:** [UI specification](../../../../../shared/templates/ui-specification.md) §2.1 Skeletons vs spinners, §6.1 Pagination — Required on ALL Lists

## Finding context

R78 reviewed T71–T77 and identified two UI-specification gaps in the otherwise complete Phase 10 surface:

- **F-4** — Three list views ship without pagination controls, violating UI spec §6.1 ("Every list/table that displays records from the backend MUST be paginated. No exceptions."):
  - `extension/src/review/staleView.ts` (no `page`/`pageSize` references; renders all filtered results inline)
  - `extension/src/review/promotionQueue.ts` (single render of the queue)
  - `extension/src/review/contradictionQueue.ts` (single render of the queue)
- **F-5** — Seven of eleven review routes have no explicit loading-state skeleton, violating UI spec §2.1. Loading skeletons are present only in `memoryInbox.ts`, `promotionQueue.ts`, `contradictionQueue.ts`. The remaining seven (`staleView`, `evidenceInspector`, `eventTraceView`, `retrievalExplanationView`, `consolidationQueueView`, `workspaceGraphHealthView`, `indexingHealthView`) render nothing until the bridge `await` resolves; if first load > 400 ms, the user sees a blank panel.

## Goal

Bring every list/table-bearing review view into compliance with UI spec §6.1 (pagination) and §2.1 (loading skeleton).

## Scope

### F-4 — Pagination

1. **`staleView`.** Add `page` and `pageSize` to `StaleViewState`. Wire `getStaleMemories` (or its bridge equivalent) to accept the same arguments, OR paginate client-side on the returned list if the bridge already returns the full snapshot. Render the standard pagination footer (`«` `Page N` `»` + page-size selector + "Showing X-Y of Z") under the table.
2. **`promotionQueue`.** Add `page` + `pageSize` to the queue state passed into `mountPromotionQueue` and render the same pagination footer.
3. **`contradictionQueue`.** Same as `promotionQueue`.
4. **Smoke tests.** Add three new tests in `extension/src/test/review.test.ts` — one per view — asserting that the rendered HTML contains the pagination footer when the stubbed bridge returns >25 rows.

### F-5 — Loading skeletons

1. **Extract a shared helper.** `renderRouteSkeleton(routeId: ReviewRouteId, i18n: ReviewI18n): string` lives in `extension/src/review/components/skeleton.ts` (new file). Returns the appropriate skeleton shape (table-rows / dashboard-cards / detail-pane) for the route family.
2. **Call it from each `mountXxx` before the bridge `await`.** For each of `staleView`, `evidenceInspector`, `eventTraceView`, `retrievalExplanationView`, `consolidationQueueView`, `workspaceGraphHealthView`, `indexingHealthView`, render the skeleton into the host (or return it as the initial `routeView.html`) before the bridge call begins.
3. **Smoke tests.** Add seven new tests — one per affected view — asserting that the skeleton HTML is rendered before the bridge resolves (simulate slow bridge by withholding the response).

## Constraints

- **i18n.** Any new user-facing strings (e.g., pagination labels for new views) must use existing keys where possible (`memoryInbox.pagination.*`, `eventTraceView.pagination.*` already exist). If a view needs a unique key, add it to the view's namespace in `en.json` and reference via `i18n.t(...)`.
- **`data-testid`.** New pagination controls follow `review-<view>-pagination-prev`, `review-<view>-pagination-next`, `review-<view>-pagination-size` per UI spec §16.
- **No regression in F-7 fix.** If `T-followup-R78-C` lands first, do not reintroduce the legacy `memory-inbox-filter-{key}` testid pattern.

## Verification commands

- `cd extension && npm run compile` → exit 0.
- `cd extension && npm run lint` → exit 0.
- `cd extension && npm test` → exit 0; new pagination + skeleton tests pass.
- `grep -c "page\b\|pageSize\|Showing.*of" extension/src/review/staleView.ts` ≥ 5.
- `grep -c "page\b\|pageSize\|Showing.*of" extension/src/review/promotionQueue.ts` ≥ 5.
- `grep -c "page\b\|pageSize\|Showing.*of" extension/src/review/contradictionQueue.ts` ≥ 5.
- `grep -l "renderRouteSkeleton" extension/src/review/*.ts` reports all seven F-5 views.

## Definition of done

- [ ] `staleView`, `promotionQueue`, `contradictionQueue` each render a standard pagination footer.
- [ ] All seven F-5 views render a loading skeleton before bridge response.
- [ ] New smoke tests pass.
- [ ] R78 review findings F-4 and F-5 marked addressed.
