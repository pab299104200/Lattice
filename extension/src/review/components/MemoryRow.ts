import { ReviewI18n } from '../i18n';
import { ReviewMemory } from '../rpcPayloads';
import { escapeAttr, escapeHtml } from './html';
import { renderStatusBadge } from './StatusBadge';

const CONTENT_PREVIEW_LIMIT = 120;

export function renderMemoryRow(memory: ReviewMemory, i18n: ReviewI18n): string {
    const contentPreview = truncateContent(memory.content);
    const contentTitle = memory.content.trim() || i18n.t('memoryInbox.noContent');
    const confidence = formatConfidence(memory.confidence);
    const lastVerified = formatLastVerified(memory.lastVerifiedAt, i18n);
    const classLabel = i18n.has(`memoryInbox.class.${memory.memoryClass}`)
        ? i18n.t(`memoryInbox.class.${memory.memoryClass}`)
        : memory.memoryClass;
    const scopeLabel = i18n.has(`memoryInbox.scope.${memory.scope}`)
        ? i18n.t(`memoryInbox.scope.${memory.scope}`)
        : memory.scope;

    return [
        '<tr class="memory-row" data-command="memoryInbox.openEvidence" data-memory-id="',
        escapeAttr(memory.id),
        '" data-testid="memory-inbox-row-',
        escapeAttr(memory.id),
        '">',
        '<td>',
        renderStatusBadge(memory.verificationStatus, i18n),
        '</td>',
        '<td>',
        escapeHtml(classLabel),
        '</td>',
        '<td>',
        escapeHtml(scopeLabel),
        '</td>',
        '<td class="content-cell" title="',
        escapeAttr(contentTitle),
        '">',
        escapeHtml(contentPreview),
        '</td>',
        '<td>',
        escapeHtml(lastVerified),
        '</td>',
        '<td>',
        escapeHtml(confidence),
        '</td>',
        '<td><div class="row-actions">',
        '<button type="button" class="link-button" data-command="memoryInbox.openEvidence" data-memory-id="',
        escapeAttr(memory.id),
        '" data-testid="memory-inbox-evidence-',
        escapeAttr(memory.id),
        '">',
        escapeHtml(i18n.t('memoryInbox.actions.viewEvidence')),
        '</button>',
        '<button type="button" class="link-button" data-command="memoryInbox.openTrace" data-memory-id="',
        escapeAttr(memory.id),
        '" data-testid="memory-inbox-trace-',
        escapeAttr(memory.id),
        '">',
        escapeHtml(i18n.t('memoryInbox.actions.openTrace')),
        '</button>',
        '</div></td>',
        '</tr>',
    ].join('');
}

function truncateContent(content: string): string {
    const normalized = content.replace(/\s+/g, ' ').trim();
    if (!normalized) {
        return '';
    }
    if (normalized.length <= CONTENT_PREVIEW_LIMIT) {
        return normalized;
    }
    return `${normalized.slice(0, CONTENT_PREVIEW_LIMIT - 1)}…`;
}

function formatLastVerified(lastVerifiedAt: number | undefined, i18n: ReviewI18n): string {
    if (!lastVerifiedAt) {
        return i18n.t('memoryInbox.lastVerifiedNever');
    }
    return new Intl.DateTimeFormat(undefined, {
        dateStyle: 'medium',
        timeStyle: 'short',
    }).format(new Date(lastVerifiedAt * 1000));
}

function formatConfidence(confidence: number): string {
    return new Intl.NumberFormat(undefined, {
        style: 'percent',
        maximumFractionDigits: 1,
    }).format(confidence);
}
