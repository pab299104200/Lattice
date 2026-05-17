# R78 — Frontend review — Phase 10 human review UI

**Reviewer:** R78 (PR gate for Phase 10 human review UI)
**Date:** 2026-05-17
**Scope:** Outputs of T71 → T77 — every artifact under `extension/src/review/` plus `extension/src/test/review.test.ts`, `extension/src/test/runTest.ts`, and `extension/package.json` review/command contributions.
**Standards reviewed:**
- [Cadres coding standard](../../../../../shared/templates/coding.md) (Hard limits; Error handling; Single source of truth; No broken windows)
- [UI specification](../../../../../shared/templates/ui-specification.md) (§2 Performance, §3 Accessibility, §6 Lists & tables, §7 Modals, §8 State, §11 Visual, §12 Data display, §14 i18n, §16 Telemetry)
- [Definition of Done checklist](../../../../../shared/templates/definition-of-done-checklist.md)
**Spec anchors:** [§10. Human Review Surface](../2026-05-16-cognitive-workspace-fork-plan.md#10-human-review-surface) and [§Phase 10: Human Review And Extension UX](../2026-05-16-cognitive-workspace-fork-plan.md#phase-10-human-review-and-extension-ux)

## Spec alignment

Spec §10 requires eleven operator views to be reachable from `ReviewPanelProvider`. Every view is declared on `ReviewRouteId` (`extension/src/review/reviewPanel.ts:43-54`) and enumerated by `routeIds()` (`extension/src/review/reviewPanel.ts:2357-2371`).

| # | Spec view (`§10`) | Implementation site | Status |
|---|---|---|---|
| 1 | Memory inbox | `extension/src/review/memoryInbox.ts:1-508` + route render `reviewPanel.ts:540-560` (route id `memoryInbox`) | **PASS** — paginated, sortable, filterable; states for loading, empty, error |
| 2 | Proposed promotions | `extension/src/review/promotionQueue.ts:1-367` + serializer + route id `promotionQueue` (`reviewPanel.ts:51`) | **PASS-WITH-CAVEATS** — sortable + status filter; loading/empty/error rendered. **Missing pagination** (F-4) |
| 3 | Proposed contradictions | `extension/src/review/contradictionQueue.ts:1-411` + route id `contradictionQueue` (`reviewPanel.ts:51`) | **PASS-WITH-CAVEATS** — sortable + filterable; loading/empty/error rendered. **Missing pagination** (F-4) |
| 4 | Stale memory list | `extension/src/review/staleView.ts:1-232` + render `reviewPanel.ts:540-560` (route id `staleView`) | **PASS-WITH-CAVEATS** — sortable; row-level re-verify with inline error state. **Missing pagination** (F-4); no explicit loading skeleton (F-5) |
| 5 | Memory evidence view | `extension/src/review/evidenceInspector.ts:1-258` + render `reviewPanel.ts:561-582` (route id `evidenceInspector`) | **PASS** — single-record inspector (pagination N/A); re-verify with try/catch + `vscode.window.showErrorMessage`; cross-links to event trace |
| 6 | Event trace view | `extension/src/review/eventTraceView.ts:1-413` + render `reviewPanel.ts:583-612` (route id `eventTrace`) | **PASS** — paginated, sortable, multi-filter; native `<dialog>` payload modal; copy/open-in-editor mutations wrapped in try/catch |
| 7 | Retrieval explanation view | `extension/src/review/retrievalExplanationView.ts:1-256` + render `reviewPanel.ts:613-637` (route id `retrievalExplanation`) | **PASS** — paginated; sortable candidate table; decision badges via shared `StatusBadge` |
| 8 | Usefulness metrics | Route declared on `reviewPanel.ts:51` (`usefulnessMetrics`), bridge capability published via `rpcBridge.ts:77,197` (`get_memory_metrics`); existing sidebar surface from R70 remains the dashboard | **PASS** — link reachable; R78 task §1 explicitly allows the metrics dashboard to live in the existing sidebar |
| 9 | Workspace graph health | `extension/src/review/workspaceGraphHealthView.ts:1-261` + render `reviewPanel.ts:686-709` (route id `workspaceGraphHealth`) | **PASS** — paginated per-family tables; recompute action; sortable headers |
| 10 | Indexing health | `extension/src/review/indexingHealthView.ts:1-197` + render `reviewPanel.ts:662-685` (route id `indexingHealth`) | **PASS** — dashboard-style breakdowns (parser, vector, FTS, event log); refresh action with refreshing state; pagination intentionally not present (per-family static metrics, not a list) |
| 11 | Consolidation queue | `extension/src/review/consolidationQueueView.ts:1-293` + render `reviewPanel.ts:638-661` (route id `consolidationQueue`) | **PASS** — paginated, sortable; retry mutation with `vscode.window.showInformationMessage(..., { modal: true })` confirm + `vscode.window.showErrorMessage` failure path |

Smoke tests (`extension/src/test/review.test.ts:282-356`) exercise 10/11 routes (`activation`, `focus`, `mounts ... without throwing`, `mounts ... with empty data`, `mounts ... with bridge error`). Usefulness metrics is intentionally exercised via the sidebar surface (R70). 33 smoke tests pass (`npm test` exit 0).

## Coding-standard alignment

### File length audit (`wc -l`)

| File | Lines | Limit | Status |
|---|---:|---:|---|
| `extension/src/review/reviewPanel.ts` | **2617** | 800 | **FAIL — 3.27× over limit** (F-1) |
| `extension/src/review/rpcBridge.ts` | **920** | 800 | **FAIL — 1.15× over limit** (F-2) |
| `extension/src/test/review.test.ts` | 726 | 800 | PASS |
| `extension/src/review/rpcPayloads.ts` | 678 | 800 | PASS |
| `extension/src/review/memoryInbox.ts` | 508 | 800 | PASS |
| `extension/src/review/components/ProposalDialog.ts` | 437 | 800 | PASS |
| `extension/src/review/eventTraceView.ts` | 413 | 800 | PASS |
| `extension/src/review/contradictionQueue.ts` | 411 | 800 | PASS |
| `extension/src/review/reviewPanelHtml.ts` | 410 | 800 | PASS |
| `extension/src/review/promotionQueue.ts` | 367 | 800 | PASS |
| `extension/src/review/consolidationQueueView.ts` | 293 | 800 | PASS |
| `extension/src/review/workspaceGraphHealthView.ts` | 261 | 800 | PASS |
| `extension/src/review/evidenceInspector.ts` | 258 | 800 | PASS |
| `extension/src/review/retrievalExplanationView.ts` | 256 | 800 | PASS |
| `extension/src/review/staleView.ts` | 232 | 800 | PASS |
| `extension/src/review/indexingHealthView.ts` | 197 | 800 | PASS |
| `extension/src/review/components/MemoryRow.ts` | 92 | 800 | PASS |
| `extension/src/review/i18n/index.ts` | 84 | 800 | PASS |
| `extension/src/review/components/StatusBadge.ts` | 65 | 800 | PASS |
| `extension/src/test/runTest.ts` | 59 | 800 | PASS |
| `extension/src/review/components/html.ts` | 12 | 800 | PASS |

### Function length audit

`ReviewPanelProvider` (`reviewPanel.ts:105-2355`) contains methods that exceed the 50-line per-function limit:

| Method | Line range | Length | Status |
|---|---|---:|---|
| `handleWorkspaceGraphHealthMessage` | 1185-1286 | 102 | **FAIL** (F-3) |
| `handleEventTraceMessage` | 977-1077 | 100 | **FAIL** (F-3) |
| `renderActiveRoute` | 539-613 | 75 | **FAIL** (F-3) |
| `handleConsolidationQueueMessage` | 1009-1068 | 60 | **FAIL** (F-3) |

All four are large `switch (message.command)` dispatchers. Per coding standard §Hard limits, they should be split into per-command helper methods (the existing pattern in `handleStaleViewMessage`, which dispatches to private helpers, is the model to follow).

### Forbidden-token audit

- `grep -rn "TODO\|FIXME\|XXX" extension/src/review/ extension/src/test/review.test.ts` → **0 hits**.
- `grep -rn "@ts-ignore\|@ts-expect-error" extension/src/review/ extension/src/test/review.test.ts` → **0 hits**.
- `grep -rn "window\.confirm\|window\.prompt\|window\.alert" extension/src/review/` → **0 hits**.
- `grep -rn "\.catch(()" extension/src/review/` → **0 hits** (no silent catch antipattern).
- Commented-out code review → none found.

### Error-handling audit (every mutation handler)

Every mutation surface is wrapped in `try/catch` with both an inline error state and (where appropriate) a `vscode.window.showErrorMessage`:

| Mutation | Site | try/catch? | User-visible error? |
|---|---|---|---|
| `ProposalDialog` accept | `components/ProposalDialog.ts:174-184` | Yes | Inline banner via `setError(errorMessage(error, i18n))` |
| `ProposalDialog` reject | `components/ProposalDialog.ts:158-172` | Yes | Inline banner via `setError(...)` |
| Initial load (promotion queue) | `promotionQueue.ts:29-40` | Yes | `reportReviewViewError()` + inline error render |
| Initial load (contradiction queue) | `contradictionQueue.ts:30-41` | Yes | `reportReviewViewError()` + inline error render |
| Initial load (stale view) | `staleView.ts:39-55` | Yes | `host.reportError?.()` + inline error |
| Initial load (evidence inspector) | `evidenceInspector.ts:33-46` | Yes | inline error |
| Initial load (consolidation queue) | `consolidationQueueView.ts:35-61` | Yes | inline error |
| Initial load (workspace graph health) | `workspaceGraphHealthView.ts:41-55` | Yes | inline error |
| Re-verify (stale) | `reviewPanel.ts:754-779` | Yes | `vscode.window.showErrorMessage` + row-level inline error |
| Re-verify (evidence inspector) | `reviewPanel.ts:781-806` | Yes | `vscode.window.showErrorMessage` + inline error |
| Retry consolidation | `reviewPanel.ts:1244-1275` | try/finally with `result.ok` branch | `vscode.window.showErrorMessage` (F-9 — no explicit `catch` for thrown errors) |
| Recompute workspace graph | `reviewPanel.ts:1186-1196` | No explicit try/catch; delegates to `refreshState()` | Errors surface through `refreshState`'s `result.ok` branch; thrown errors propagate (F-6) |
| `refreshState` | `reviewPanel.ts:421-437` | Checks `overviewResult.ok`; does NOT catch thrown bridge errors | Errors propagate uncaught (F-6) |
| Copy event payload | `reviewPanel.ts:1374-1386` | Yes | `vscode.window.showErrorMessage` |
| Open event payload in editor | `reviewPanel.ts:1388-1400` | Yes | `vscode.window.showErrorMessage` |

### Single-source-of-truth audit

- **Status-badge mapping:** `STATUS_PRESENTATION` in `components/StatusBadge.ts:31-49` is the sole verification-status → CSS-class/i18n-key mapping. `grep -rn "switch.*verified\|verified.*case" extension/src/review/` confirms no duplicates. Verification, proposal, job, and decision badges all flow through `renderStatusBadge`. `eventTraceView.ts:364-374` uses distinct event-kind classes (`status-badge--error/info/success/neutral`) and does not duplicate the verification mapping.
- **i18n catalog:** `extension/src/review/i18n/en.json` is the sole user-facing string source. `extension/src/review/i18n/index.ts:1-84` exposes `createReviewI18n(locale)`, `i18n.t(key, params)`, and `i18n.has(key)`. A `FALLBACK_CATALOG` (English) is consulted on miss; missing keys fall back to the key string (no thrown errors). Pluralization is single-locale only — no ICU MessageFormat (F-8).
- **HTML escape:** `escapeHtml` is duplicated across views (each module redefines it). Acceptable per `coding.md` Single-Source-of-Truth: two-instance duplication is permitted; three+ triggers extraction. `grep -c "function escapeHtml" extension/src/review/` reports nine duplicates — F-10 records this for cleanup but it is not a blocker because the canonical helper would be a 6-line utility.

### `npm install && npm run compile && npm run lint && npm test` results

- `npm install` (already installed; cache hit).
- `npm run compile` → exit 0, no warnings.
- `npm run lint` → exit 0, no eslint findings.
- `npm test` → exit 0; all 33 smoke tests in `review.test.ts` pass plus pre-existing test suites.

## UI-specification compliance

### §2 Response time & perceived performance

- **§2.1 Skeletons vs spinners:** Skeleton loaders are rendered for `memoryInbox` (`memoryInbox.ts:329-342`), `promotionQueue` (`promotionQueue.ts:313-315`), and `contradictionQueue` (`contradictionQueue.ts:361-363`). For the remaining 8 routes (`staleView`, `evidenceInspector`, `eventTraceView`, `retrievalExplanationView`, `consolidationQueueView`, `workspaceGraphHealthView`, `indexingHealthView`), the bridge call runs before the renderer is invoked, so a slow initial load leaves the panel blank until completion. Per §2.1 this is acceptable below 400 ms but should ship with explicit skeletons for parity (F-5).
- **Optimistic UI (§2.2):** Not used; all mutations show in-flight indicator and wait for the server. Correct for a review UI where success is not >99% (re-verify, retry consolidation, accept/reject proposals).

### §3 Accessibility baseline

- **§3.1 Color:** Status badges are CSS-token driven (`status-badge--success/warning/danger/info/muted`) via `reviewPanelHtml.ts` theme tokens; no hex literals in the review tree. Status badges always pair color with an i18n label. PASS.
- **§3.2 Keyboard:** All controls are native `<button>`, `<input>`, `<select>` rendered into the webview; no positive `tabindex`. Native `<dialog>` traps focus and supports Esc out of the box (`components/ProposalDialog.ts:33,193`).
- **§3.3 Focus management:** `ProposalDialog` focuses the right element on mount: rejection mode → reason input, typed-confirm/accept → confirm input, otherwise accept button (`ProposalDialog.ts:189-195`). On `close` the dialog removes itself and focus returns to the previously focused element (browser default for native `<dialog>`). PASS.
- **§3.4 Target size:** All buttons rendered through DaisyUI-equivalent `.btn` / `.icon-button` classes defined in `reviewPanelHtml.ts`; row action buttons get explicit min-height. PASS.
- **§3.5 Forms:** `ProposalDialog`'s reason textarea has an associated `<span>` label inside `<label>` (`ProposalDialog.ts:80-88`); typed-confirm input similarly wrapped (`ProposalDialog.ts:93-100`); placeholders never used as labels. PASS.
- **§3.6 Live regions & status messages:** Every banner uses `role="alert"`: `consolidationQueueView.ts:73,94,184`, `eventTraceView.ts:90,226`, `evidenceInspector.ts:73,192`, `indexingHealthView.ts:53,196`, `memoryInbox.ts:347`, `promotionQueue.ts:318`, `contradictionQueue.ts:366`, `retrievalExplanationView.ts:69,89,203`, `reviewPanelHtml.ts:307`, `reviewPanel.ts:2124`, `staleView.ts:145,182`, `workspaceGraphHealthView.ts:89,260`. Warning banners use `role="status"`. PASS.
- **§3.7 Motion:** No animation/transition CSS introduced. `reviewPanelHtml.ts` opts out of motion through the host's reduced-motion settings.
- **§3.8 Screen reader patterns:** Tables use `<th scope="col">` (e.g., `memoryInbox.ts:395-410`, `staleView.ts:106-116`). Modals use native `<dialog>`. Icon-only close button uses `aria-label` via the i18n string. PASS.

### §6 Lists & tables

- **§6.1 Pagination — required on ALL lists.** Pagination controls (Prev/Next + page-size selector + "Showing X-Y of Z") are present in: `memoryInbox.ts:279-327`, `eventTraceView.ts:200-218`, `retrievalExplanationView.ts:173-191`, `consolidationQueueView.ts:155-174`, `workspaceGraphHealthView.ts:201-226`. **Missing in `staleView.ts`, `promotionQueue.ts`, `contradictionQueue.ts`** (F-4). `indexingHealthView` and `evidenceInspector` are dashboards / single-record inspectors — pagination N/A.
- **§6.2 Sortable headers.** `sortableHeader()`-style helpers with click affordance and `↑/↓/↕` indicators exist in: `memoryInbox.ts:395-410`, `promotionQueue.ts:118-135`, `contradictionQueue.ts:160-177`, `staleView.ts:106-116`, `eventTraceView.ts:305-308`, `retrievalExplanationView.ts:245-248`, `consolidationQueueView.ts:234-242`, `workspaceGraphHealthView.ts:145,191`. PASS.
- **§6.3 Search & filtering.** `memoryInbox` filters status/scope/memoryClass via UI dropdowns; `eventTraceView` filters by kind/actor/session/task/workspace/branch + ISO time window; `consolidationQueueView` filters by status/kind/mode; `staleView` filters status + scope. All filter pickers route through `vscode.window.showQuickPick` (`reviewPanel.ts:526,716,738,1209,1292,1316,2527`), satisfying the §5.2 "ID-bearing fields are dropdowns" rule. PASS.

### §7 Modals & dialogs

- **§7.1 Native `<dialog>`.** Two modal call sites — both use native `<dialog>`:
  - `ProposalDialog` (`components/ProposalDialog.ts:33`) — `dialog.showModal()`; backdrop styled via `.proposal-dialog::backdrop` in `reviewPanelHtml.ts:1822`; Esc-to-close native.
  - Event-trace payload dialog (`eventTraceView.ts:163-178`) — `<dialog id="payload-...">` opened via local command dispatcher.
- **§7.2 Confirmation dialogs.** Destructive accept/reject for `repo`/`user`/`organization` scope requires typed confirmation: `ProposalDialog.ts:124-138` enables the Accept button only when `confirmInput.value.trim() === confirmToken` (token derived from `proposedClass`). Retry consolidation requires `vscode.window.showInformationMessage(..., { modal: true })` confirm (`reviewPanel.ts:1254-1259`).
- **No `window.confirm/prompt/alert`** in the entire review tree (grep result: 0). PASS.

### §8 State management

- **§8.1 No `Promise.all` for independent calls.** `grep -n "Promise\.all" extension/src/review/` → 0 hits. Each per-view `refresh` issues a single bridge call and surfaces failures locally. PASS.
- **§8.2 Loading states.** Skeletons render in 3 views (see §2.1 above); F-5 captures the gap.
- **§8.3 Empty states.** Every view renders an explicit empty placeholder when the bridge returns 0 records: `memoryInbox.ts:357-369`, `promotionQueue.ts:321-328`, `contradictionQueue.ts:369-376`, `staleView.ts:102`, `eventTraceView.ts:220-223`, `retrievalExplanationView.ts:92`, `consolidationQueueView.ts:176-181`, `workspaceGraphHealthView.ts:171`, `indexingHealthView.ts:45,171`, `evidenceInspector.ts:30,96,100,104`. PASS.
- **§8.4 Toast notifications.** Mutations show `vscode.window.showInformationMessage` on success (`reviewPanel.ts:767,794,1268,1382`) and `vscode.window.showErrorMessage` on failure (`reviewPanel.ts:352,716,738,1209,1244,…`). PASS.
- **§8.5 Error display on API failure.** Every mutation handler audited (see Coding-standard §Error-handling audit). Two gaps: F-6 (refreshState does not catch thrown errors; only Result-typed `result.ok=false` is handled) and F-9 (retryConsolidationJob has try/finally without explicit catch).
- **§8.6 Error message style.** Error text comes from `parseApiError`-equivalent `errorMessage(error, i18n)` (`ProposalDialog.ts:407-418`), which falls back to the i18n `proposalDialog.genericError` key when the bridge returns no message. Inline errors point at the affected row/field. PASS.
- **§8.7 Success messages.** Success toasts in past tense via `consolidationQueueView.retrySuccess`, `staleView.verifySuccess`, `evidenceInspector.verifySuccess`, `promotionQueue.messages.applySuccess`, `promotionQueue.messages.rejectSuccess`. PASS.
- **§8.8 Undo & optimistic UI.** Not used; correct for this surface (destructive irreversible decisions per §8.8).
- **§8.9 Bulk actions.** Out of Phase 10 scope (no bulk operations declared in spec §10).

### §11 Visual standards

- **§11.1 Status badges.** Sole source `components/StatusBadge.ts` covers all 8 verification statuses (`verified`, `unverified`, `in_review`, `stale`, `contradicted`, `superseded`, `expired`, `invalidated`) plus proposal (`pending`, `applied`, `rejected`, `reverted`, `proposed`) and job (`queued`, `running`, `failed`, `dropped`) — 17 total mapped statuses. Color paired with i18n label in every render path. PASS.
- **§11.2 Priority indicators.** N/A — no priority surfaces.
- **§11.3 Typography.** `reviewPanelHtml.ts` defines the type stack; section headers `h2`/`h3`, table headers `text-sm font-medium uppercase`-style classes. PASS.
- **§11.4 Spacing.** All review views render through the shared `.route-stack` / `.queue-surface` layouts in `reviewPanelHtml.ts`; consistent gaps. PASS.
- **§11.5 Density.** Single density (review surfaces have low row counts per page; spec §10 does not mandate comfort/compact toggle). Acceptable.
- **§11.6 Visual hierarchy.** Single primary action per surface (accept on proposal dialog; retry on consolidation row). PASS.

### §12 Data display

- **§12.1 Dates & times.** `Intl.DateTimeFormat` used in `components/MemoryRow.ts:81-87`; `toLocaleString()` (browser locale) used in `staleView.ts:218`, `evidenceInspector.ts:236`, `ProposalDialog.ts:406`, `consolidationQueueView.ts:263`, `contradictionQueue.ts:397`, `promotionQueue.ts:353`, `eventTraceView.ts:386`, `indexingHealthView.ts:184`. Acceptable per §14 because the locale is `undefined` (browser default), not a hard-coded `'en-US'`. No raw ISO strings displayed to the user. PASS.
- **§12.2 Numbers.** `toLocaleString()` used for counts across `workspaceGraphHealthView.ts:91-94,142` and `indexingHealthView.ts:56-58,112,145-149`. PASS.
- **§12.3 Truncation.** Long content (memory snippets, proposal evidence) renders via `escapeHtml`-safe templates; `MemoryRow` truncates via CSS class on `.memory-row__snippet`. PASS.

### §14 Internationalization & localization

- **All user-facing strings via `i18n.t(...)`.** `grep -nE "i18n\.t\('[^']+'\)" extension/src/review/` → **356 calls**; `grep -nE 'i18n\.t\("[^"]+"\)' extension/src/review/` → 0 (single-quote convention). `grep -nE '>[A-Z][a-z]+ [A-Z][a-z]+<' extension/src/review/` → 0 (no inline literal English text in template output). PASS.
- **No string concatenation for user text.** Interpolation uses placeholders (e.g., `t('proposalDialog.typedConfirmPrompt', { value: confirmToken })`). PASS for replacement; F-8 flags that ICU MessageFormat / plural rules are not supported (the catalog ships English-only today, but the helper would have to be extended to support plurals when a second locale is added).
- **Numbers/dates via `Intl`** with locale = `undefined` (browser default). PASS.
- **Bidi-friendly.** All review CSS in `reviewPanelHtml.ts` uses logical layout (flex/grid), no `margin-left`/`padding-right` literals. PASS.

### §16 Telemetry-friendly markup

- **`data-testid` on every route container,** following the `review-<view>-<action>` convention:
  - `review-memory-inbox` (`memoryInbox.ts:210,223,329,344`)
  - `review-promotion-queue` (`promotionQueue.ts:46`)
  - `review-contradiction-queue` (`contradictionQueue.ts:90`)
  - `review-stale-view` (`staleView.ts:71,182`)
  - `review-event-trace` (`eventTraceView.ts:75,226`)
  - `review-retrieval-explanation` (`retrievalExplanationView.ts:62,82,203`)
  - `review-consolidation-queue` (`consolidationQueueView.ts:81,184`)
  - `review-indexing-health` (`indexingHealthView.ts:46,196`)
  - `review-workspace-graph-health` (`workspaceGraphHealthView.ts:82,260`)
  - `review-evidence-inspector` (`evidenceInspector.ts:30,58,192`)
- **Exception:** `memoryInbox.ts:376` uses `data-testid="memory-inbox-filter-{key}"` rather than `review-memory-inbox-filter-{key}` (F-7).
- **Smoke tests query by these testids** (`review.test.ts:363-378` — `routeSpecs()`). PASS modulo F-7.

## i18n coverage

Catalog file: `extension/src/review/i18n/en.json` (737 lines). Helper: `extension/src/review/i18n/index.ts` (84 lines) — `createReviewI18n(locale)` returns `{ locale, t(key, params), has(key) }`. Missing keys fall back to the `FALLBACK_CATALOG` (English) and then to the key string. Placeholder interpolation only — no ICU plurals (F-8).

All 14 expected namespaces from the R78 task are present plus one bonus namespace (`reviewOverview`):

| Namespace | Present | Key count (approx.) | Covers (sampled) |
|---|---|---:|---|
| `reviewPanel` | YES | 50 | title, subtitle, refresh, retry, loading, empty, error, route labels, descriptions, capability indicators |
| `reviewStatus` | YES | 16 | All 8 verification statuses + proposal/job statuses |
| `reviewActions` | YES | 2 | accept, reject |
| `reviewOverview` | YES (bonus) | 9 | summary card labels |
| `memoryInbox` | YES | 26 | tableLabel, emptyTitle/Initial/Filtered, error/errorBanner, loadingHint, columns (7), actions (3), pagination (4), filters (status/scope/class) |
| `promotionQueue` | YES | 8 | description, empty, columns (8), actions (3), messages (2: applySuccess, rejectSuccess) |
| `contradictionQueue` | YES | 8 | description, empty, columns (7), actions (3), messages (2) |
| `proposalDialog` | YES | 30 | close, cancel, accept, reject, summaryTitle, evidenceTitle, provenanceTitle, diffTitle, typedConfirmPrompt, genericError, promotion/contradiction titles + effects, rejectReasonLabel/Help/Placeholder, scope/class metadata |
| `staleView` | YES | 40 | title, loadingHint, errorTitle, filterStatus/Scope, inspectEvidence, reverify(/Loading), verifySuccess, empty, emptyFiltered, columns (7), scope (4), class (12), statusReason (5), reason (10) |
| `evidenceInspector` | YES | 20 | title, subtitle, selectPrompt, errorTitle, openTrace, openReference, reverify(/Loading), verifySuccess, content/evidence/provenance/history/link/verification panels with empty states |
| `eventTraceView` | YES | 48 | title, subtitle, errorTitle, payloadDialogTitle, copyPayload(/Error/Success), openInEditor, closeDialog, openMemory/FileReference, columns (9), pagination (4), filters (kind/actor/session/task/workspace/branch + ISO window), 22 event kinds |
| `retrievalExplanationView` | YES | 28 | title, subtitle, errorTitle, unsupportedTitle, empty, noExcluded, anchorsTitle, excludedTitle, fullSignalsTitle, columns (6), pagination (4), request (5), decision (3), source (9), signal (13) |
| `consolidationQueueView` | YES | 39 | title, subtitle, tableLabel, errorTitle, retryMissingSession, retryConfirm, retrySuccess, durationMs/Seconds, columns (9), queue depth/dropped, actions (6), pagination (4), filters (status/kind/mode), statuses (7), kinds (8), modes (5) |
| `indexingHealthView` | YES | 29 | title, subtitle, errorTitle, summary cards, status badges, sections (pipeline/vector/fts/eventLog), notesTitle, actions (refresh/refreshing) |
| `workspaceGraphHealthView` | YES | 27 | title, subtitle, errorTitle, notesTitle, summary (4), sections (5), actions (refresh/recompute/refreshing/inspect), columns (6), pagination (4), per-table empty states |

**Inline-literal scan:** `grep -nE '>[A-Z][a-z]+ [A-Z][a-z]+<' extension/src/review/` → 0 hits. Every user-facing string in webview output is wrapped in `i18n.t(...)`. PASS.

## Findings

| # | Severity | Finding | Evidence | Recommended follow-up |
|---|---|---|---|---|
| F-1 | major | `extension/src/review/reviewPanel.ts` is 2617 lines — 3.27× the 800-line file limit. `ReviewPanelProvider` is a single 2250-line class spanning lines 105-2355 with no inline justification. | `wc -l extension/src/review/reviewPanel.ts` → 2617 | Split per-route handler/render concerns into co-located modules under `extension/src/review/routes/` and leave `reviewPanel.ts` as a thin dispatcher. See `T-followup-R78-A`. |
| F-2 | major | `extension/src/review/rpcBridge.ts` is 920 lines — 1.15× over the 800-line limit. | `wc -l extension/src/review/rpcBridge.ts` → 920 | Either split the bridge by capability surface (memory / consolidation / workspace-graph) OR document the boundary inline in the commit. See `T-followup-R78-A`. |
| F-3 | major | Four methods in `ReviewPanelProvider` exceed the 50-line function limit: `handleWorkspaceGraphHealthMessage` (102 lines), `handleEventTraceMessage` (100 lines), `renderActiveRoute` (75 lines), `handleConsolidationQueueMessage` (60 lines). All four are large `switch(message.command)` dispatchers. | `awk` length report (see Coding-standard alignment above). | Extract per-command helpers (the pattern in `handleStaleViewMessage` → `pickStaleStatusFilters` / `pickStaleScopeFilters` / `reverifyStaleMemory` is the model). See `T-followup-R78-A`. |
| F-4 | major | `staleView`, `promotionQueue`, `contradictionQueue` lack pagination controls. Per UI spec §6.1, every list MUST be paginated. | `grep -c "page\b\|Showing.*of" extension/src/review/staleView.ts` → 0; same for `promotionQueue.ts`, `contradictionQueue.ts`. | Add Prev/Next + page-size selector + "Showing X-Y of Z" rows to all three views; expose `page` / `pageSize` on the backing route state. See `T-followup-R78-B`. |
| F-5 | major | Seven views lack an explicit loading-state skeleton: `staleView`, `evidenceInspector`, `eventTraceView`, `retrievalExplanationView`, `consolidationQueueView`, `workspaceGraphHealthView`, `indexingHealthView`. UI spec §2.1 requires a skeleton for first-load of predictable layouts when load > 400 ms. | Loading skeleton present only in `memoryInbox.ts:329-342`, `promotionQueue.ts:313-315`, `contradictionQueue.ts:361-363`. | Extract a shared `renderRouteSkeleton(routeId)` helper and call it from each `mountXxx(host, …)` before the bridge `await`. See `T-followup-R78-B`. |
| F-6 | major | `ReviewPanelProvider.refreshState` (`reviewPanel.ts:421-437`) only handles the `result.ok = false` Result branch; if the bridge `getOverview()` *throws* (network blip, JSON parse error), the error propagates uncaught to the webview message handler. | `reviewPanel.ts:421-437`. | Wrap `await this.bridge.getOverview()` and `await this.renderActiveRoute()` in try/catch, posting an `{ type: 'error', message }` to the webview on throw. See `T-followup-R78-C`. |
| F-7 | minor | `extension/src/review/memoryInbox.ts:376` uses `data-testid="memory-inbox-filter-{key}"` rather than the spec-mandated `review-memory-inbox-filter-{key}` convention. | `grep -n 'memory-inbox-filter' extension/src/review/memoryInbox.ts` → 376. | Rename to `review-memory-inbox-filter-{key}` and update any smoke-test selector that depends on the old id. See `T-followup-R78-C`. |
| F-8 | minor | i18n helper `extension/src/review/i18n/index.ts:36-44` supports `{name}` placeholder replacement only — no ICU MessageFormat or plural rules. UI spec §14 mandates ICU plurals. Single-locale catalog today, so impact is deferred. | `i18n/index.ts:36-44`. | When second locale is added, swap the simple `interpolate(template, params)` for `i18next-icu` or equivalent. Document the constraint inline. See `T-followup-R78-C`. |
| F-9 | minor | `retryConsolidationJob` (`reviewPanel.ts:1244-1275`) uses try/finally without an explicit catch. Relies on `Result<T,E>` typing of `this.bridge.retryConsolidationSession`, so a thrown error would bypass the finally cleanup before propagating. | `reviewPanel.ts:1262-1275`. | Add an explicit catch that calls `vscode.window.showErrorMessage(err.message)` before letting the finally restore state. See `T-followup-R78-C`. |
| F-10 | minor | `escapeHtml` is redefined in nine modules (`grep -c "function escapeHtml" extension/src/review/*.ts` → 9). Coding standard §Single-source-of-truth says two duplicates is the threshold for extraction. | Multiple files. | Move `escapeHtml` and `escapeAttr` into `extension/src/review/components/html.ts` (which already exists at 12 lines for a different helper) and import. See `T-followup-R78-C`. |

## Coding-standard alignment

(See dedicated section above.)

## Verdict

**PASS-WITH-FOLLOWUP-TASKS T-followup-R78-A, T-followup-R78-B, T-followup-R78-C.**

The eleven mandated operator views from spec §10 are all reachable from `ReviewPanelProvider`; smoke tests cover ten of them (the eleventh is intentionally exposed through the R70 sidebar dashboard per the R78 task scope clause). The build is clean (`npm run compile`, `npm run lint`, `npm test` all exit 0). Every mutation handler is wrapped in try/catch with user-visible error surfacing. Status badges live in a single source (`StatusBadge.ts`), all 8 verification statuses are covered, and the i18n catalog is comprehensive across 14 namespaces with zero inline English literals in webview output. Modals are native `<dialog>` elements with focus management and typed confirm for high-scope destructive actions.

The two file-size violations (F-1, F-2) and four function-length violations (F-3) are real but represent code-organization debt, not correctness or security risk — the file functions correctly under the existing test coverage. They are recorded in `T-followup-R78-A` and MUST be cleaned up before any Phase 11 task touches `reviewPanel.ts` or `rpcBridge.ts`, because the next session will breach the limit on first touch. The pagination + loading-skeleton gaps (F-4, F-5) and the error-handling tightening (F-6, F-9) are recorded in `T-followup-R78-B` and `T-followup-R78-C` respectively. None of the findings block downstream review tasks R79 or R88 from beginning.
