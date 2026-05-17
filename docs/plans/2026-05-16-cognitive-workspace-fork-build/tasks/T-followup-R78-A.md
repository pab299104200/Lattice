# T-followup-R78-A — Phase 10 file/function-length cleanup (reviewPanel.ts + rpcBridge.ts)

**Phase:** 10 (follow-up from R78)
**Type:** frontend cleanup + tests
**Model class:** advanced
**Depends on:** R78 (PASS-WITH-FOLLOWUP)
**Opened by:** R78 (Frontend review — Phase 10 human review UI)
**Spec anchor:** [§10. Human Review Surface](../../2026-05-16-cognitive-workspace-fork-plan.md#10-human-review-surface)
**Standards:** [Cadres coding standard](../../../../../shared/templates/coding.md) §Hard limits (file ≤ 800 lines, function ≤ 50 lines)

## Finding context

R78 reviewed T71–T77 and confirmed every Phase 10 spec deliverable is met and every spec-mandated smoke test passes. Three findings from the review's `## Findings` table address the coding-standard Hard Limits violations:

- **F-1** — `extension/src/review/reviewPanel.ts` is **2617 lines** (3.27× the 800-line file limit). `ReviewPanelProvider` is a single 2250-line class spanning lines 105–2355.
- **F-2** — `extension/src/review/rpcBridge.ts` is **920 lines** (1.15× the 800-line file limit).
- **F-3** — Four `ReviewPanelProvider` methods exceed the 50-line function limit:
  - `handleWorkspaceGraphHealthMessage` (102 lines, `reviewPanel.ts:1185-1286`)
  - `handleEventTraceMessage` (100 lines, `reviewPanel.ts:977-1077`)
  - `renderActiveRoute` (75 lines, `reviewPanel.ts:539-613`)
  - `handleConsolidationQueueMessage` (60 lines, `reviewPanel.ts:1009-1068`)

The next Phase 11 task that touches `reviewPanel.ts` will breach the limit on first touch. Splitting now is cheaper than splitting under deadline.

## Goal

Bring `reviewPanel.ts` and `rpcBridge.ts` under the coding standard Hard Limits without regressing any spec view, smoke test, or i18n key.

## Scope

1. **F-1 — Split `reviewPanel.ts` per route.** Create `extension/src/review/routes/` with one file per route handler (`memoryInboxRoute.ts`, `staleViewRoute.ts`, `evidenceInspectorRoute.ts`, `eventTraceRoute.ts`, `retrievalExplanationRoute.ts`, `consolidationQueueRoute.ts`, `indexingHealthRoute.ts`, `workspaceGraphHealthRoute.ts`, `promotionQueueRoute.ts`, `contradictionQueueRoute.ts`, `usefulnessMetricsRoute.ts`). Each file owns its `handleXxxMessage`, `renderXxx`, and per-route state. `reviewPanel.ts` retains the `ReviewPanelProvider` class, route registry, `refreshState`, `postState`, and `dispatchRpcMethod` only.

2. **F-2 — Split `rpcBridge.ts` per capability surface.** Create `extension/src/review/bridges/` with one file per capability domain (`memoryBridge.ts`, `consolidationBridge.ts`, `workspaceGraphBridge.ts`, `eventTraceBridge.ts`, `retrievalBridge.ts`, `proposalsBridge.ts`). `rpcBridge.ts` keeps only the public `ReviewRpcBridgeContract` interface, `ReviewBridgeCapabilities` type, capability detection (`getCapabilities`), and the composite `ReviewRpcBridge` constructor.

3. **F-3 — Extract per-command helpers from the four long dispatchers.** Each `switch` branch becomes a private helper named for its command (e.g., `handleWorkspaceGraphHealthMessage`'s `'refreshWorkspaceGraphPanel'` case becomes `private async refreshWorkspaceGraphPanel(panel: WorkspaceGraphPanel)`). The dispatch method should be a ≤ 30-line `switch` that delegates.

## Constraints

- **No behavior change.** Every existing smoke test (`extension/src/test/review.test.ts`) must pass unchanged. No new tests are required; tests already cover all 11 routes plus activation/focus/empty/error paths.
- **No new files exceed 800 lines.** Run `wc -l extension/src/review/{**/*,*}.ts` after the split and confirm.
- **No function exceeds 50 lines.** Run `awk '/^\s*(private |public |protected |export )?(async )?function |^\s*(private |public |protected )(async )?[a-zA-Z_]+\(/ {…}'` equivalent and confirm.
- **Maintain `escapeHtml` discipline.** When extracting, do not introduce another copy. (F-10 is tracked in T-followup-R78-C — do not pre-emptively fix it here, but do not regress.)
- **Keep import surface stable.** `extension/src/extension.ts` imports `ReviewPanelProvider` from `./review/reviewPanel`; that import path must remain.

## Verification commands

- `cd extension && npm run compile` → exit 0.
- `cd extension && npm run lint` → exit 0.
- `cd extension && npm test` → exit 0; all `review.test.ts` smoke tests still pass.
- `wc -l extension/src/review/*.ts extension/src/review/routes/*.ts extension/src/review/bridges/*.ts extension/src/review/components/*.ts extension/src/review/i18n/*.ts` — every file ≤ 800 lines.
- `grep -c "function escapeHtml" extension/src/review/**/*.ts` does not increase from current count.

## Definition of done

- [ ] `reviewPanel.ts` ≤ 800 lines.
- [ ] `rpcBridge.ts` ≤ 800 lines.
- [ ] Every method in `ReviewPanelProvider` ≤ 50 lines.
- [ ] All verification commands pass.
- [ ] R78 review findings F-1, F-2, F-3 marked addressed.
