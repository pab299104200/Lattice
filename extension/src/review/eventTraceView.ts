import { escapeAttr, escapeHtml } from './components/html';
import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewEventTraceEntry, ReviewEventTracePage } from './rpcPayloads';

type EventTraceSortKey = 'timestamp' | 'kind' | 'actor';
type SortDirection = 'asc' | 'desc';

export interface EventTraceViewState {
    branch?: string;
    inlineError?: string;
    kinds: string[];
    lastPage?: ReviewEventTracePage;
    memoryId?: string;
    page: number;
    pageSize: number;
    selectedActors: string[];
    sessionId?: string;
    sortBy: EventTraceSortKey;
    sortDirection: SortDirection;
    taskId?: string;
    until?: string;
    since?: string;
    workspaceId?: string;
}

export interface EventTraceHost {
    readonly state: EventTraceViewState;
    rememberPage(page: ReviewEventTracePage): void;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountEventTraceView(
    host: EventTraceHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n,
    initialFilter: { sessionId?: string; taskId?: string; memoryId?: string; workspaceId?: string }
): Promise<void> {
    const resolvedScope = resolveScope(host.state, initialFilter);
    if (!resolvedScope.kind || !resolvedScope.value) {
        host.setContent(renderPrompt(i18n));
        return;
    }
    try {
        const result = await bridge.getEventTrace({
            sessionId: resolvedScope.kind === 'session' ? resolvedScope.value : undefined,
            taskId: resolvedScope.kind === 'task' ? resolvedScope.value : undefined,
            workspaceId: resolvedScope.kind === 'workspace' ? resolvedScope.value : undefined,
            kinds: host.state.kinds,
            since: host.state.since,
            until: host.state.until,
            limit: 200,
            renderMode: 'full',
        });
        if (!result.ok) {
            host.reportError?.(result.error.message);
            host.setContent(renderError(i18n, result.error.message));
            return;
        }
        host.rememberPage(result.value);
        host.setContent(renderView(result.value, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderView(page: ReviewEventTracePage, state: EventTraceViewState, i18n: ReviewI18n): string {
    const filtered = applyFilters(page.events, state);
    const sorted = sortEvents(filtered, state);
    const paged = paginate(sorted, state.page, state.pageSize);
    const rows = paged.items.map((event) => renderRow(event, i18n)).join('');
    return `<section class="route-stack" data-testid="review-event-trace">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('eventTraceView.title'))}</h3>
      <p>${escapeHtml(i18n.t('eventTraceView.subtitle', { scope: describeScope(page, i18n) }))}</p>
    </div>
    <div class="toolbar-actions">
      ${filterButton('pickEventTraceKinds', i18n.t('eventTraceView.filters.kindButton', { count: state.kinds.length || page.events.length }))}
      ${filterButton('pickEventTraceActors', i18n.t('eventTraceView.filters.actorButton', { count: state.selectedActors.length || uniqueActors(page.events).length }))}
      ${filterButton('pickEventTraceSession', i18n.t('eventTraceView.filters.sessionButton'))}
      ${filterButton('pickEventTraceTask', i18n.t('eventTraceView.filters.taskButton'))}
      ${filterButton('pickEventTraceWorkspace', i18n.t('eventTraceView.filters.workspaceButton'))}
      ${filterButton('pickEventTraceBranch', i18n.t('eventTraceView.filters.branchButton'))}
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  <form class="filter-bar" data-command="applyEventTraceWindow">
    <label class="field-stack">
      <span>${escapeHtml(i18n.t('eventTraceView.filters.since'))}</span>
      <input type="text" name="since" value="${escapeAttr(state.since ?? '')}" placeholder="${escapeAttr(i18n.t('eventTraceView.filters.isoPlaceholder'))}" />
    </label>
    <label class="field-stack">
      <span>${escapeHtml(i18n.t('eventTraceView.filters.until'))}</span>
      <input type="text" name="until" value="${escapeAttr(state.until ?? '')}" placeholder="${escapeAttr(i18n.t('eventTraceView.filters.isoPlaceholder'))}" />
    </label>
    <button type="submit">${escapeHtml(i18n.t('eventTraceView.filters.applyWindow'))}</button>
    <button type="button" data-command="clearEventTraceWindow">${escapeHtml(i18n.t('eventTraceView.filters.clearWindow'))}</button>
  </form>
  <div class="filter-summary">
    <span class="filter-pill">${escapeHtml(i18n.t('eventTraceView.filters.activeKinds', { value: summarizeKinds(state, page, i18n) }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('eventTraceView.filters.activeActors', { value: summarizeActors(state, i18n) }))}</span>
    <span class="filter-pill">${escapeHtml(i18n.t('eventTraceView.filters.activeBranch', { value: state.branch ?? i18n.t('eventTraceView.filters.all') }))}</span>
    ${state.memoryId ? `<span class="filter-pill">${escapeHtml(i18n.t('eventTraceView.filters.activeMemory', { value: state.memoryId }))}</span>` : ''}
  </div>
  ${rows ? renderTable(rows, state, i18n) : renderEmpty(filtered.length === 0 && page.events.length > 0, i18n)}
  ${renderPagination(sorted.length, state, i18n)}
</section>`;
}

function renderTable(rows: string, state: EventTraceViewState, i18n: ReviewI18n): string {
    return `<div class="table-shell"><table class="data-table">
  <thead><tr>
    ${sortableHeader('timestamp', state, i18n, 'eventTraceView.columns.timestamp')}
    ${sortableHeader('kind', state, i18n, 'eventTraceView.columns.kind')}
    ${sortableHeader('actor', state, i18n, 'eventTraceView.columns.actor')}
    <th>${escapeHtml(i18n.t('eventTraceView.columns.task'))}</th>
    <th>${escapeHtml(i18n.t('eventTraceView.columns.session'))}</th>
    <th>${escapeHtml(i18n.t('eventTraceView.columns.workspace'))}</th>
    <th>${escapeHtml(i18n.t('eventTraceView.columns.references'))}</th>
    <th>${escapeHtml(i18n.t('eventTraceView.columns.summary'))}</th>
    <th>${escapeHtml(i18n.t('eventTraceView.columns.payload'))}</th>
  </tr></thead>
  <tbody>${rows}</tbody>
</table></div>`;
}

function renderRow(event: ReviewEventTraceEntry, i18n: ReviewI18n): string {
    const references = event.references.length
        ? event.references.map((reference) => renderReference(reference, i18n)).join('')
        : `<span class="muted-text">${escapeHtml(i18n.t('eventTraceView.notAvailable'))}</span>`;
    const payloadDialog = renderPayloadDialog(event, i18n);
    const retrievalButton = event.kind === 'memory_retrieved'
        ? `<button data-command="openRetrievalExplanationFromEvent" data-event-id="${escapeAttr(event.eventId)}">${escapeHtml(i18n.t('eventTraceView.openRetrievalExplanation'))}</button>`
        : '';
    return `<tr>
  <td>${escapeHtml(formatTimestamp(event.timestamp))}</td>
  <td>${renderEventKindBadge(event.kind, i18n)}</td>
  <td>${escapeHtml(formatActor(event.actor, i18n))}</td>
  <td><div class="cell-stack"><span>${escapeHtml(event.taskId ?? i18n.t('eventTraceView.notAvailable'))}</span><small>${escapeHtml(event.branch || i18n.t('eventTraceView.notAvailable'))}</small></div></td>
  <td>${escapeHtml(event.sessionId)}</td>
  <td>${escapeHtml(event.workspaceId)}</td>
  <td><div class="reference-stack">${references}</div></td>
  <td><div class="cell-stack"><span>${escapeHtml(event.summary)}</span><small>${escapeHtml(event.eventId)}</small></div></td>
  <td>
    <div class="row-actions">
      <button data-local-command="open-dialog" data-dialog-id="payload-${escapeAttr(event.eventId)}">${escapeHtml(i18n.t('eventTraceView.openPayload'))}</button>
      ${retrievalButton}
    </div>
    ${payloadDialog}
  </td>
</tr>`;
}

function renderPayloadDialog(event: ReviewEventTraceEntry, i18n: ReviewI18n): string {
    const title = i18n.t('eventTraceView.payloadDialogTitle', { id: event.eventId });
    const payloadBody = event.payload === undefined
        ? `<div class="placeholder compact"><h3>${escapeHtml(i18n.t('eventTraceView.noPayload'))}</h3></div>`
        : `<div class="json-tree">${renderJson(event.payload, true)}</div>`;
    return `<dialog id="payload-${escapeAttr(event.eventId)}" class="payload-dialog">
  <article class="dialog-shell">
    <header class="route-toolbar compact">
      <div>
        <h3>${escapeHtml(title)}</h3>
        <p>${escapeHtml(event.kind)}</p>
      </div>
      <div class="toolbar-actions">
        <button data-command="copyEventPayload" data-event-id="${escapeAttr(event.eventId)}">${escapeHtml(i18n.t('eventTraceView.copyPayload'))}</button>
        <button data-command="openEventPayloadInEditor" data-event-id="${escapeAttr(event.eventId)}">${escapeHtml(i18n.t('eventTraceView.openInEditor'))}</button>
        <button data-local-command="close-dialog">${escapeHtml(i18n.t('eventTraceView.closeDialog'))}</button>
      </div>
    </header>
    ${payloadBody}
  </article>
</dialog>`;
}

function renderReference(reference: string, i18n: ReviewI18n): string {
    const parsed = parseReference(reference);
    if (!parsed) {
        return `<span class="reference-chip">${escapeHtml(reference)}</span>`;
    }
    const command = parsed.kind === 'memory'
        ? 'openEvidenceInspectorFromReference'
        : parsed.kind === 'file' || parsed.kind === 'symbol'
            ? 'openEventTraceReference'
            : '';
    if (!command) {
        return `<span class="reference-chip">${escapeHtml(parsed.label)}</span>`;
    }
    const label = parsed.kind === 'memory'
        ? i18n.t('eventTraceView.openMemoryReference', { value: parsed.label })
        : i18n.t('eventTraceView.openFileReference', { value: parsed.label });
    return `<button class="reference-chip" data-command="${escapeAttr(command)}" data-value="${escapeAttr(reference)}">${escapeHtml(label)}</button>`;
}

function renderPagination(total: number, state: EventTraceViewState, i18n: ReviewI18n): string {
    if (total === 0) {
        return '';
    }
    const totalPages = Math.max(1, Math.ceil(total / state.pageSize));
    const start = (state.page - 1) * state.pageSize + 1;
    const end = Math.min(total, state.page * state.pageSize);
    return `<div class="pagination-bar">
  <span>${escapeHtml(i18n.t('eventTraceView.pagination.showing', { start, end, total }))}</span>
  <div class="row-actions">
    <button data-command="pageEventTrace" data-page="${state.page - 1}" ${state.page <= 1 ? 'disabled' : ''}>${escapeHtml(i18n.t('eventTraceView.pagination.previous'))}</button>
    <span>${escapeHtml(i18n.t('eventTraceView.pagination.page', { page: state.page, totalPages }))}</span>
    <button data-command="pageEventTrace" data-page="${state.page + 1}" ${state.page >= totalPages ? 'disabled' : ''}>${escapeHtml(i18n.t('eventTraceView.pagination.next'))}</button>
    ${pageSizeButton(25, state.pageSize)}
    ${pageSizeButton(50, state.pageSize)}
    ${pageSizeButton(100, state.pageSize)}
  </div>
</div>`;
}

function renderEmpty(filtered: boolean, i18n: ReviewI18n): string {
    const message = filtered ? i18n.t('eventTraceView.emptyFiltered') : i18n.t('eventTraceView.empty');
    return `<div class="placeholder"><h3>${escapeHtml(message)}</h3></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-event-trace"><div class="banner error" role="alert"><strong>${escapeHtml(i18n.t('eventTraceView.errorTitle'))}</strong><p>${escapeHtml(message)}</p></div></section>`;
}

function renderPrompt(i18n: ReviewI18n): string {
    return `<div class="placeholder" data-testid="review-event-trace"><h3>${escapeHtml(i18n.t('eventTraceView.selectPrompt'))}</h3></div>`;
}

function renderJson(value: unknown, open = false): string {
    if (Array.isArray(value)) {
        return `<details ${open ? 'open' : ''}><summary>[${value.length}]</summary>${value.map((item) => `<div class="json-node">${renderJson(item)}</div>`).join('')}</details>`;
    }
    if (typeof value === 'object' && value !== null) {
        const entries = Object.entries(value as Record<string, unknown>);
        return `<details ${open ? 'open' : ''}><summary>{${entries.length}}</summary>${entries.map(([key, item]) => `<div class="json-node"><span class="json-key">${escapeHtml(key)}</span>: ${renderJson(item)}</div>`).join('')}</details>`;
    }
    if (typeof value === 'string') {
        return `<span class="json-string">"${escapeHtml(value)}"</span>`;
    }
    if (typeof value === 'number') {
        return `<span class="json-number">${escapeHtml(String(value))}</span>`;
    }
    if (typeof value === 'boolean') {
        return `<span class="json-boolean">${escapeHtml(String(value))}</span>`;
    }
    return `<span class="json-null">null</span>`;
}

function applyFilters(events: ReviewEventTraceEntry[], state: EventTraceViewState): ReviewEventTraceEntry[] {
    return events.filter((event) => {
        if (state.selectedActors.length && !state.selectedActors.includes(event.actor)) {
            return false;
        }
        if (state.sessionId && event.sessionId !== state.sessionId) {
            return false;
        }
        if (state.taskId && event.taskId !== state.taskId) {
            return false;
        }
        if (state.workspaceId && event.workspaceId !== state.workspaceId) {
            return false;
        }
        if (state.branch && event.branch !== state.branch) {
            return false;
        }
        if (state.memoryId && !event.references.some((reference) => reference.includes(state.memoryId ?? ''))) {
            return false;
        }
        return true;
    });
}

function sortEvents(events: ReviewEventTraceEntry[], state: EventTraceViewState): ReviewEventTraceEntry[] {
    const sorted = [...events].sort((left, right) => compareEvents(left, right, state.sortBy));
    return state.sortDirection === 'desc' ? sorted.reverse() : sorted;
}

function compareEvents(left: ReviewEventTraceEntry, right: ReviewEventTraceEntry, sortBy: EventTraceSortKey): number {
    switch (sortBy) {
        case 'kind':
            return left.kind.localeCompare(right.kind);
        case 'actor':
            return left.actor.localeCompare(right.actor);
        case 'timestamp':
        default:
            return left.timestamp.localeCompare(right.timestamp);
    }
}

function paginate<T>(items: T[], page: number, pageSize: number): { items: T[]; totalPages: number } {
    const totalPages = Math.max(1, Math.ceil(items.length / pageSize));
    const safePage = Math.min(Math.max(page, 1), totalPages);
    const start = (safePage - 1) * pageSize;
    return { items: items.slice(start, start + pageSize), totalPages };
}

function pageSizeButton(size: number, active: number): string {
    return `<button data-command="setEventTracePageSize" data-page-size="${size}" ${size === active ? 'disabled' : ''}>${size}/page</button>`;
}

function sortableHeader(sortBy: EventTraceSortKey, state: EventTraceViewState, i18n: ReviewI18n, key: string): string {
    const direction = state.sortBy === sortBy ? (state.sortDirection === 'asc' ? '↑' : '↓') : '↕';
    return `<th><button class="table-sort" data-command="sortEventTrace" data-sort-by="${escapeAttr(sortBy)}">${escapeHtml(i18n.t(key))} ${direction}</button></th>`;
}

function filterButton(command: string, label: string): string {
    return `<button data-command="${escapeAttr(command)}">${escapeHtml(label)}</button>`;
}

function summarizeKinds(state: EventTraceViewState, page: ReviewEventTracePage, i18n: ReviewI18n): string {
    const values = state.kinds.length ? state.kinds : uniqueKinds(page.events);
    if (!state.kinds.length) {
        return i18n.t('eventTraceView.filters.all');
    }
    return values.map((kind) => labelForKind(kind, i18n)).join(', ');
}

function summarizeActors(state: EventTraceViewState, i18n: ReviewI18n): string {
    if (!state.selectedActors.length) {
        return i18n.t('eventTraceView.filters.all');
    }
    return state.selectedActors.map((actor) => formatActor(actor, i18n)).join(', ');
}

function uniqueKinds(events: ReviewEventTraceEntry[]): string[] {
    return [...new Set(events.map((event) => event.kind))].sort((left, right) => left.localeCompare(right));
}

function uniqueActors(events: ReviewEventTraceEntry[]): string[] {
    return [...new Set(events.map((event) => event.actor).filter(Boolean))].sort((left, right) => left.localeCompare(right));
}

function parseReference(reference: string): { kind: string; label: string } | undefined {
    try {
        const parsed = JSON.parse(reference) as Record<string, unknown>;
        const [kind, rawValue] = Object.entries(parsed)[0] ?? [];
        if (!kind || typeof rawValue !== 'object' || rawValue === null || Array.isArray(rawValue)) {
            return undefined;
        }
        const value = rawValue as Record<string, unknown>;
        if (kind === 'MemoryRef') {
            return { kind: 'memory', label: String(value.ulid ?? value.id ?? reference) };
        }
        if (kind === 'FileRef') {
            return { kind: 'file', label: String(value.repo_relative_path ?? reference) };
        }
        if (kind === 'SymbolRef') {
            return { kind: 'symbol', label: String(value.qualified_name ?? reference) };
        }
        return { kind, label: reference };
    } catch {
        return undefined;
    }
}

function labelForKind(kind: string, i18n: ReviewI18n): string {
    return i18n.has(`eventTraceView.kind.${kind}`) ? i18n.t(`eventTraceView.kind.${kind}`) : kind;
}

function renderEventKindBadge(kind: string, i18n: ReviewI18n): string {
    const tone = kind.includes('failed') || kind.includes('invalidated')
        ? 'error'
        : kind.includes('retrieved') || kind.includes('expanded') || kind.includes('bundle')
            ? 'info'
            : kind.includes('diagnostic') || kind.includes('correction')
                ? 'warning'
                : kind.includes('created') || kind.includes('updated') || kind.includes('succeeded')
                    ? 'success'
                    : 'neutral';
    return `<span class="status-badge status-badge--${tone}">${escapeHtml(labelForKind(kind, i18n))}</span>`;
}

function formatActor(actor: string, i18n: ReviewI18n): string {
    const base = actor.split(':')[0];
    const detail = actor.includes(':') ? actor.slice(actor.indexOf(':') + 1) : '';
    const label = i18n.has(`eventTraceView.actor.${base}`) ? i18n.t(`eventTraceView.actor.${base}`) : actor;
    return detail ? `${label} (${detail})` : label;
}

function formatTimestamp(timestamp: string): string {
    const date = new Date(timestamp);
    return Number.isNaN(date.getTime()) ? timestamp : date.toLocaleString();
}

function describeScope(page: ReviewEventTracePage, i18n: ReviewI18n): string {
    const scope = i18n.has(`eventTraceView.scope.${page.scope.kind}`)
        ? i18n.t(`eventTraceView.scope.${page.scope.kind}`)
        : page.scope.kind;
    return `${scope}: ${page.scope.value}`;
}

function resolveScope(
    state: EventTraceViewState,
    initialFilter: { sessionId?: string; taskId?: string; memoryId?: string; workspaceId?: string }
): { kind?: 'task' | 'session' | 'workspace'; value?: string } {
    const taskId = state.taskId ?? initialFilter.taskId;
    if (taskId) {
        return { kind: 'task', value: taskId };
    }
    const sessionId = state.sessionId ?? initialFilter.sessionId;
    if (sessionId) {
        return { kind: 'session', value: sessionId };
    }
    const workspaceId = state.workspaceId ?? initialFilter.workspaceId;
    if (workspaceId) {
        return { kind: 'workspace', value: workspaceId };
    }
    return {};
}
