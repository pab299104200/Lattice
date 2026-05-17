import { escapeAttr, escapeHtml } from './components/html';
import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewFamilyCount, ReviewGraphDiagnosticRow, ReviewWorkspaceGraphHealth } from './rpcPayloads';

type FamilySortKey = 'family' | 'count';
type DiagnosticSortKey = 'identity' | 'reason';
type SortDirection = 'asc' | 'desc';
type GraphPanel = 'nodes' | 'edges' | 'brokenReferences' | 'staleEdges' | 'orphanSymbols';

export interface WorkspaceGraphHealthViewState {
    inlineError?: string;
    health?: ReviewWorkspaceGraphHealth;
    nodePage: number;
    edgePage: number;
    brokenPage: number;
    stalePage: number;
    orphanPage: number;
    pageSize: number;
    nodeSortBy: FamilySortKey;
    edgeSortBy: FamilySortKey;
    diagnosticSortBy: DiagnosticSortKey;
    nodeSortDirection: SortDirection;
    edgeSortDirection: SortDirection;
    diagnosticSortDirection: SortDirection;
    refreshingPanel?: GraphPanel;
}

export interface WorkspaceGraphHealthHost {
    readonly state: WorkspaceGraphHealthViewState;
    remember(health: ReviewWorkspaceGraphHealth): void;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountWorkspaceGraphHealthView(
    host: WorkspaceGraphHealthHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n
): Promise<void> {
    try {
        const result = await bridge.getWorkspaceGraphHealth();
        if (!result.ok) {
            host.reportError?.(result.error.message);
            host.setContent(renderError(i18n, result.error.message));
            return;
        }
        host.remember(result.value);
        host.setContent(renderView(result.value, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderView(
    health: ReviewWorkspaceGraphHealth,
    state: WorkspaceGraphHealthViewState,
    i18n: ReviewI18n
): string {
    const nodeTable = renderFamilyTable(
        'nodes',
        health.nodeFamilies,
        state.nodeSortBy,
        state.nodeSortDirection,
        state.nodePage,
        state.pageSize,
        state,
        i18n
    );
    const edgeTable = renderFamilyTable(
        'edges',
        health.edgeFamilies,
        state.edgeSortBy,
        state.edgeSortDirection,
        state.edgePage,
        state.pageSize,
        state,
        i18n
    );
    return `<section class="route-stack" data-testid="review-workspace-graph-health">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('workspaceGraphHealthView.title'))}</h3>
      <p>${escapeHtml(i18n.t('workspaceGraphHealthView.subtitle', { workspace: health.snapshot.workspace }))}</p>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  <div class="summary-grid">
    ${summaryCard(i18n.t('workspaceGraphHealthView.summary.nodes'), health.snapshot.nodes.toLocaleString())}
    ${summaryCard(i18n.t('workspaceGraphHealthView.summary.edges'), health.snapshot.edges.toLocaleString())}
    ${summaryCard(i18n.t('workspaceGraphHealthView.summary.files'), health.snapshot.files.toLocaleString())}
    ${summaryCard(i18n.t('workspaceGraphHealthView.summary.languages'), Object.keys(health.snapshot.languages).length.toLocaleString())}
  </div>
  ${renderPanel('nodes', i18n.t('workspaceGraphHealthView.sections.nodeFamilies'), nodeTable, state, i18n)}
  ${renderPanel('edges', i18n.t('workspaceGraphHealthView.sections.edgeFamilies'), edgeTable, state, i18n)}
  ${renderPanel('brokenReferences', i18n.t('workspaceGraphHealthView.sections.brokenReferences'), renderDiagnosticTable('brokenReferences', health.brokenReferences, state, i18n), state, i18n)}
  ${renderPanel('staleEdges', i18n.t('workspaceGraphHealthView.sections.staleEdges'), renderDiagnosticTable('staleEdges', health.staleEdges, state, i18n), state, i18n)}
  ${renderPanel('orphanSymbols', i18n.t('workspaceGraphHealthView.sections.orphanSymbols'), renderDiagnosticTable('orphanSymbols', health.orphanSymbols, state, i18n), state, i18n)}
  ${health.notes.length ? `<div class="placeholder compact"><h3>${escapeHtml(i18n.t('workspaceGraphHealthView.notesTitle'))}</h3><p>${escapeHtml(health.notes.join(' '))}</p></div>` : ''}
</section>`;
}

function renderPanel(
    panel: GraphPanel,
    title: string,
    body: string,
    state: WorkspaceGraphHealthViewState,
    i18n: ReviewI18n
): string {
    const actionKey = panel === 'nodes' || panel === 'edges'
        ? 'workspaceGraphHealthView.actions.refresh'
        : 'workspaceGraphHealthView.actions.recompute';
    const active = state.refreshingPanel === panel;
    return `<section class="panel-block">
  <div class="route-toolbar compact">
    <h4>${escapeHtml(title)}</h4>
    <div class="toolbar-actions">
      <button type="button" data-command="refreshWorkspaceGraphPanel" data-value="${escapeAttr(panel)}" ${active ? 'disabled' : ''}>${escapeHtml(i18n.t(active ? 'workspaceGraphHealthView.actions.refreshing' : actionKey))}</button>
    </div>
  </div>
  ${body}
</section>`;
}

function renderFamilyTable(
    panel: 'nodes' | 'edges',
    families: ReviewFamilyCount[],
    sortBy: FamilySortKey,
    sortDirection: SortDirection,
    page: number,
    pageSize: number,
    state: WorkspaceGraphHealthViewState,
    i18n: ReviewI18n
): string {
    const sorted = [...families].sort((left, right) => compareFamilies(left, right, sortBy));
    if (sortDirection === 'desc') {
        sorted.reverse();
    }
    const paged = paginate(sorted, page, pageSize);
    const rows = paged.items.map((family) => `<tr><td>${escapeHtml(family.family)}</td><td>${escapeHtml(family.count === undefined ? i18n.t('workspaceGraphHealthView.notReported') : family.count.toLocaleString())}</td><td>${escapeHtml(family.note ?? '')}</td></tr>`).join('');
    return `<div class="table-shell"><table class="data-table" aria-label="${escapeAttr(i18n.t(`workspaceGraphHealthView.${panel}.tableLabel`))}">
  <thead><tr>
    <th><button type="button" class="table-sort" data-command="sortWorkspaceGraphFamilies" data-value="${escapeAttr(panel)}:family">${escapeHtml(i18n.t('workspaceGraphHealthView.columns.family'))} ${sortIndicator(sortBy === 'family', sortDirection)}</button></th>
    <th><button type="button" class="table-sort" data-command="sortWorkspaceGraphFamilies" data-value="${escapeAttr(panel)}:count">${escapeHtml(i18n.t('workspaceGraphHealthView.columns.count'))} ${sortIndicator(sortBy === 'count', sortDirection)}</button></th>
    <th>${escapeHtml(i18n.t('workspaceGraphHealthView.columns.notes'))}</th>
  </tr></thead>
  <tbody>${rows}</tbody>
</table></div>
${renderPagination(panel === 'nodes' ? 'pageWorkspaceGraphNodes' : 'pageWorkspaceGraphEdges', sorted.length, page, pageSize, state, i18n)}`;
}

function renderDiagnosticTable(
    panel: 'brokenReferences' | 'staleEdges' | 'orphanSymbols',
    rows: ReviewGraphDiagnosticRow[],
    state: WorkspaceGraphHealthViewState,
    i18n: ReviewI18n
): string {
    const sorted = [...rows].sort((left, right) => compareDiagnostics(left, right, state.diagnosticSortBy));
    if (state.diagnosticSortDirection === 'desc') {
        sorted.reverse();
    }
    const page = panel === 'brokenReferences'
        ? state.brokenPage
        : panel === 'staleEdges'
            ? state.stalePage
            : state.orphanPage;
    const paged = paginate(sorted, page, state.pageSize);
    if (!rows.length) {
        return `<div class="placeholder compact"><h3>${escapeHtml(i18n.t(`workspaceGraphHealthView.${panel}.empty`))}</h3></div>`;
    }
    const command = panel === 'brokenReferences'
        ? 'openWorkspaceGraphReference'
        : panel === 'staleEdges'
            ? 'openWorkspaceGraphEvidence'
            : 'openWorkspaceGraphReference';
    const tableRows = paged.items.map((row) => `<tr>
  <td>${escapeHtml(row.identity)}</td>
  <td>${escapeHtml(row.reason)}</td>
  <td>${escapeHtml(row.detail ?? '')}</td>
  <td><button type="button" data-command="${escapeAttr(command)}" data-value="${escapeAttr(row.reference ?? row.relatedMemoryId ?? row.identity)}">${escapeHtml(i18n.t('workspaceGraphHealthView.actions.inspect'))}</button></td>
</tr>`).join('');
    const pageCommand = panel === 'brokenReferences'
        ? 'pageWorkspaceGraphBroken'
        : panel === 'staleEdges'
            ? 'pageWorkspaceGraphStale'
            : 'pageWorkspaceGraphOrphan';
    return `<div class="table-shell"><table class="data-table" aria-label="${escapeAttr(i18n.t(`workspaceGraphHealthView.${panel}.tableLabel`))}">
  <thead><tr>
    <th><button type="button" class="table-sort" data-command="sortWorkspaceGraphDiagnostics" data-value="identity">${escapeHtml(i18n.t('workspaceGraphHealthView.columns.identity'))} ${sortIndicator(state.diagnosticSortBy === 'identity', state.diagnosticSortDirection)}</button></th>
    <th><button type="button" class="table-sort" data-command="sortWorkspaceGraphDiagnostics" data-value="reason">${escapeHtml(i18n.t('workspaceGraphHealthView.columns.reason'))} ${sortIndicator(state.diagnosticSortBy === 'reason', state.diagnosticSortDirection)}</button></th>
    <th>${escapeHtml(i18n.t('workspaceGraphHealthView.columns.detail'))}</th>
    <th>${escapeHtml(i18n.t('workspaceGraphHealthView.columns.actions'))}</th>
  </tr></thead>
  <tbody>${tableRows}</tbody>
</table></div>
${renderPagination(pageCommand, sorted.length, page, state.pageSize, state, i18n)}`;
}

function renderPagination(
    command: string,
    total: number,
    page: number,
    pageSize: number,
    state: WorkspaceGraphHealthViewState,
    i18n: ReviewI18n
): string {
    if (total === 0) {
        return '';
    }
    const totalPages = Math.max(1, Math.ceil(total / pageSize));
    const safePage = Math.min(Math.max(page, 1), totalPages);
    const start = (safePage - 1) * pageSize + 1;
    const end = Math.min(total, safePage * pageSize);
    return `<div class="pagination-bar">
  <span>${escapeHtml(i18n.t('workspaceGraphHealthView.pagination.showing', { start, end, total }))}</span>
  <div class="row-actions">
    <button type="button" data-command="${escapeAttr(command)}" data-page="${safePage - 1}" ${safePage <= 1 ? 'disabled' : ''}>${escapeHtml(i18n.t('workspaceGraphHealthView.pagination.previous'))}</button>
    <span>${escapeHtml(i18n.t('workspaceGraphHealthView.pagination.page', { page: safePage, totalPages }))}</span>
    <button type="button" data-command="${escapeAttr(command)}" data-page="${safePage + 1}" ${safePage >= totalPages ? 'disabled' : ''}>${escapeHtml(i18n.t('workspaceGraphHealthView.pagination.next'))}</button>
    <button type="button" data-command="setWorkspaceGraphPageSize" data-page-size="10" ${state.pageSize === 10 ? 'disabled' : ''}>10/page</button>
    <button type="button" data-command="setWorkspaceGraphPageSize" data-page-size="25" ${state.pageSize === 25 ? 'disabled' : ''}>25/page</button>
  </div>
</div>`;
}

function compareFamilies(left: ReviewFamilyCount, right: ReviewFamilyCount, sortBy: FamilySortKey): number {
    if (sortBy === 'family') {
        return left.family.localeCompare(right.family);
    }
    return (left.count ?? -1) - (right.count ?? -1);
}

function compareDiagnostics(left: ReviewGraphDiagnosticRow, right: ReviewGraphDiagnosticRow, sortBy: DiagnosticSortKey): number {
    return sortBy === 'identity'
        ? left.identity.localeCompare(right.identity)
        : left.reason.localeCompare(right.reason);
}

function paginate<T>(items: T[], page: number, pageSize: number): { items: T[] } {
    const totalPages = Math.max(1, Math.ceil(items.length / pageSize));
    const safePage = Math.min(Math.max(page, 1), totalPages);
    const start = (safePage - 1) * pageSize;
    return { items: items.slice(start, start + pageSize) };
}

function sortIndicator(active: boolean, direction: SortDirection): string {
    if (!active) {
        return '↕';
    }
    return direction === 'asc' ? '↑' : '↓';
}

function summaryCard(label: string, value: string): string {
    return `<div class="card"><small>${escapeHtml(label)}</small><strong>${escapeHtml(value)}</strong></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-workspace-graph-health"><div class="banner error" role="alert"><strong>${escapeHtml(i18n.t('workspaceGraphHealthView.errorTitle'))}</strong><p>${escapeHtml(message)}</p></div></section>`;
}
