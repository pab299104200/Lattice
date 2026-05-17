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

export function mountPromotionQueue(
    host: HTMLElement,
    bridge: {
        listPromotionProposals: () => Promise<Record<string, unknown>>;
        applyPromotion: (proposalId: string, reason?: string) => Promise<unknown>;
        rejectPromotion: (proposalId: string, reason: string) => Promise<unknown>;
    },
    i18n: { t: (key: string, params?: Record<string, string | number>) => string }
): void {
    let rows: PromotionRow[] = [];
    let sortKey: PromotionSortKey = 'createdAt';
    let sortDirection: 'asc' | 'desc' = 'desc';

    void load();

    async function load(): Promise<void> {
        renderLoading(host);
        try {
            const report = await bridge.listPromotionProposals();
            rows = readProposalRows(report);
            renderTable();
        } catch (error) {
            const message = errorMessage(error);
            renderError(host, message);
            reportReviewViewError('promotionQueue', message);
        }
    }

    function renderTable(): void {
        host.innerHTML = '';
        const container = document.createElement('section');
        container.className = 'queue-surface';
        container.setAttribute('data-testid', 'review-promotion-queue');
        container.appendChild(renderNotes());
        if (rows.length === 0) {
            container.appendChild(renderEmptyState(i18n.t('promotionQueue.empty')));
            host.appendChild(container);
            return;
        }
        const table = document.createElement('table');
        table.className = 'review-table';
        table.append(createHead(), createBody());
        container.appendChild(table);
        host.appendChild(container);
    }

    function renderNotes(): HTMLElement {
        const note = document.createElement('div');
        note.className = 'banner info';
        note.textContent = i18n.t('promotionQueue.description');
        return note;
    }

    function createHead(): HTMLElement {
        const thead = document.createElement('thead');
        const row = document.createElement('tr');
        row.append(
            sortableHeader('proposedClass', i18n.t('promotionQueue.columns.proposedClass')),
            plainHeader(i18n.t('promotionQueue.columns.currentScope')),
            plainHeader(i18n.t('promotionQueue.columns.targetScope')),
            sortableHeader('confidence', i18n.t('promotionQueue.columns.confidence')),
            sortableHeader('evidenceCount', i18n.t('promotionQueue.columns.evidenceCount')),
            sortableHeader('createdAt', i18n.t('promotionQueue.columns.createdAt')),
            sortableHeader('status', i18n.t('promotionQueue.columns.status')),
            plainHeader(i18n.t('promotionQueue.columns.actions')),
        );
        thead.appendChild(row);
        return thead;
    }

    function createBody(): HTMLElement {
        const tbody = document.createElement('tbody');
        for (const entry of sortedRows()) {
            const row = document.createElement('tr');
            row.append(
                textCell(entry.proposedClass),
                textCell(entry.currentScope),
                textCell(entry.targetScope),
                textCell(formatPercent(entry.confidence)),
                textCell(String(entry.evidenceCount)),
                textCell(formatTimestamp(entry.createdAt)),
                badgeCell(entry.status),
                actionCell(entry),
            );
            tbody.appendChild(row);
        }
        return tbody;
    }

    function sortedRows(): PromotionRow[] {
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

    function sortableHeader(key: PromotionSortKey, label: string): HTMLElement {
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
                sortDirection = key === 'proposedClass' || key === 'status' ? 'asc' : 'desc';
            }
            renderTable();
        });
        cell.appendChild(button);
        return cell;
    }

    function sortLabel(key: PromotionSortKey, label: string): string {
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

    function actionCell(entry: PromotionRow): HTMLElement {
        const cell = document.createElement('td');
        const actions = document.createElement('div');
        actions.className = 'table-actions';
        actions.append(
            queueAction(i18n.t('promotionQueue.actions.review'), () => openDialog(entry, 'review')),
            queueAction(i18n.t('promotionQueue.actions.accept'), () => openDialog(entry, 'accept')),
            queueAction(i18n.t('promotionQueue.actions.reject'), () => openDialog(entry, 'reject')),
        );
        cell.appendChild(actions);
        return cell;
    }

    function openDialog(entry: PromotionRow, mode: 'review' | 'accept' | 'reject'): void {
        openProposalDialog(
            {
                proposal: {
                    title: i18n.t('proposalDialog.promotionTitle', { value: entry.proposedClass }),
                    summary: entry.summary,
                    proposalKind: entry.proposalKind,
                    status: i18n.t(`reviewStatus.${entry.status}`),
                    proposedClass: entry.proposedClass,
                    currentScope: entry.currentScope,
                    targetScope: entry.targetScope,
                    confidence: entry.confidence,
                    evidenceCount: entry.evidenceCount,
                    createdAt: entry.createdAt,
                    expectedEffect: i18n.t('proposalDialog.promotionEffect', {
                        value: `${entry.proposedClass} ${entry.targetScope}`,
                    }),
                    priorState: entry.priorState,
                    proposedState: entry.proposedState,
                    evidence: entry.evidence,
                    provenance: entry.provenance,
                    initialMode: mode,
                },
                onApply: async (reason?: string) => {
                    await bridge.applyPromotion(entry.proposalId, reason);
                    await load();
                },
                onReject: async (reason: string) => {
                    await bridge.rejectPromotion(entry.proposalId, reason);
                    await load();
                },
            },
            i18n,
        );
    }
}

export function serializePromotionQueue(): string {
    return [
        mountPromotionQueue,
        readProposalRows,
        extractState,
        sortableValue,
        plainHeader,
        textCell,
        queueAction,
        renderLoading,
        renderError,
        renderEmptyState,
        asRecord,
        stringValue,
        numberValue,
        formatPercent,
        formatTimestamp,
        errorMessage,
        escapeHtml,
    ].map((entry) => entry.toString()).join('\n');
}

type PromotionSortKey = 'proposedClass' | 'confidence' | 'evidenceCount' | 'createdAt' | 'status';

interface PromotionRow {
    proposalId: string;
    proposalKind: string;
    summary: string;
    proposedClass: string;
    currentScope: string;
    targetScope: string;
    confidence: number;
    evidenceCount: number;
    createdAt: number;
    status: string;
    priorState?: unknown;
    proposedState?: unknown;
    evidence?: unknown;
    provenance?: unknown;
}

function readProposalRows(report: Record<string, unknown>): PromotionRow[] {
    const proposals = Array.isArray(report.proposals) ? report.proposals : [];
    return proposals
        .map((entry) => asRecord(entry))
        .filter((entry): entry is Record<string, unknown> => entry !== undefined)
        .map((entry) => {
            const proposed = extractState(entry.proposedState);
            const prior = extractState(entry.priorState);
            return {
                proposalId: stringValue(entry.proposalId),
                proposalKind: stringValue(entry.proposalKind),
                summary: stringValue(entry.summary),
                proposedClass: stringValue(proposed.memory_class ?? proposed.memoryClass ?? entry.proposedClass) || '—',
                currentScope: stringValue(prior.scope ?? entry.currentScope) || '—',
                targetScope: stringValue(proposed.scope ?? entry.targetScope) || '—',
                confidence: numberValue(proposed.confidence ?? entry.confidence),
                evidenceCount: numberValue(entry.evidenceCount),
                createdAt: numberValue(entry.createdAt),
                status: stringValue(entry.decision || entry.status || 'pending'),
                priorState: entry.priorState,
                proposedState: entry.proposedState,
                evidence: entry.evidence,
                provenance: entry.provenance,
            };
        });
}

function extractState(value: unknown): Record<string, unknown> {
    const record = asRecord(value);
    if (!record) {
        return {};
    }
    const nested = asRecord(record.memory);
    return nested ?? record;
}

function sortableValue(row: PromotionRow, key: PromotionSortKey): number | string {
    switch (key) {
        case 'proposedClass':
            return row.proposedClass.toLowerCase();
        case 'confidence':
            return row.confidence;
        case 'evidenceCount':
            return row.evidenceCount;
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

function formatPercent(value: number): string {
    return value ? `${(value * 100).toFixed(1)}%` : '—';
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
