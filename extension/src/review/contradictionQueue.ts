/// <reference lib="dom" />

declare function renderStatusBadge(status: string, i18n: { t: (key: string, params?: Record<string, string | number>) => string }): string;
declare function openProposalDialog(
    options: {
        proposal: Record<string, unknown>;
        onApply: (reason?: string) => Promise<unknown>;
        onReject: (reason: string) => Promise<unknown>;
    },
    i18n: { t: (key: string, params?: Record<string, string | number>) => string }
): HTMLDialogElement;
declare function reportReviewViewError(route: string, message: string): void;

export function mountContradictionQueue(
    host: HTMLElement,
    bridge: {
        listContradictions: () => Promise<Record<string, unknown>>;
        getMemoryEvidence: (memoryId: string) => Promise<Record<string, unknown>>;
        applyContradictionResolution: (args: Record<string, unknown>) => Promise<unknown>;
        rejectContradictionResolution: (args: Record<string, unknown>) => Promise<unknown>;
    },
    i18n: { t: (key: string, params?: Record<string, string | number>) => string }
): void {
    let rows: ContradictionRow[] = [];
    let sortKey: ContradictionSortKey = 'createdAt';
    let sortDirection: 'asc' | 'desc' = 'desc';

    void load();

    async function load(): Promise<void> {
        renderLoading(host);
        try {
            const payload = await bridge.listContradictions();
            rows = await hydrateRows(payload);
            renderTable();
        } catch (error) {
            const message = errorMessage(error);
            renderError(host, message);
            reportReviewViewError('contradictionQueue', message);
        }
    }

    async function hydrateRows(payload: Record<string, unknown>): Promise<ContradictionRow[]> {
        const conflicts = Array.isArray(payload.conflicts) ? payload.conflicts : [];
        const items = conflicts
            .map((entry) => asRecord(entry))
            .filter((entry): entry is Record<string, unknown> => entry !== undefined);
        const ids = new Set<string>();
        for (const entry of items) {
            ids.add(stringValue(entry.source));
            ids.add(stringValue(entry.target));
        }
        const details = new Map<string, Record<string, unknown>>();
        await Promise.all(
            [...ids]
                .filter((id) => id.length > 0)
                .map(async (id) => {
                    try {
                        details.set(id, await bridge.getMemoryEvidence(id));
                    } catch {
                        details.set(id, {});
                    }
                }),
        );
        return items.map((entry) => {
            const sourceId = stringValue(entry.source);
            const targetId = stringValue(entry.target);
            const source = details.get(sourceId) ?? {};
            const target = details.get(targetId) ?? {};
            return {
                sourceId,
                targetId,
                memoryA: memoryLabel(sourceId, source),
                memoryB: memoryLabel(targetId, target),
                linkType: stringValue(entry.linkType),
                detectedBy: stringValue(entry.createdBy),
                createdAt: numberValue(entry.createdAt),
                status: stringValue(entry.linkVerificationStatus || 'pending'),
                reason: stringValue(entry.reason),
                sourceMemory: source,
                targetMemory: target,
            };
        });
    }

    function renderTable(): void {
        host.innerHTML = '';
        const container = document.createElement('section');
        container.className = 'queue-surface';
        container.setAttribute('data-testid', 'review-contradiction-queue');
        container.appendChild(renderInfoBanner());
        if (rows.length === 0) {
            container.appendChild(renderEmptyState(i18n.t('contradictionQueue.empty')));
            host.appendChild(container);
            return;
        }
        const table = document.createElement('table');
        table.className = 'review-table';
        table.append(createHead(), createBody());
        container.appendChild(table);
        host.appendChild(container);
    }

    function renderInfoBanner(): HTMLElement {
        const note = document.createElement('div');
        note.className = 'banner info';
        note.textContent = i18n.t('contradictionQueue.description');
        return note;
    }

    function createHead(): HTMLElement {
        const thead = document.createElement('thead');
        const row = document.createElement('tr');
        row.append(
            plainHeader(i18n.t('contradictionQueue.columns.memoryA')),
            plainHeader(i18n.t('contradictionQueue.columns.memoryB')),
            plainHeader(i18n.t('contradictionQueue.columns.linkType')),
            plainHeader(i18n.t('contradictionQueue.columns.detectedBy')),
            sortableHeader('createdAt', i18n.t('contradictionQueue.columns.createdAt')),
            sortableHeader('status', i18n.t('contradictionQueue.columns.status')),
            plainHeader(i18n.t('contradictionQueue.columns.actions')),
        );
        thead.appendChild(row);
        return thead;
    }

    function createBody(): HTMLElement {
        const tbody = document.createElement('tbody');
        for (const entry of sortedRows()) {
            const row = document.createElement('tr');
            row.append(
                textCell(entry.memoryA),
                textCell(entry.memoryB),
                textCell(entry.linkType),
                textCell(entry.detectedBy),
                textCell(formatTimestamp(entry.createdAt)),
                badgeCell(entry.status),
                actionCell(entry),
            );
            tbody.appendChild(row);
        }
        return tbody;
    }

    function sortedRows(): ContradictionRow[] {
        const factor = sortDirection === 'asc' ? 1 : -1;
        return [...rows].sort((left, right) => {
            const leftValue = sortableValue(left, sortKey);
            const rightValue = sortableValue(right, sortKey);
            if (leftValue < rightValue) {
                return -1 * factor;
            }
            if (leftValue > rightValue) {
                return 1 * factor;
            }
            return 0;
        });
    }

    function sortableHeader(key: ContradictionSortKey, label: string): HTMLElement {
        const cell = document.createElement('th');
        const button = document.createElement('button');
        button.type = 'button';
        button.className = 'table-sort';
        button.textContent = sortLabel(key, label);
        button.addEventListener('click', () => {
            if (sortKey === key) {
                sortDirection = sortDirection === 'asc' ? 'desc' : 'asc';
            } else {
                sortKey = key;
                sortDirection = key === 'status' ? 'asc' : 'desc';
            }
            renderTable();
        });
        cell.appendChild(button);
        return cell;
    }

    function sortLabel(key: ContradictionSortKey, label: string): string {
        if (sortKey !== key) {
            return label;
        }
        return `${label} ${sortDirection === 'asc' ? '↑' : '↓'}`;
    }

    function badgeCell(status: string): HTMLElement {
        const cell = document.createElement('td');
        cell.innerHTML = renderStatusBadge(status, i18n);
        return cell;
    }

    function actionCell(entry: ContradictionRow): HTMLElement {
        const cell = document.createElement('td');
        const actions = document.createElement('div');
        actions.className = 'table-actions';
        actions.append(
            queueAction(i18n.t('contradictionQueue.actions.review'), () => openDialog(entry, 'review')),
            queueAction(i18n.t('contradictionQueue.actions.accept'), () => openDialog(entry, 'accept')),
            queueAction(i18n.t('contradictionQueue.actions.reject'), () => openDialog(entry, 'reject')),
        );
        cell.appendChild(actions);
        return cell;
    }

    function openDialog(entry: ContradictionRow, mode: 'review' | 'accept' | 'reject'): void {
        openProposalDialog(
            {
                proposal: {
                    title: i18n.t('proposalDialog.contradictionTitle'),
                    summary: entry.reason,
                    proposalKind: contradictionProposalKind(entry),
                    status: i18n.t(`reviewStatus.${entry.status}`),
                    proposedClass: stringValue(entry.targetMemory.memoryClass) || 'Memory',
                    currentScope: stringValue(entry.targetMemory.scope) || '—',
                    targetScope: stringValue(entry.targetMemory.scope) || '—',
                    createdAt: entry.createdAt,
                    evidenceCount: 2,
                    expectedEffect: contradictionEffect(entry, i18n),
                    priorState: entry.targetMemory,
                    proposedState: predictedState(entry),
                    evidence: {
                        source_memory_ids: [entry.sourceId, entry.targetId],
                        link_type: entry.linkType,
                        detected_by: entry.detectedBy,
                        reason: entry.reason,
                    },
                    provenance: {
                        model: entry.detectedBy,
                    },
                    initialMode: mode,
                },
                onApply: async (reason?: string) => {
                    await bridge.applyContradictionResolution({
                        sourceMemoryId: entry.sourceId,
                        targetMemoryId: entry.targetId,
                        linkType: entry.linkType,
                        reason,
                        detectedBy: entry.detectedBy,
                    });
                    await load();
                },
                onReject: async (reason: string) => {
                    await bridge.rejectContradictionResolution({
                        sourceMemoryId: entry.sourceId,
                        targetMemoryId: entry.targetId,
                        linkType: entry.linkType,
                        reason,
                        detectedBy: entry.detectedBy,
                    });
                    await load();
                },
            },
            i18n,
        );
    }
}

export function serializeContradictionQueue(): string {
    return [
        mountContradictionQueue,
        contradictionProposalKind,
        contradictionEffect,
        predictedState,
        sortableValue,
        plainHeader,
        textCell,
        queueAction,
        memoryLabel,
        renderLoading,
        renderError,
        renderEmptyState,
        asRecord,
        stringValue,
        numberValue,
        formatTimestamp,
        errorMessage,
        escapeHtml,
    ].map((entry) => entry.toString()).join('\n');
}

type ContradictionSortKey = 'createdAt' | 'status';

interface ContradictionRow {
    sourceId: string;
    targetId: string;
    memoryA: string;
    memoryB: string;
    linkType: string;
    detectedBy: string;
    createdAt: number;
    status: string;
    reason: string;
    sourceMemory: Record<string, unknown>;
    targetMemory: Record<string, unknown>;
}

function contradictionProposalKind(entry: ContradictionRow): string {
    return entry.linkType === 'supersedes' ? 'supersede' : 'mark_invalidated';
}

function contradictionEffect(
    entry: ContradictionRow,
    i18n: { t: (key: string, params?: Record<string, string | number>) => string }
): string {
    if (entry.linkType === 'supersedes') {
        return i18n.t('proposalDialog.supersedeEffect', { value: entry.targetId });
    }
    return i18n.t('proposalDialog.invalidateEffect', { value: entry.targetId });
}

function predictedState(entry: ContradictionRow): Record<string, unknown> {
    const next = { ...entry.targetMemory };
    if (entry.linkType === 'supersedes') {
        next.verificationStatus = 'superseded';
        next.supersededBy = entry.sourceId;
        return next;
    }
    next.verificationStatus = 'invalidated';
    next.staleReason = entry.reason;
    return next;
}

function sortableValue(row: ContradictionRow, key: ContradictionSortKey): number | string {
    switch (key) {
        case 'createdAt':
            return row.createdAt;
        case 'status':
            return row.status.toLowerCase();
    }
}

function plainHeader(label: string): HTMLElement {
    const cell = document.createElement('th');
    cell.textContent = label;
    return cell;
}

function textCell(value: string): HTMLElement {
    const cell = document.createElement('td');
    cell.textContent = value;
    return cell;
}

function queueAction(label: string, onClick: () => void): HTMLElement {
    const button = document.createElement('button');
    button.type = 'button';
    button.className = 'table-action';
    button.textContent = label;
    button.addEventListener('click', onClick);
    return button;
}

function memoryLabel(memoryId: string, memory: Record<string, unknown>): string {
    const content = stringValue(memory.content);
    if (content) {
        return content.length > 72 ? `${content.slice(0, 69)}...` : content;
    }
    return memoryId;
}

function renderLoading(host: HTMLElement): void {
    host.innerHTML = '<div class="queue-surface"><div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div></div>';
}

function renderError(host: HTMLElement, message: string): void {
    host.innerHTML = `<div class="queue-surface"><div class="banner error" role="alert">${escapeHtml(message)}</div></div>`;
}

function renderEmptyState(message: string): HTMLElement {
    const box = document.createElement('div');
    box.className = 'placeholder';
    const title = document.createElement('h3');
    title.textContent = message;
    box.appendChild(title);
    return box;
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
        ? value as Record<string, unknown>
        : undefined;
}

function stringValue(value: unknown): string {
    return typeof value === 'string' ? value : '';
}

function numberValue(value: unknown): number {
    return typeof value === 'number' ? value : 0;
}

function formatTimestamp(value: number): string {
    if (!value) {
        return '—';
    }
    const millis = value > 1_000_000_000_000 ? Math.floor(value / 1000) : value * 1000;
    return new Date(millis).toLocaleString();
}

function errorMessage(error: unknown): string {
    return error instanceof Error && error.message ? error.message : 'Request failed.';
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
