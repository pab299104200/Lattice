import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewMemory } from './rpcPayloads';
import { renderMemoryRow } from './components/MemoryRow';
import { escapeAttr, escapeHtml } from './components/html';

type MemoryInboxRoute = 'memoryInbox' | 'evidenceInspector' | 'eventTrace';
type MemoryInboxFilterKey = 'status' | 'scope' | 'memoryClass';
type MemoryInboxSortKey = 'status' | 'memoryClass' | 'scope' | 'lastVerifiedAt' | 'confidence';
type SortDirection = 'asc' | 'desc';

interface FilterOption {
    value: string;
    label: string;
    detail: string;
}

export interface ReviewRouteView {
    html: string;
    testId: string;
}

export interface ReviewPanelMessage {
    command?: string;
    filterKey?: MemoryInboxFilterKey;
    sortKey?: MemoryInboxSortKey;
    page?: number;
    pageSize?: number;
    memoryId?: string;
}

export interface MemoryInboxHost {
    navigate(route: MemoryInboxRoute, context?: Record<string, string>): Promise<void>;
    pickFilter(args: {
        title: string;
        placeholder: string;
        selectedValues: string[];
        options: FilterOption[];
    }): Promise<string[] | undefined>;
    showError(message: string): void;
}

export interface MountedMemoryInbox {
    dispose(): void;
    refresh(): Promise<ReviewRouteView>;
    handleMessage(message: ReviewPanelMessage): Promise<ReviewRouteView | undefined>;
}

interface MemoryInboxFilters {
    status: string[];
    scope: string[];
    memoryClass: string[];
}

interface SortState {
    key: MemoryInboxSortKey;
    direction: SortDirection;
}

const STATUS_VALUES = [
    'verified',
    'unverified',
    'in_review',
    'stale',
    'contradicted',
    'superseded',
    'expired',
    'invalidated',
] as const;

const SCOPE_VALUES = ['session', 'branch', 'repo', 'user', 'organization'] as const;

const MEMORY_CLASS_VALUES = [
    'observation',
    'decision',
    'constraint',
    'pattern',
    'anti_pattern',
    'workflow_outcome',
    'failure_pattern',
    'procedure',
    'preference',
    'architecture_invariant',
    'docs_contract',
    'open_question',
    'counter_memory',
] as const;

// Spec anchor: `## 10. Human Review Surface` defines memory inbox as the primary review list.
export function mountMemoryInbox(
    host: MemoryInboxHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n
): MountedMemoryInbox {
    let disposed = false;
    let memories: ReviewMemory[] = [];
    let errorMessage = '';
    let isLoading = true;
    let page = 1;
    let pageSize = 25;
    let filters: MemoryInboxFilters = {
        status: [],
        scope: [],
        memoryClass: [],
    };
    let sort: SortState = { key: 'lastVerifiedAt', direction: 'desc' };
    const refreshInbox = async (): Promise<ReviewRouteView> => {
        isLoading = true;
        errorMessage = '';
        try {
            const result = await bridge.listMemories({
                limit: 200,
                status: filters.status,
                scope: filters.scope,
                memoryClass: filters.memoryClass,
            });
            if (!result.ok) {
                errorMessage = result.error.message;
                isLoading = false;
                host.showError(errorMessage);
                return renderView();
            }
            memories = result.value.memories;
            normalizePage();
            isLoading = false;
            return renderView();
        } catch (error) {
            const message = error instanceof Error ? error.message : i18n.t('memoryInbox.error');
            errorMessage = message;
            isLoading = false;
            host.showError(message);
            return renderView();
        }
    };

    return {
        dispose() {
            disposed = true;
        },
        refresh: refreshInbox,
        async handleMessage(message) {
            if (disposed) {
                return undefined;
            }
            switch (message.command) {
                case 'memoryInbox.pickFilter':
                    return handleFilterSelection(message.filterKey);
                case 'memoryInbox.clearFilters':
                    filters = { status: [], scope: [], memoryClass: [] };
                    page = 1;
                    return refreshInbox();
                case 'memoryInbox.sort':
                    if (message.sortKey) {
                        sort = nextSort(sort, message.sortKey);
                    }
                    return renderView();
                case 'memoryInbox.page':
                    if (typeof message.page === 'number') {
                        page = clampPage(message.page, memories.length, pageSize);
                    }
                    return renderView();
                case 'memoryInbox.pageSize':
                    if (typeof message.pageSize === 'number') {
                        pageSize = message.pageSize;
                        page = 1;
                    }
                    return renderView();
                case 'memoryInbox.openEvidence':
                    if (message.memoryId) {
                        await host.navigate('evidenceInspector', { memoryId: message.memoryId });
                    }
                    return undefined;
                case 'memoryInbox.openTrace':
                    if (message.memoryId) {
                        await host.navigate('eventTrace', { memoryId: message.memoryId });
                    }
                    return undefined;
                default:
                    return undefined;
            }
        },
    };

    async function handleFilterSelection(
        filterKey: MemoryInboxFilterKey | undefined
    ): Promise<ReviewRouteView | undefined> {
        if (!filterKey) {
            return undefined;
        }
        const selectedValues = filters[filterKey];
        const selection = await host.pickFilter({
            title: i18n.t(`memoryInbox.filters.${filterKey}.title`),
            placeholder: i18n.t(`memoryInbox.filters.${filterKey}.placeholder`),
            selectedValues,
            options: filterOptions(filterKey),
        });
        if (!selection) {
            return undefined;
        }
        filters = {
            ...filters,
            [filterKey]: selection,
        };
        page = 1;
        return refreshInbox();
    }

    function renderView(): ReviewRouteView {
        return {
            testId: 'review-route-memoryInbox',
            html: renderContent(),
        };
    }

    function renderContent(): string {
        if (isLoading) {
            return loadingHtml();
        }
        if (errorMessage) {
            return errorHtml(errorMessage);
        }
        return [
            '<section class="table-card" data-testid="review-memory-inbox">',
            renderFilters(),
            renderTable(),
            renderPagination(),
            '</section>',
        ].join('');
    }

    function renderFilters(): string {
        return [
            '<div class="filter-bar">',
            filterButton('status', filters.status),
            filterButton('scope', filters.scope),
            filterButton('memoryClass', filters.memoryClass),
            '<button type="button" data-command="memoryInbox.clearFilters">',
            escapeHtml(i18n.t('memoryInbox.actions.clearFilters')),
            '</button>',
            '</div>',
            '<div class="filter-summary">',
            activeFilterSummary('status', filters.status),
            activeFilterSummary('scope', filters.scope),
            activeFilterSummary('memoryClass', filters.memoryClass),
            '</div>',
        ].join('');
    }

    function renderTable(): string {
        const sortedMemories = [...memories].sort(compareMemories);
        const total = sortedMemories.length;
        if (total === 0) {
            return emptyHtml(hasActiveFilters(filters));
        }
        const pagedMemories = pageItems(sortedMemories, page, pageSize);
        const rows = pagedMemories.map((memory) => renderMemoryRow(memory, i18n)).join('');
        return [
            '<div class="table-wrap">',
            '<table class="review-table" aria-label="',
            escapeAttr(i18n.t('memoryInbox.tableLabel')),
            '">',
            '<thead><tr>',
            sortableHeader('status', 'memoryInbox.columns.status'),
            sortableHeader('memoryClass', 'memoryInbox.columns.class'),
            sortableHeader('scope', 'memoryInbox.columns.scope'),
            staticHeader('memoryInbox.columns.content'),
            sortableHeader('lastVerifiedAt', 'memoryInbox.columns.lastVerified'),
            sortableHeader('confidence', 'memoryInbox.columns.confidence'),
            staticHeader('memoryInbox.columns.actions'),
            '</tr></thead>',
            '<tbody>',
            rows,
            '</tbody>',
            '</table>',
            '</div>',
        ].join('');
    }

    function renderPagination(): string {
        const total = memories.length;
        if (total === 0) {
            return '';
        }
        const totalPages = Math.max(1, Math.ceil(total / pageSize));
        const start = (page - 1) * pageSize + 1;
        const end = Math.min(page * pageSize, total);
        const pageButtons = [10, 25, 50, 100]
            .map((size) => [
                '<button type="button" ',
                size === pageSize ? 'class="active"' : '',
                ' data-command="memoryInbox.pageSize" data-page-size="',
                String(size),
                '">',
                escapeHtml(i18n.t('memoryInbox.pageSizeLabel', { pageSize: size })),
                '</button>',
            ].join(''))
            .join('');
        return [
            '<div class="pagination-bar">',
            '<span>',
            escapeHtml(i18n.t('memoryInbox.pagination.showing', { start, end, total })),
            '</span>',
            '<div class="pagination-controls">',
            '<button type="button" data-command="memoryInbox.page" data-page="',
            String(page - 1),
            '" ',
            page <= 1 ? 'disabled' : '',
            '>',
            escapeHtml(i18n.t('memoryInbox.pagination.previous')),
            '</button>',
            '<span>',
            escapeHtml(i18n.t('memoryInbox.pagination.page', { page, totalPages })),
            '</span>',
            '<button type="button" data-command="memoryInbox.page" data-page="',
            String(page + 1),
            '" ',
            page >= totalPages ? 'disabled' : '',
            '>',
            escapeHtml(i18n.t('memoryInbox.pagination.next')),
            '</button>',
            '</div>',
            '<div class="page-size-group">',
            pageButtons,
            '</div>',
            '</div>',
        ].join('');
    }

    function loadingHtml(): string {
        return [
            '<section class="table-card" data-testid="review-memory-inbox">',
            '<div class="banner">',
            escapeHtml(i18n.t('memoryInbox.loadingHint')),
            '</div>',
            '<div class="summary-grid">',
            '<div class="skeleton"></div>',
            '<div class="skeleton"></div>',
            '<div class="skeleton"></div>',
            '</div>',
            '</section>',
        ].join('');
    }

    function errorHtml(message: string): string {
        return [
            '<section class="table-card" data-testid="review-memory-inbox">',
            '<div class="banner error" role="alert">',
            escapeHtml(i18n.t('memoryInbox.errorBanner', { message })),
            '</div>',
            '<button type="button" data-command="refresh">',
            escapeHtml(i18n.t('reviewPanel.retry')),
            '</button>',
            '</section>',
        ].join('');
    }

    function emptyHtml(hasFiltersApplied: boolean): string {
        const key = hasFiltersApplied ? 'memoryInbox.emptyFiltered' : 'memoryInbox.emptyInitial';
        return [
            '<section class="empty-state" data-testid="review-memory-inbox">',
            '<h3>',
            escapeHtml(i18n.t('memoryInbox.emptyTitle')),
            '</h3>',
            '<p>',
            escapeHtml(i18n.t(key)),
            '</p>',
            '</section>',
        ].join('');
    }

    function filterButton(filterKey: MemoryInboxFilterKey, selectedValues: string[]): string {
        const count = selectedValues.length;
        return [
            '<button type="button" data-command="memoryInbox.pickFilter" data-filter-key="',
            escapeAttr(filterKey),
            '" data-testid="memory-inbox-filter-',
            escapeAttr(filterKey),
            '">',
            escapeHtml(i18n.t(`memoryInbox.filters.${filterKey}.button`, { count })),
            '</button>',
        ].join('');
    }

    function activeFilterSummary(filterKey: MemoryInboxFilterKey, values: string[]): string {
        const summary = values.length === 0
            ? i18n.t('memoryInbox.filters.all')
            : values.map((value) => filterLabel(filterKey, value)).join(', ');
        return [
            '<span>',
            escapeHtml(i18n.t(`memoryInbox.filters.${filterKey}.summary`, { values: summary })),
            '</span>',
        ].join('');
    }

    function sortableHeader(sortKey: MemoryInboxSortKey, labelKey: string): string {
        const isActive = sort.key === sortKey;
        const direction = isActive ? sort.direction : 'desc';
        const indicator = isActive ? (sort.direction === 'asc' ? '↑' : '↓') : '↕';
        return [
            '<th scope="col"><button type="button" class="sort-button" data-command="memoryInbox.sort" data-sort-key="',
            escapeAttr(sortKey),
            '">',
            escapeHtml(i18n.t(labelKey)),
            ' <span aria-hidden="true">',
            indicator,
            '</span><span class="sr-only">',
            escapeHtml(i18n.t('memoryInbox.sortDirection', { direction })),
            '</span></button></th>',
        ].join('');
    }

    function staticHeader(labelKey: string): string {
        return `<th scope="col">${escapeHtml(i18n.t(labelKey))}</th>`;
    }

    function compareMemories(left: ReviewMemory, right: ReviewMemory): number {
        const direction = sort.direction === 'asc' ? 1 : -1;
        const leftValue = sortValue(left, sort.key);
        const rightValue = sortValue(right, sort.key);
        if (leftValue < rightValue) {
            return -1 * direction;
        }
        if (leftValue > rightValue) {
            return 1 * direction;
        }
        return left.id.localeCompare(right.id);
    }

    function sortValue(memory: ReviewMemory, key: MemoryInboxSortKey): number | string {
        switch (key) {
            case 'confidence':
                return memory.confidence;
            case 'lastVerifiedAt':
                return memory.lastVerifiedAt ?? 0;
            case 'memoryClass':
                return filterLabel('memoryClass', memory.memoryClass);
            case 'scope':
                return filterLabel('scope', memory.scope);
            case 'status':
                return filterLabel('status', memory.verificationStatus);
            default:
                return '';
        }
    }

    function normalizePage(): void {
        const totalPages = Math.max(1, Math.ceil(memories.length / pageSize));
        if (page > totalPages) {
            page = totalPages;
        }
    }

    function filterOptions(filterKey: MemoryInboxFilterKey): FilterOption[] {
        switch (filterKey) {
            case 'status':
                return STATUS_VALUES.map((value) => ({
                    value,
                    label: i18n.t(`reviewStatus.${value}`),
                    detail: i18n.t('memoryInbox.filters.status.detail'),
                }));
            case 'scope':
                return SCOPE_VALUES.map((value) => ({
                    value,
                    label: i18n.t(`memoryInbox.scope.${value}`),
                    detail: i18n.t('memoryInbox.filters.scope.detail'),
                }));
            case 'memoryClass':
                return MEMORY_CLASS_VALUES.map((value) => ({
                    value,
                    label: i18n.t(`memoryInbox.class.${value}`),
                    detail: i18n.t('memoryInbox.filters.memoryClass.detail'),
                }));
            default:
                return [];
        }
    }

    function filterLabel(filterKey: MemoryInboxFilterKey, value: string): string {
        if (filterKey === 'status') {
            return i18n.t(`reviewStatus.${value}`);
        }
        return i18n.t(`memoryInbox.${filterKey === 'scope' ? 'scope' : 'class'}.${value}`);
    }
}

function hasActiveFilters(filters: MemoryInboxFilters): boolean {
    return filters.status.length > 0 || filters.scope.length > 0 || filters.memoryClass.length > 0;
}

function nextSort(current: SortState, sortKey: MemoryInboxSortKey): SortState {
    if (current.key !== sortKey) {
        return { key: sortKey, direction: sortKey === 'confidence' ? 'desc' : 'asc' };
    }
    return {
        key: current.key,
        direction: current.direction === 'asc' ? 'desc' : 'asc',
    };
}

function pageItems<T>(items: T[], page: number, pageSize: number): T[] {
    const start = (page - 1) * pageSize;
    return items.slice(start, start + pageSize);
}

function clampPage(nextPage: number, totalItems: number, pageSize: number): number {
    const totalPages = Math.max(1, Math.ceil(totalItems / pageSize));
    return Math.min(Math.max(nextPage, 1), totalPages);
}
