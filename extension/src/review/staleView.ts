import { ReviewI18n } from './i18n';
import { renderStatusBadge } from './components/StatusBadge';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewMemory } from './rpcPayloads';

// Status taxonomy from `## 8. Verification Engine`; stale surfacing requirement from
// `## Stale Memory Leakage` in docs/plans/2026-05-16-cognitive-workspace-fork-plan.md.
export const STALE_FAMILY_STATUSES = [
    'stale',
    'contradicted',
    'superseded',
    'expired',
    'invalidated',
] as const;

type StaleSortKey = 'status' | 'memoryClass' | 'scope' | 'staleSince';
type SortDirection = 'asc' | 'desc';

export interface StaleViewState {
    selectedStatuses: string[];
    selectedScopes: string[];
    sortBy: StaleSortKey;
    sortDirection: SortDirection;
    reverifyInFlightId?: string;
    rowErrors: Record<string, string>;
}

export interface StaleViewHost {
    readonly state: StaleViewState;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountStaleView(
    host: StaleViewHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n
): Promise<void> {
    try {
        const result = await bridge.listStaleMemories(undefined, 200);
        if (!result.ok) {
            host.reportError?.(result.error.message);
            host.setContent(renderError(i18n, result.error.message));
            return;
        }
        const staleMemories = result.value.memories.filter((memory) =>
            STALE_FAMILY_STATUSES.includes(memory.verificationStatus as typeof STALE_FAMILY_STATUSES[number])
        );
        host.setContent(renderTable(staleMemories, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderTable(memories: ReviewMemory[], state: StaleViewState, i18n: ReviewI18n): string {
    const scopes = uniqueValues(memories.map((memory) => memory.scope));
    const filtered = applyFilters(memories, state);
    const rows = sortMemories(filtered, state).map((memory) => renderRow(memory, state, i18n)).join('');
    const activeStatuses = state.selectedStatuses.length
        ? state.selectedStatuses.map((status) => i18n.t(`reviewStatus.${status}`)).join(', ')
        : i18n.t('staleView.allStatuses');
    const activeScopes = state.selectedScopes.length
        ? state.selectedScopes.map((scope) => i18n.t(`staleView.scope.${scope}`)).join(', ')
        : i18n.t('staleView.allScopes');
    const emptyMessage = memories.length === 0
        ? i18n.t('staleView.empty')
        : i18n.t('staleView.emptyFiltered');

    return `<section class="route-stack" data-testid="review-stale-view">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('staleView.title'))}</h3>
      <p>${escapeHtml(i18n.t('staleView.loadingHint'))}</p>
    </div>
    <div class="toolbar-actions">
      <button data-command="pickStaleStatusFilters">${escapeHtml(i18n.t('staleView.filterStatus'))}</button>
      <button data-command="pickStaleScopeFilters">${escapeHtml(i18n.t('staleView.filterScope'))}</button>
    </div>
  </div>
  <div class="filter-bar">
    <span class="filter-pill">${escapeHtml(i18n.t('staleView.activeStatus', { value: activeStatuses }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('staleView.activeScope', { value: activeScopes }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('staleView.scopeCount', { count: scopes.length }))}</span>
  </div>
  ${rows
        ? `<div class="table-shell"><table class="data-table">
      <thead>
        <tr>
          ${renderHeader('status', state, i18n, 'staleView.column.status')}
          ${renderHeader('memoryClass', state, i18n, 'staleView.column.class')}
          ${renderHeader('scope', state, i18n, 'staleView.column.scope')}
          <th>${escapeHtml(i18n.t('staleView.column.content'))}</th>
          ${renderHeader('staleSince', state, i18n, 'staleView.column.staleSince')}
          <th>${escapeHtml(i18n.t('staleView.column.reason'))}</th>
          <th>${escapeHtml(i18n.t('staleView.column.actions'))}</th>
        </tr>
      </thead>
      <tbody>${rows}</tbody>
    </table></div>`
        : `<div class="placeholder"><h3>${escapeHtml(emptyMessage)}</h3></div>`}
</section>`;
}

function renderHeader(
    sortBy: StaleSortKey,
    state: StaleViewState,
    i18n: ReviewI18n,
    key: string
): string {
    const isActive = state.sortBy === sortBy;
    const direction = isActive && state.sortDirection === 'asc' ? '↑' : isActive ? '↓' : '↕';
    return `<th><button class="table-sort" data-command="sortStaleView" data-sort-by="${escapeHtml(sortBy)}">
${escapeHtml(i18n.t(key))} ${escapeHtml(direction)}</button></th>`;
}

function renderRow(memory: ReviewMemory, state: StaleViewState, i18n: ReviewI18n): string {
    const isReverifying = state.reverifyInFlightId === memory.id;
    const rowError = state.rowErrors[memory.id];
    const staleSince = formatTimestamp(memory.lastVerifiedAt ?? memory.createdAt, i18n);
    return `<tr class="row-link" data-command="openEvidenceInspector" data-memory-id="${escapeHtml(memory.id)}">
  <td>${renderStatusBadge(memory.verificationStatus, i18n)}</td>
  <td>${escapeHtml(formatClass(memory.memoryClass, i18n))}</td>
  <td>${escapeHtml(formatScope(memory.scope, i18n))}</td>
  <td>
    <div class="cell-stack">
      <button class="link-button" data-command="openEvidenceInspector" data-memory-id="${escapeHtml(memory.id)}">
        ${escapeHtml(truncate(memory.content, 96))}
      </button>
      <small>${escapeHtml(memory.id)}</small>
    </div>
  </td>
  <td>${escapeHtml(staleSince)}</td>
  <td>${escapeHtml(formatStaleReason(memory, i18n))}</td>
  <td>
    <div class="row-actions">
      <button data-command="openEvidenceInspector" data-memory-id="${escapeHtml(memory.id)}">
        ${escapeHtml(i18n.t('staleView.inspectEvidence'))}
      </button>
      <button data-command="reverifyStaleMemory" data-memory-id="${escapeHtml(memory.id)}" ${isReverifying ? 'disabled' : ''}>
        ${isReverifying ? escapeHtml(i18n.t('staleView.reverifyLoading')) : escapeHtml(i18n.t('staleView.reverify'))}
      </button>
    </div>
    ${rowError ? `<div class="inline-error" role="alert">${escapeHtml(rowError)}</div>` : ''}
  </td>
</tr>`;
}

function applyFilters(memories: ReviewMemory[], state: StaleViewState): ReviewMemory[] {
    return memories.filter((memory) => {
        if (state.selectedStatuses.length && !state.selectedStatuses.includes(memory.verificationStatus)) {
            return false;
        }
        if (state.selectedScopes.length && !state.selectedScopes.includes(memory.scope)) {
            return false;
        }
        return true;
    });
}

function sortMemories(memories: ReviewMemory[], state: StaleViewState): ReviewMemory[] {
    const sorted = [...memories].sort((left, right) => compareMemory(left, right, state.sortBy));
    return state.sortDirection === 'desc' ? sorted.reverse() : sorted;
}

function compareMemory(left: ReviewMemory, right: ReviewMemory, sortBy: StaleSortKey): number {
    switch (sortBy) {
        case 'memoryClass':
            return left.memoryClass.localeCompare(right.memoryClass);
        case 'scope':
            return left.scope.localeCompare(right.scope);
        case 'staleSince':
            return (left.lastVerifiedAt ?? left.createdAt ?? 0) - (right.lastVerifiedAt ?? right.createdAt ?? 0);
        case 'status':
        default:
            return left.verificationStatus.localeCompare(right.verificationStatus);
    }
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-stale-view"><div class="banner error" role="alert">
  <strong>${escapeHtml(i18n.t('staleView.errorTitle'))}</strong>
  <p>${escapeHtml(message)}</p>
</div></section>`;
}

function formatStaleReason(memory: ReviewMemory, i18n: ReviewI18n): string {
    const rawReason = memory.staleReason?.trim();
    if (!rawReason) {
        return i18n.t(`staleView.statusReason.${memory.verificationStatus}`);
    }
    const normalized = rawReason.toLowerCase().replaceAll(' ', '_').replaceAll('-', '_');
    if (i18n.has(`staleView.reason.${normalized}`)) {
        return i18n.t(`staleView.reason.${normalized}`);
    }
    return rawReason;
}

function formatClass(memoryClass: string, i18n: ReviewI18n): string {
    return i18n.has(`staleView.class.${memoryClass}`)
        ? i18n.t(`staleView.class.${memoryClass}`)
        : memoryClass;
}

function formatScope(scope: string, i18n: ReviewI18n): string {
    return i18n.has(`staleView.scope.${scope}`) ? i18n.t(`staleView.scope.${scope}`) : scope;
}

function truncate(value: string, length: number): string {
    return value.length <= length ? value : `${value.slice(0, length - 1)}…`;
}

function formatTimestamp(value: number | undefined, i18n: ReviewI18n): string {
    if (!value) {
        return i18n.t('staleView.notAvailable');
    }
    return new Date(value * 1000).toLocaleString();
}

function uniqueValues(values: string[]): string[] {
    return [...new Set(values.filter(Boolean))].sort((left, right) => left.localeCompare(right));
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
