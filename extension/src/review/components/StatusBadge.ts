/// <reference lib="dom" />

export type VerificationStatus =
    | 'verified'
    | 'unverified'
    | 'in_review'
    | 'stale'
    | 'contradicted'
    | 'superseded'
    | 'expired'
    | 'invalidated'
    | 'pending'
    | 'applied'
    | 'rejected'
    | 'reverted'
    | 'queued'
    | 'running'
    | 'proposed'
    | 'failed'
    | 'dropped';

interface ReviewI18nLike {
    t: (key: string, params?: Record<string, string | number>) => string;
}

interface StatusPresentation {
    className: string;
    labelKey: string;
}

const STATUS_PRESENTATION: Record<string, StatusPresentation> = {
    verified: { className: 'status-badge--success', labelKey: 'reviewStatus.verified' },
    unverified: { className: 'status-badge--muted', labelKey: 'reviewStatus.unverified' },
    in_review: { className: 'status-badge--warning', labelKey: 'reviewStatus.in_review' },
    stale: { className: 'status-badge--warning', labelKey: 'reviewStatus.stale' },
    contradicted: { className: 'status-badge--danger', labelKey: 'reviewStatus.contradicted' },
    superseded: { className: 'status-badge--info', labelKey: 'reviewStatus.superseded' },
    expired: { className: 'status-badge--danger', labelKey: 'reviewStatus.expired' },
    invalidated: { className: 'status-badge--danger', labelKey: 'reviewStatus.invalidated' },
    pending: { className: 'status-badge--warning', labelKey: 'reviewStatus.pending' },
    applied: { className: 'status-badge--success', labelKey: 'reviewStatus.applied' },
    rejected: { className: 'status-badge--danger', labelKey: 'reviewStatus.rejected' },
    reverted: { className: 'status-badge--muted', labelKey: 'reviewStatus.reverted' },
    queued: { className: 'status-badge--muted', labelKey: 'reviewStatus.queued' },
    running: { className: 'status-badge--info', labelKey: 'reviewStatus.running' },
    proposed: { className: 'status-badge--warning', labelKey: 'reviewStatus.proposed' },
    failed: { className: 'status-badge--danger', labelKey: 'reviewStatus.failed' },
    dropped: { className: 'status-badge--danger', labelKey: 'reviewStatus.dropped' },
};

// Single source of truth for proposal + verification status styling across the review surface.
export function renderStatusBadge(status: VerificationStatus | string, i18n: ReviewI18nLike): string {
    const normalized = typeof status === 'string' ? status.trim().toLowerCase() : '';
    const presentation = STATUS_PRESENTATION[normalized] ?? {
        className: 'status-badge--muted',
        labelKey: 'reviewPanel.statusUnknown',
    };
    const label = i18n.t(presentation.labelKey)
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
    return `<span class="status-badge ${presentation.className}">${label}</span>`;
}
