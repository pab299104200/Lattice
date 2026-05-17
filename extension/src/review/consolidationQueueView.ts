import { escapeAttr, escapeHtml } from './components/html';
import { renderStatusBadge } from './components/StatusBadge';
import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewConsolidationJob, ReviewConsolidationQueueData } from './rpcPayloads';

type ConsolidationQueueSortKey = 'status' | 'kind' | 'mode' | 'createdAt' | 'duration';
type SortDirection = 'asc' | 'desc';

export interface ConsolidationQueueViewState {
    inlineError?: string;
    jobs?: ReviewConsolidationQueueData;
    page: number;
    pageSize: number;
    selectedKinds: string[];
    selectedModes: string[];
    selectedStatuses: string[];
    sortBy: ConsolidationQueueSortKey;
    sortDirection: SortDirection;
    retryingJobId?: string;
}

export interface ConsolidationQueueHost {
    readonly state: ConsolidationQueueViewState;
    remember(data: ReviewConsolidationQueueData): void;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountConsolidationQueueView(
    host: ConsolidationQueueHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n
): Promise<void> {
    try {
        const [jobsResult, depthResult] = await Promise.all([
            bridge.listConsolidationJobs(),
            bridge.getConsolidationQueueDepth(),
        ]);
        if (!jobsResult.ok) {
            host.reportError?.(jobsResult.error.message);
            host.setContent(renderError(i18n, jobsResult.error.message));
            return;
        }
        if (!depthResult.ok) {
            host.reportError?.(depthResult.error.message);
            host.setContent(renderError(i18n, depthResult.error.message));
            return;
        }
        const data = {
            ...jobsResult.value,
            queueDepth: depthResult.value,
        };
        host.remember(data);
        host.setContent(renderView(data, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderView(
    data: ReviewConsolidationQueueData,
    state: ConsolidationQueueViewState,
    i18n: ReviewI18n
): string {
    const filtered = filterJobs(data.jobs, state);
    const sorted = sortJobs(filtered, state);
    const page = paginate(sorted, state.page, state.pageSize);
    const rows = page.items.map((job) => renderRow(job, state, i18n)).join('');
    const droppedBanner = data.queueDepth.droppedCount > 0
        ? `<div class="banner warning" role="status">
  <strong>${escapeHtml(i18n.t('consolidationQueueView.queue.droppedTitle', { count: data.queueDepth.droppedCount }))}</strong>
  <p>${escapeHtml(i18n.t('consolidationQueueView.queue.droppedMessage'))}</p>
  <div class="row-actions">
    <button type="button" data-command="openConsolidationFailures">${escapeHtml(i18n.t('consolidationQueueView.actions.openFailures'))}</button>
  </div>
</div>`
        : '';
    return `<section class="route-stack" data-testid="review-consolidation-queue">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('consolidationQueueView.title'))}</h3>
      <p>${escapeHtml(i18n.t('consolidationQueueView.subtitle'))}</p>
    </div>
    <div class="toolbar-actions">
      ${filterButton('pickConsolidationStatuses', i18n.t('consolidationQueueView.filters.statusButton', { count: countLabel(state.selectedStatuses, data.jobs, 'status') }))}
      ${filterButton('pickConsolidationKinds', i18n.t('consolidationQueueView.filters.kindButton', { count: countLabel(state.selectedKinds, data.jobs, 'kind') }))}
      ${filterButton('pickConsolidationModes', i18n.t('consolidationQueueView.filters.modeButton', { count: countLabel(state.selectedModes, data.jobs, 'mode') }))}
      <button type="button" data-command="clearConsolidationFilters">${escapeHtml(i18n.t('consolidationQueueView.actions.clearFilters'))}</button>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  ${renderQueueSummary(data, i18n)}
  ${droppedBanner}
  <div class="filter-summary">
    <span class="filter-pill">${escapeHtml(i18n.t('consolidationQueueView.filters.activeStatus', { value: summarize(state.selectedStatuses, 'consolidationQueueView.status', i18n) }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('consolidationQueueView.filters.activeKind', { value: summarize(state.selectedKinds, 'consolidationQueueView.kind', i18n) }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('consolidationQueueView.filters.activeMode', { value: summarize(state.selectedModes, 'consolidationQueueView.mode', i18n) }))}</span>
  </div>
  ${rows ? renderTable(rows, state, i18n) : renderEmpty(filtered.length === 0 && data.jobs.length > 0, i18n)}
  ${renderPagination(sorted.length, state, i18n)}
</section>`;
}

function renderQueueSummary(data: ReviewConsolidationQueueData, i18n: ReviewI18n): string {
    return `<div class="summary-grid">
  ${summaryCard(i18n.t('consolidationQueueView.queue.currentDepth'), `${data.queueDepth.currentDepth}`)}
  ${summaryCard(i18n.t('consolidationQueueView.queue.maxDepth'), `${data.queueDepth.maxDepth}`)}
  ${summaryCard(i18n.t('consolidationQueueView.queue.droppedCount'), `${data.queueDepth.droppedCount}`)}
  ${summaryCard(i18n.t('consolidationQueueView.queue.visibleJobs'), `${data.jobs.length}`)}
</div>`;
}

function renderTable(rows: string, state: ConsolidationQueueViewState, i18n: ReviewI18n): string {
    return `<div class="table-shell"><table class="data-table" aria-label="${escapeAttr(i18n.t('consolidationQueueView.tableLabel'))}">
  <thead><tr>
    ${sortableHeader('status', state, i18n, 'consolidationQueueView.columns.status')}
    ${sortableHeader('kind', state, i18n, 'consolidationQueueView.columns.kind')}
    ${sortableHeader('mode', state, i18n, 'consolidationQueueView.columns.mode')}
    ${sortableHeader('createdAt', state, i18n, 'consolidationQueueView.columns.createdAt')}
    <th>${escapeHtml(i18n.t('consolidationQueueView.columns.startedAt'))}</th>
    <th>${escapeHtml(i18n.t('consolidationQueueView.columns.completedAt'))}</th>
    ${sortableHeader('duration', state, i18n, 'consolidationQueueView.columns.duration')}
    <th>${escapeHtml(i18n.t('consolidationQueueView.columns.llmModel'))}</th>
    <th>${escapeHtml(i18n.t('consolidationQueueView.columns.resultSummary'))}</th>
    <th>${escapeHtml(i18n.t('consolidationQueueView.columns.actions'))}</th>
  </tr></thead>
  <tbody>${rows}</tbody>
</table></div>`;
}

function renderRow(job: ReviewConsolidationJob, state: ConsolidationQueueViewState, i18n: ReviewI18n): string {
    const retryDisabled = !job.sessionId || state.retryingJobId === job.jobId;
    return `<tr>
  <td>${renderStatusBadge(job.status, i18n)}</td>
  <td>${escapeHtml(labelFor(job.kind, 'consolidationQueueView.kind', i18n))}</td>
  <td>${escapeHtml(labelFor(job.mode, 'consolidationQueueView.mode', i18n))}</td>
  <td>${escapeHtml(formatTimestamp(job.createdAt, i18n))}</td>
  <td>${escapeHtml(formatTimestamp(job.startedAt, i18n))}</td>
  <td>${escapeHtml(formatTimestamp(job.completedAt, i18n))}</td>
  <td>${escapeHtml(formatDuration(job.durationMs, i18n))}</td>
  <td>${escapeHtml(job.llmModel ?? i18n.t('consolidationQueueView.notAvailable'))}</td>
  <td><div class="cell-stack"><span>${escapeHtml(job.resultSummary)}</span><small>${escapeHtml(job.jobId)}</small></div></td>
  <td>
    <div class="row-actions">
      <button type="button" data-command="openConsolidationEventTrace" data-value="${escapeAttr(job.jobId)}">${escapeHtml(i18n.t('consolidationQueueView.actions.inspect'))}</button>
      <button type="button" data-command="retryConsolidationJob" data-value="${escapeAttr(job.jobId)}" ${retryDisabled ? 'disabled' : ''}>${escapeHtml(state.retryingJobId === job.jobId ? i18n.t('consolidationQueueView.actions.retrying') : i18n.t('consolidationQueueView.actions.retry'))}</button>
    </div>
  </td>
</tr>`;
}

function renderPagination(total: number, state: ConsolidationQueueViewState, i18n: ReviewI18n): string {
    if (total === 0) {
        return '';
    }
    const totalPages = Math.max(1, Math.ceil(total / state.pageSize));
    const safePage = clampPage(state.page, totalPages);
    const start = (safePage - 1) * state.pageSize + 1;
    const end = Math.min(total, safePage * state.pageSize);
    return `<div class="pagination-bar">
  <span>${escapeHtml(i18n.t('consolidationQueueView.pagination.showing', { start, end, total }))}</span>
  <div class="row-actions">
    <button type="button" data-command="pageConsolidationQueue" data-page="${safePage - 1}" ${safePage <= 1 ? 'disabled' : ''}>${escapeHtml(i18n.t('consolidationQueueView.pagination.previous'))}</button>
    <span>${escapeHtml(i18n.t('consolidationQueueView.pagination.page', { page: safePage, totalPages }))}</span>
    <button type="button" data-command="pageConsolidationQueue" data-page="${safePage + 1}" ${safePage >= totalPages ? 'disabled' : ''}>${escapeHtml(i18n.t('consolidationQueueView.pagination.next'))}</button>
    ${pageSizeButton(25, state.pageSize)}
    ${pageSizeButton(50, state.pageSize)}
    ${pageSizeButton(100, state.pageSize)}
  </div>
</div>`;
}

function renderEmpty(filtered: boolean, i18n: ReviewI18n): string {
    const message = filtered
        ? i18n.t('consolidationQueueView.emptyFiltered')
        : i18n.t('consolidationQueueView.empty');
    return `<div class="placeholder"><h3>${escapeHtml(message)}</h3></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-consolidation-queue"><div class="banner error" role="alert"><strong>${escapeHtml(i18n.t('consolidationQueueView.errorTitle'))}</strong><p>${escapeHtml(message)}</p></div></section>`;
}

function filterJobs(jobs: ReviewConsolidationJob[], state: ConsolidationQueueViewState): ReviewConsolidationJob[] {
    return jobs.filter((job) => {
        if (state.selectedStatuses.length && !state.selectedStatuses.includes(job.status)) {
            return false;
        }
        if (state.selectedKinds.length && !state.selectedKinds.includes(job.kind)) {
            return false;
        }
        if (state.selectedModes.length && !state.selectedModes.includes(job.mode)) {
            return false;
        }
        return true;
    });
}

function sortJobs(jobs: ReviewConsolidationJob[], state: ConsolidationQueueViewState): ReviewConsolidationJob[] {
    const sorted = [...jobs].sort((left, right) => compareJobs(left, right, state.sortBy));
    return state.sortDirection === 'desc' ? sorted.reverse() : sorted;
}

function compareJobs(left: ReviewConsolidationJob, right: ReviewConsolidationJob, sortBy: ConsolidationQueueSortKey): number {
    switch (sortBy) {
        case 'status':
            return left.status.localeCompare(right.status);
        case 'kind':
            return left.kind.localeCompare(right.kind);
        case 'mode':
            return left.mode.localeCompare(right.mode);
        case 'duration':
            return (left.durationMs ?? -1) - (right.durationMs ?? -1);
        case 'createdAt':
        default:
            return (Date.parse(left.createdAt ?? '') || 0) - (Date.parse(right.createdAt ?? '') || 0);
    }
}

function paginate<T>(items: T[], page: number, pageSize: number): { items: T[] } {
    const totalPages = Math.max(1, Math.ceil(items.length / pageSize));
    const safePage = clampPage(page, totalPages);
    const start = (safePage - 1) * pageSize;
    return { items: items.slice(start, start + pageSize) };
}

function clampPage(page: number, totalPages: number): number {
    return Math.min(Math.max(page, 1), totalPages);
}

function sortableHeader(
    sortBy: ConsolidationQueueSortKey,
    state: ConsolidationQueueViewState,
    i18n: ReviewI18n,
    key: string
): string {
    const direction = state.sortBy === sortBy ? (state.sortDirection === 'asc' ? '↑' : '↓') : '↕';
    return `<th><button type="button" class="table-sort" data-command="sortConsolidationQueue" data-sort-by="${escapeAttr(sortBy)}">${escapeHtml(i18n.t(key))} ${direction}</button></th>`;
}

function labelFor(value: string, prefix: string, i18n: ReviewI18n): string {
    return i18n.has(`${prefix}.${value}`) ? i18n.t(`${prefix}.${value}`) : value;
}

function summarize(values: string[], prefix: string, i18n: ReviewI18n): string {
    if (!values.length) {
        return i18n.t('consolidationQueueView.filters.all');
    }
    return values.map((value) => labelFor(value, prefix, i18n)).join(', ');
}

function formatTimestamp(value: string | undefined, i18n: ReviewI18n): string {
    if (!value) {
        return i18n.t('consolidationQueueView.notAvailable');
    }
    const timestamp = Date.parse(value);
    if (Number.isNaN(timestamp)) {
        return value;
    }
    return new Date(timestamp).toLocaleString();
}

function formatDuration(durationMs: number | undefined, i18n: ReviewI18n): string {
    if (durationMs === undefined) {
        return i18n.t('consolidationQueueView.notAvailable');
    }
    if (durationMs < 1000) {
        return i18n.t('consolidationQueueView.durationMs', { value: durationMs });
    }
    return i18n.t('consolidationQueueView.durationSeconds', { value: (durationMs / 1000).toFixed(1) });
}

function filterButton(command: string, label: string): string {
    return `<button type="button" data-command="${escapeAttr(command)}">${escapeHtml(label)}</button>`;
}

function summaryCard(label: string, value: string): string {
    return `<div class="card"><small>${escapeHtml(label)}</small><strong>${escapeHtml(value)}</strong></div>`;
}

function pageSizeButton(size: number, active: number): string {
    return `<button type="button" data-command="setConsolidationQueuePageSize" data-page-size="${size}" ${size === active ? 'disabled' : ''}>${size}/page</button>`;
}

function countLabel(values: string[], jobs: ReviewConsolidationJob[], field: 'status' | 'kind' | 'mode'): number {
    if (values.length) {
        return values.length;
    }
    return new Set(jobs.map((job) => job[field])).size;
}
