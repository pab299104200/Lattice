import { escapeAttr, escapeHtml } from './components/html';
import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import {
    ReviewRetrievalCandidate,
    ReviewRetrievalExcludedCandidate,
    ReviewRetrievalExplanation,
} from './rpcPayloads';

type RetrievalSortKey = 'score' | 'source' | 'decision';
type SortDirection = 'asc' | 'desc';

export interface RetrievalExplanationViewState {
    inlineError?: string;
    lastExplanation?: ReviewRetrievalExplanation;
    page: number;
    pageSize: number;
    requestId?: string;
    sortBy: RetrievalSortKey;
    sortDirection: SortDirection;
}

export interface RetrievalExplanationHost {
    readonly state: RetrievalExplanationViewState;
    rememberExplanation(explanation: ReviewRetrievalExplanation): void;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountRetrievalExplanationView(
    host: RetrievalExplanationHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n,
    requestId?: string
): Promise<void> {
    if (!requestId) {
        host.setContent(renderPrompt(i18n));
        return;
    }
    try {
        const result = await bridge.getRetrievalExplanation(requestId);
        if (!result.ok) {
            host.reportError?.(result.error.message);
            host.setContent(renderError(i18n, result.error.message));
            return;
        }
        host.rememberExplanation(result.value);
        host.setContent(renderExplanation(result.value, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderExplanation(
    explanation: ReviewRetrievalExplanation,
    state: RetrievalExplanationViewState,
    i18n: ReviewI18n
): string {
    if (!explanation.supported) {
        return `<section class="route-stack" data-testid="review-retrieval-explanation">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('retrievalExplanationView.title'))}</h3>
      <p>${escapeHtml(i18n.t('retrievalExplanationView.subtitle', { requestId: explanation.requestId }))}</p>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  <div class="placeholder">
    <h3>${escapeHtml(i18n.t('retrievalExplanationView.unsupportedTitle'))}</h3>
    <p>${escapeHtml(explanation.reason)}</p>
  </div>
</section>`;
    }
    const candidates = sortCandidates(explanation.candidates ?? [], state);
    const page = paginate(candidates, state.page, state.pageSize);
    const rows = page.items.map((candidate) => renderCandidateRow(candidate, i18n)).join('');
    const excludedRows = (explanation.excludedCandidates ?? [])
        .map((candidate) => renderExcludedCandidate(candidate, i18n))
        .join('');
    return `<section class="route-stack" data-testid="review-retrieval-explanation">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('retrievalExplanationView.title'))}</h3>
      <p>${escapeHtml(i18n.t('retrievalExplanationView.subtitle', { requestId: explanation.requestId }))}</p>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  ${renderHeader(explanation, i18n)}
  ${renderAnchors(explanation, i18n)}
  ${rows ? renderCandidateTable(rows, state, i18n) : `<div class="placeholder"><h3>${escapeHtml(i18n.t('retrievalExplanationView.empty'))}</h3></div>`}
  ${renderPagination(candidates.length, state, i18n)}
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('retrievalExplanationView.excludedTitle'))}</h4>
    ${excludedRows || `<div class="placeholder compact"><h3>${escapeHtml(i18n.t('retrievalExplanationView.noExcluded'))}</h3></div>`}
  </section>
</section>`;
}

function renderHeader(explanation: ReviewRetrievalExplanation, i18n: ReviewI18n): string {
    const request = explanation.request;
    if (!request) {
        return '';
    }
    return `<div class="summary-grid">
  ${summaryCard(i18n.t('retrievalExplanationView.request.toolName'), request.toolName)}
  ${summaryCard(i18n.t('retrievalExplanationView.request.intent'), request.intentClassification)}
  ${summaryCard(i18n.t('retrievalExplanationView.request.timestamp'), request.timestamp ?? i18n.t('retrievalExplanationView.notAvailable'))}
  ${summaryCard(i18n.t('retrievalExplanationView.request.requestId'), explanation.requestId)}
</div>
<section class="panel-block">
  <h4>${escapeHtml(i18n.t('retrievalExplanationView.request.taskStatement'))}</h4>
  <p>${escapeHtml(request.taskStatement)}</p>
</section>`;
}

function renderAnchors(explanation: ReviewRetrievalExplanation, i18n: ReviewI18n): string {
    const anchors = explanation.anchors ?? [];
    return `<section class="panel-block">
  <h4>${escapeHtml(i18n.t('retrievalExplanationView.anchorsTitle'))}</h4>
  ${anchors.length ? `<div class="list-stack">${anchors.map((anchor) => `<article class="list-card"><strong>${escapeHtml(anchor.label)}</strong><span>${escapeHtml(anchor.provenance)}</span><small>${escapeHtml(anchor.kind)}</small></article>`).join('')}</div>` : `<div class="placeholder compact"><h3>${escapeHtml(i18n.t('retrievalExplanationView.noAnchors'))}</h3></div>`}
</section>`;
}

function renderCandidateTable(rows: string, state: RetrievalExplanationViewState, i18n: ReviewI18n): string {
    return `<div class="table-shell"><table class="data-table">
  <thead><tr>
    ${sortableHeader('source', state, i18n, 'retrievalExplanationView.columns.source')}
    <th>${escapeHtml(i18n.t('retrievalExplanationView.columns.identity'))}</th>
    ${sortableHeader('decision', state, i18n, 'retrievalExplanationView.columns.decision')}
    ${sortableHeader('score', state, i18n, 'retrievalExplanationView.columns.score')}
    <th>${escapeHtml(i18n.t('retrievalExplanationView.columns.topSignals'))}</th>
    <th>${escapeHtml(i18n.t('retrievalExplanationView.columns.reason'))}</th>
  </tr></thead>
  <tbody>${rows}</tbody>
</table></div>`;
}

function renderCandidateRow(candidate: ReviewRetrievalCandidate, i18n: ReviewI18n): string {
    const signals = candidate.topSignals
        .slice(0, 3)
        .map((signal) => `${labelForSignal(signal.signal, i18n)} ${signal.score.toFixed(2)}`)
        .join(', ');
    return `<tr>
  <td>${escapeHtml(labelForSource(candidate.source, i18n))}</td>
  <td>${escapeHtml(candidate.identity)}</td>
  <td>${renderDecisionBadge(candidate.decision, i18n)}</td>
  <td>${escapeHtml(candidate.score.toFixed(2))}</td>
  <td>${escapeHtml(signals || i18n.t('retrievalExplanationView.notAvailable'))}</td>
  <td>
    <details>
      <summary>${escapeHtml(candidate.reason)}</summary>
      <div class="signal-list">${candidate.allSignals.map((signal) => `<div class="signal-row"><span>${escapeHtml(labelForSignal(signal.signal, i18n))}</span><strong>${escapeHtml(signal.score.toFixed(2))}</strong></div>`).join('')}</div>
    </details>
  </td>
</tr>`;
}

function renderExcludedCandidate(candidate: ReviewRetrievalExcludedCandidate, i18n: ReviewI18n): string {
    return `<article class="list-card">
  <strong>${escapeHtml(candidate.identity)}</strong>
  <span>${escapeHtml(labelForSource(candidate.source, i18n))}</span>
  <small>${escapeHtml(i18n.t('retrievalExplanationView.excludedReason', { value: candidate.reason }))}</small>
  <small>${escapeHtml(i18n.t('retrievalExplanationView.excludedScore', { value: candidate.score.toFixed(2) }))}</small>
  <details>
    <summary>${escapeHtml(i18n.t('retrievalExplanationView.fullSignalsTitle'))}</summary>
    <div class="signal-list">${candidate.allSignals.map((signal) => `<div class="signal-row"><span>${escapeHtml(labelForSignal(signal.signal, i18n))}</span><strong>${escapeHtml(signal.score.toFixed(2))}</strong></div>`).join('')}</div>
  </details>
</article>`;
}

function renderPagination(total: number, state: RetrievalExplanationViewState, i18n: ReviewI18n): string {
    if (total === 0) {
        return '';
    }
    const totalPages = Math.max(1, Math.ceil(total / state.pageSize));
    const start = (state.page - 1) * state.pageSize + 1;
    const end = Math.min(total, state.page * state.pageSize);
    return `<div class="pagination-bar">
  <span>${escapeHtml(i18n.t('retrievalExplanationView.pagination.showing', { start, end, total }))}</span>
  <div class="row-actions">
    <button data-command="pageRetrievalExplanation" data-page="${state.page - 1}" ${state.page <= 1 ? 'disabled' : ''}>${escapeHtml(i18n.t('retrievalExplanationView.pagination.previous'))}</button>
    <span>${escapeHtml(i18n.t('retrievalExplanationView.pagination.page', { page: state.page, totalPages }))}</span>
    <button data-command="pageRetrievalExplanation" data-page="${state.page + 1}" ${state.page >= totalPages ? 'disabled' : ''}>${escapeHtml(i18n.t('retrievalExplanationView.pagination.next'))}</button>
    ${pageSizeButton(25, state.pageSize)}
    ${pageSizeButton(50, state.pageSize)}
    ${pageSizeButton(100, state.pageSize)}
  </div>
</div>`;
}

function renderDecisionBadge(decision: string, i18n: ReviewI18n): string {
    const tone = decision === 'included' ? 'success' : decision === 'expanded' ? 'info' : 'warning';
    return `<span class="status-badge status-badge--${tone}">${escapeHtml(i18n.t(`retrievalExplanationView.decision.${decision}`))}</span>`;
}

function renderPrompt(i18n: ReviewI18n): string {
    return `<div class="placeholder" data-testid="review-retrieval-explanation"><h3>${escapeHtml(i18n.t('retrievalExplanationView.selectPrompt'))}</h3></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-retrieval-explanation"><div class="banner error" role="alert"><strong>${escapeHtml(i18n.t('retrievalExplanationView.errorTitle'))}</strong><p>${escapeHtml(message)}</p></div></section>`;
}

function summaryCard(label: string, value: string): string {
    return `<div class="card"><small>${escapeHtml(label)}</small><strong>${escapeHtml(value)}</strong></div>`;
}

function sortCandidates(
    candidates: ReviewRetrievalCandidate[],
    state: RetrievalExplanationViewState
): ReviewRetrievalCandidate[] {
    const sorted = [...candidates].sort((left, right) => compareCandidates(left, right, state.sortBy));
    return state.sortDirection === 'desc' ? sorted.reverse() : sorted;
}

function compareCandidates(
    left: ReviewRetrievalCandidate,
    right: ReviewRetrievalCandidate,
    sortBy: RetrievalSortKey
): number {
    switch (sortBy) {
        case 'source':
            return left.source.localeCompare(right.source);
        case 'decision':
            return left.decision.localeCompare(right.decision);
        case 'score':
        default:
            return left.score - right.score;
    }
}

function paginate<T>(items: T[], page: number, pageSize: number): { items: T[] } {
    const totalPages = Math.max(1, Math.ceil(items.length / pageSize));
    const safePage = Math.min(Math.max(page, 1), totalPages);
    const start = (safePage - 1) * pageSize;
    return { items: items.slice(start, start + pageSize) };
}

function pageSizeButton(size: number, active: number): string {
    return `<button data-command="setRetrievalExplanationPageSize" data-page-size="${size}" ${size === active ? 'disabled' : ''}>${size}/page</button>`;
}

function sortableHeader(sortBy: RetrievalSortKey, state: RetrievalExplanationViewState, i18n: ReviewI18n, key: string): string {
    const direction = state.sortBy === sortBy ? (state.sortDirection === 'asc' ? '↑' : '↓') : '↕';
    return `<th><button class="table-sort" data-command="sortRetrievalExplanation" data-sort-by="${escapeAttr(sortBy)}">${escapeHtml(i18n.t(key))} ${direction}</button></th>`;
}

function labelForSource(source: string, i18n: ReviewI18n): string {
    return i18n.has(`retrievalExplanationView.source.${source}`) ? i18n.t(`retrievalExplanationView.source.${source}`) : source;
}

function labelForSignal(signal: string, i18n: ReviewI18n): string {
    return i18n.has(`retrievalExplanationView.signal.${signal}`) ? i18n.t(`retrievalExplanationView.signal.${signal}`) : signal;
}
