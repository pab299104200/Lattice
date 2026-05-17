/// <reference lib="dom" />

interface ReviewI18nLike {
    t: (key: string, params?: Record<string, string | number>) => string;
}

interface ProposalDialogPayload {
    title: string;
    summary: string;
    proposalKind: string;
    status: string;
    proposedClass: string;
    currentScope: string;
    targetScope: string;
    confidence?: number;
    evidenceCount?: number;
    createdAt?: number;
    expectedEffect: string;
    priorState?: unknown;
    proposedState?: unknown;
    evidence?: unknown;
    provenance?: unknown;
    initialMode?: 'review' | 'accept' | 'reject';
}

interface ProposalDialogOptions {
    proposal: ProposalDialogPayload;
    onApply: (reason?: string) => Promise<unknown>;
    onReject: (reason: string) => Promise<unknown>;
}

export function openProposalDialog(options: ProposalDialogOptions, i18n: ReviewI18nLike): HTMLDialogElement {
    const dialog = document.createElement('dialog');
    dialog.className = 'proposal-dialog';

    const form = document.createElement('form');
    form.method = 'dialog';
    form.className = 'proposal-dialog__surface';

    const header = document.createElement('header');
    header.className = 'proposal-dialog__header';

    const titleWrap = document.createElement('div');
    const title = document.createElement('h3');
    title.textContent = options.proposal.title;
    const subtitle = document.createElement('p');
    subtitle.className = 'proposal-dialog__subtitle';
    subtitle.textContent = options.proposal.summary;
    titleWrap.append(title, subtitle);

    const closeButton = document.createElement('button');
    closeButton.type = 'button';
    closeButton.className = 'icon-button';
    closeButton.textContent = i18n.t('proposalDialog.close');
    closeButton.addEventListener('click', () => dialog.close());

    header.append(titleWrap, closeButton);

    const body = document.createElement('div');
    body.className = 'proposal-dialog__body';

    const errorBanner = document.createElement('div');
    errorBanner.className = 'banner error hidden';
    errorBanner.setAttribute('role', 'alert');

    body.append(
        errorBanner,
        buildSummarySection(options.proposal, i18n),
        buildEvidenceSection(options.proposal.evidence, i18n),
        buildProvenanceSection(options.proposal.provenance, i18n),
        buildDiffSection(options.proposal, i18n),
    );

    const footer = document.createElement('footer');
    footer.className = 'proposal-dialog__footer';

    const reasonWrap = document.createElement('label');
    reasonWrap.className = 'proposal-dialog__reason';
    const reasonLabel = document.createElement('span');
    reasonLabel.textContent = i18n.t('proposalDialog.rejectReasonLabel');
    const reasonHelp = document.createElement('small');
    reasonHelp.textContent = i18n.t('proposalDialog.rejectReasonHelp');
    const reasonInput = document.createElement('textarea');
    reasonInput.rows = 3;
    reasonInput.placeholder = i18n.t('proposalDialog.rejectReasonPlaceholder');
    reasonInput.value = '';
    reasonWrap.append(reasonLabel, reasonInput, reasonHelp);

    const confirmWrap = document.createElement('label');
    confirmWrap.className = 'proposal-dialog__confirm hidden';
    const confirmLabel = document.createElement('span');
    const confirmToken = typedConfirmToken(options.proposal.proposedClass);
    confirmLabel.textContent = i18n.t('proposalDialog.typedConfirmPrompt', { value: confirmToken });
    const confirmInput = document.createElement('input');
    confirmInput.type = 'text';
    confirmInput.autocomplete = 'off';
    confirmInput.spellcheck = false;
    confirmWrap.append(confirmLabel, confirmInput);

    const actions = document.createElement('div');
    actions.className = 'proposal-dialog__actions';
    const cancelButton = document.createElement('button');
    cancelButton.type = 'button';
    cancelButton.textContent = i18n.t('proposalDialog.cancel');
    cancelButton.addEventListener('click', () => dialog.close());

    const rejectButton = document.createElement('button');
    rejectButton.type = 'button';
    rejectButton.className = 'danger-button';
    rejectButton.textContent = i18n.t('proposalDialog.reject');
    rejectButton.disabled = true;

    const acceptButton = document.createElement('button');
    acceptButton.type = 'button';
    acceptButton.className = 'primary-button';
    acceptButton.textContent = i18n.t('proposalDialog.accept');

    actions.append(cancelButton, rejectButton, acceptButton);
    footer.append(reasonWrap, confirmWrap, actions);
    form.append(header, body, footer);
    dialog.append(form);
    document.body.append(dialog);

    const requiresTypedConfirm = ['repo', 'user', 'organization'].includes(
        options.proposal.targetScope.trim().toLowerCase(),
    );
    if (requiresTypedConfirm) {
        confirmWrap.classList.remove('hidden');
        acceptButton.disabled = true;
    }

    const syncButtons = (): void => {
        rejectButton.disabled = reasonInput.value.trim().length === 0;
        if (!requiresTypedConfirm) {
            acceptButton.disabled = false;
            return;
        }
        acceptButton.disabled = confirmInput.value.trim() !== confirmToken;
    };

    const setPending = (pending: boolean): void => {
        cancelButton.disabled = pending;
        rejectButton.disabled = pending || reasonInput.value.trim().length === 0;
        acceptButton.disabled = pending || (requiresTypedConfirm && confirmInput.value.trim() !== confirmToken);
        reasonInput.disabled = pending;
        confirmInput.disabled = pending;
    };

    const setError = (message: string): void => {
        errorBanner.textContent = message;
        errorBanner.classList.remove('hidden');
    };

    reasonInput.addEventListener('input', syncButtons);
    confirmInput.addEventListener('input', syncButtons);
    syncButtons();

    rejectButton.addEventListener('click', async () => {
        const reason = reasonInput.value.trim();
        if (!reason) {
            return;
        }
        setPending(true);
        errorBanner.classList.add('hidden');
        try {
            await options.onReject(reason);
            dialog.close();
        } catch (error) {
            setError(errorMessage(error, i18n));
            setPending(false);
        }
    });

    acceptButton.addEventListener('click', async () => {
        setPending(true);
        errorBanner.classList.add('hidden');
        try {
            await options.onApply(reasonInput.value.trim() || undefined);
            dialog.close();
        } catch (error) {
            setError(errorMessage(error, i18n));
            setPending(false);
        }
    });

    dialog.addEventListener('close', () => dialog.remove(), { once: true });
    dialog.showModal();

    if (options.proposal.initialMode === 'reject') {
        reasonInput.focus();
    } else if (requiresTypedConfirm || options.proposal.initialMode === 'accept') {
        confirmInput.focus();
    } else {
        acceptButton.focus();
    }

    return dialog;
}

export function serializeProposalDialog(): string {
    return [
        openProposalDialog,
        buildSummarySection,
        buildEvidenceSection,
        buildProvenanceSection,
        buildDiffSection,
        buildDiffPanel,
        appendDetail,
        collectEvidenceItems,
        typedConfirmToken,
        prettyJson,
        asRecord,
        numberValue,
        stringValue,
        hashValue,
        formatTimestamp,
        formatPercent,
        formatInteger,
        errorMessage,
        escapeHtml,
    ].map((entry) => entry.toString()).join('\n');
}

function buildSummarySection(proposal: ProposalDialogPayload, i18n: ReviewI18nLike): HTMLElement {
    const section = document.createElement('section');
    section.className = 'proposal-dialog__section';
    const heading = document.createElement('h4');
    heading.textContent = i18n.t('proposalDialog.summaryTitle');

    const grid = document.createElement('div');
    grid.className = 'detail-grid';
    appendDetail(grid, i18n.t('proposalDialog.proposalKind'), proposal.proposalKind);
    appendDetail(grid, i18n.t('proposalDialog.status'), proposal.status);
    appendDetail(grid, i18n.t('proposalDialog.proposedClass'), proposal.proposedClass);
    appendDetail(grid, i18n.t('proposalDialog.currentScope'), proposal.currentScope);
    appendDetail(grid, i18n.t('proposalDialog.targetScope'), proposal.targetScope);
    appendDetail(grid, i18n.t('proposalDialog.confidence'), formatPercent(proposal.confidence));
    appendDetail(grid, i18n.t('proposalDialog.evidenceCount'), formatInteger(proposal.evidenceCount));
    appendDetail(grid, i18n.t('proposalDialog.createdAt'), formatTimestamp(proposal.createdAt));
    appendDetail(grid, i18n.t('proposalDialog.expectedEffect'), proposal.expectedEffect);

    section.append(heading, grid);
    return section;
}

function buildEvidenceSection(evidence: unknown, i18n: ReviewI18nLike): HTMLElement {
    const section = document.createElement('section');
    section.className = 'proposal-dialog__section';
    const heading = document.createElement('h4');
    heading.textContent = i18n.t('proposalDialog.evidenceTitle');
    section.appendChild(heading);

    const list = document.createElement('ul');
    list.className = 'detail-list';
    const evidenceItems = collectEvidenceItems(evidence);
    if (evidenceItems.length === 0) {
        const empty = document.createElement('p');
        empty.className = 'muted';
        empty.textContent = i18n.t('proposalDialog.noEvidence');
        section.appendChild(empty);
        return section;
    }
    for (const item of evidenceItems) {
        const entry = document.createElement('li');
        entry.innerHTML = `<strong>${escapeHtml(item.label)}</strong>: ${escapeHtml(item.value)}`;
        list.appendChild(entry);
    }
    section.appendChild(list);
    return section;
}

function buildProvenanceSection(provenance: unknown, i18n: ReviewI18nLike): HTMLElement {
    const section = document.createElement('section');
    section.className = 'proposal-dialog__section';
    const heading = document.createElement('h4');
    heading.textContent = i18n.t('proposalDialog.provenanceTitle');
    section.appendChild(heading);

    const record = asRecord(provenance);
    if (!record) {
        const empty = document.createElement('p');
        empty.className = 'muted';
        empty.textContent = i18n.t('proposalDialog.noProvenance');
        section.appendChild(empty);
        return section;
    }

    const grid = document.createElement('div');
    grid.className = 'detail-grid';
    appendDetail(grid, i18n.t('proposalDialog.model'), stringValue(record.model));
    appendDetail(grid, i18n.t('proposalDialog.promptHash'), hashValue(record.prompt_sha256));
    appendDetail(grid, i18n.t('proposalDialog.responseHash'), hashValue(record.response_sha256));
    appendDetail(grid, i18n.t('proposalDialog.promptTokens'), formatInteger(numberValue(record.prompt_token_count)));
    appendDetail(grid, i18n.t('proposalDialog.responseTokens'), formatInteger(numberValue(record.response_token_count)));
    appendDetail(grid, i18n.t('proposalDialog.latencyMs'), formatInteger(numberValue(record.latency_ms)));
    appendDetail(grid, i18n.t('proposalDialog.calledAt'), formatTimestamp(numberValue(asRecord(record.called_at)?.unix_micros)));
    section.appendChild(grid);
    return section;
}

function buildDiffSection(proposal: ProposalDialogPayload, i18n: ReviewI18nLike): HTMLElement {
    const section = document.createElement('section');
    section.className = 'proposal-dialog__section';
    const heading = document.createElement('h4');
    heading.textContent = i18n.t('proposalDialog.diffTitle');

    const comparison = document.createElement('div');
    comparison.className = 'diff-grid';
    comparison.append(
        buildDiffPanel(i18n.t('proposalDialog.before'), proposal.priorState),
        buildDiffPanel(i18n.t('proposalDialog.after'), proposal.proposedState),
    );

    section.append(heading, comparison);
    return section;
}

function buildDiffPanel(label: string, value: unknown): HTMLElement {
    const panel = document.createElement('article');
    panel.className = 'diff-panel';
    const heading = document.createElement('strong');
    heading.textContent = label;
    const content = document.createElement('pre');
    content.textContent = prettyJson(value);
    panel.append(heading, content);
    return panel;
}

function appendDetail(container: HTMLElement, label: string, value: string): void {
    const row = document.createElement('div');
    const key = document.createElement('span');
    key.textContent = label;
    const body = document.createElement('strong');
    body.textContent = value;
    row.append(key, body);
    container.appendChild(row);
}

function collectEvidenceItems(evidence: unknown): Array<{ label: string; value: string }> {
    const record = asRecord(evidence);
    if (!record) {
        return [];
    }
    const items: Array<{ label: string; value: string }> = [];
    for (const [key, value] of Object.entries(record)) {
        if (Array.isArray(value)) {
            for (const entry of value) {
                if (typeof entry === 'string') {
                    items.push({ label: key, value: entry });
                }
            }
            continue;
        }
        if (typeof value === 'string' || typeof value === 'number' || typeof value === 'boolean') {
            items.push({ label: key, value: String(value) });
        }
    }
    return items;
}

function typedConfirmToken(proposedClass: string): string {
    return proposedClass.replace(/[^A-Za-z0-9]+/g, '');
}

function prettyJson(value: unknown): string {
    if (value === undefined) {
        return '{}';
    }
    try {
        return JSON.stringify(value, null, 2);
    } catch {
        return '{}';
    }
}

function asRecord(value: unknown): Record<string, unknown> | undefined {
    return typeof value === 'object' && value !== null && !Array.isArray(value)
        ? value as Record<string, unknown>
        : undefined;
}

function numberValue(value: unknown): number | undefined {
    return typeof value === 'number' ? value : undefined;
}

function stringValue(value: unknown): string {
    return typeof value === 'string' && value.trim().length > 0 ? value : '—';
}

function hashValue(value: unknown): string {
    if (!Array.isArray(value)) {
        return '—';
    }
    const bytes = value.filter((entry): entry is number => typeof entry === 'number');
    if (bytes.length === 0) {
        return '—';
    }
    return bytes.map((entry) => entry.toString(16).padStart(2, '0')).join('');
}

function formatTimestamp(value: number | undefined): string {
    if (!value || value <= 0) {
        return '—';
    }
    const millis = value > 1_000_000_000_000 ? Math.floor(value / 1000) : value * 1000;
    return new Date(millis).toLocaleString();
}

function formatPercent(value: number | undefined): string {
    if (typeof value !== 'number') {
        return '—';
    }
    return `${(value * 100).toFixed(1)}%`;
}

function formatInteger(value: number | undefined): string {
    if (typeof value !== 'number') {
        return '—';
    }
    return Math.round(value).toLocaleString();
}

function errorMessage(error: unknown, i18n: ReviewI18nLike): string {
    if (error instanceof Error && error.message) {
        return error.message;
    }
    return i18n.t('proposalDialog.genericError');
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
