import { ReviewI18n } from './i18n';
import { renderStatusBadge } from './components/StatusBadge';
import { ReviewRpcBridgeContract } from './rpcBridge';
import {
    ReviewEventTraceEntry,
    ReviewMemory,
    ReviewMemoryEvidenceBundle,
    ReviewVerifyExplainResponse,
} from './rpcPayloads';

export interface EvidenceInspectorState {
    memoryId?: string;
    reverifyInFlight: boolean;
    inlineError?: string;
}

export interface EvidenceInspectorHost {
    readonly state: EvidenceInspectorState;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountEvidenceInspector(
    host: EvidenceInspectorHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n,
    memoryId?: string
): Promise<void> {
    if (!memoryId) {
        host.setContent(`<div class="placeholder" data-testid="review-evidence-inspector"><h3>${escapeHtml(i18n.t('evidenceInspector.selectPrompt'))}</h3></div>`);
        return;
    }
    try {
        const result = await bridge.getMemoryEvidence(memoryId);
        if (!result.ok) {
            host.reportError?.(result.error.message);
            host.setContent(renderError(i18n, result.error.message));
            return;
        }
        host.setContent(renderInspector(result.value, host.state, i18n));
    } catch (error) {
        const message = error instanceof Error ? error.message : String(error);
        host.reportError?.(message);
        host.setContent(renderError(i18n, message));
    }
}

function renderInspector(
    bundle: ReviewMemoryEvidenceBundle,
    state: EvidenceInspectorState,
    i18n: ReviewI18n
): string {
    const { memory, provenanceEvents, lastVerification } = bundle;
    const evidence = asObjectArray(memory.evidence);
    const links = asObjectArray(memory.links);
    const provenance = asObjectArray(memory.provenance);

    return `<section class="route-stack" data-testid="review-evidence-inspector">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('evidenceInspector.title'))}</h3>
      <p>${escapeHtml(i18n.t('evidenceInspector.subtitle', { id: memory.id }))}</p>
    </div>
    <div class="toolbar-actions">
      <button data-command="openEventTraceForMemory" data-memory-id="${escapeHtml(memory.id)}">
        ${escapeHtml(i18n.t('evidenceInspector.openTrace'))}
      </button>
      <button data-command="reverifyInspectorMemory" data-memory-id="${escapeHtml(memory.id)}" ${state.reverifyInFlight ? 'disabled' : ''}>
        ${state.reverifyInFlight ? escapeHtml(i18n.t('evidenceInspector.reverifyLoading')) : escapeHtml(i18n.t('evidenceInspector.reverify'))}
      </button>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  <div class="summary-grid">
    ${summaryCard(i18n.t('evidenceInspector.memoryId'), memory.id)}
    ${summaryCard(i18n.t('evidenceInspector.memoryClass'), formatClass(memory.memoryClass, i18n))}
    ${summaryCard(i18n.t('evidenceInspector.scope'), formatScope(memory.scope, i18n))}
    <div class="card"><small>${escapeHtml(i18n.t('evidenceInspector.status'))}</small><strong>${renderStatusBadge(memory.verificationStatus, i18n)}</strong></div>
  </div>
  <div class="kv">
    ${kvItem(i18n.t('evidenceInspector.confidence'), formatConfidence(memory))}
    ${kvItem(i18n.t('evidenceInspector.confidenceReason'), memory.confidenceReason ?? i18n.t('evidenceInspector.notAvailable'))}
    ${kvItem(i18n.t('evidenceInspector.lastVerifiedAt'), formatTimestamp(memory.lastVerifiedAt, i18n))}
    ${kvItem(i18n.t('evidenceInspector.lastVerificationResult'), describeVerification(lastVerification, memory, i18n))}
  </div>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.contentPanel'))}</h4>
    <p>${escapeHtml(memory.content)}</p>
  </section>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.evidenceList'))}</h4>
    ${evidence.length ? `<div class="list-stack">${evidence.map((entry) => renderEvidence(entry, i18n)).join('')}</div>` : renderInlineEmpty(i18n.t('evidenceInspector.noEvidence'))}
  </section>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.provenanceList'))}</h4>
    ${provenance.length ? `<div class="list-stack">${provenance.map((entry) => renderProvenance(entry, i18n)).join('')}</div>` : renderInlineEmpty(i18n.t('evidenceInspector.noProvenance'))}
  </section>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.eventHistory'))}</h4>
    ${provenanceEvents.length ? `<div class="list-stack">${provenanceEvents.map((event) => renderEvent(event, i18n)).join('')}</div>` : renderInlineEmpty(i18n.t('evidenceInspector.noEvents'))}
  </section>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.linkList'))}</h4>
    ${links.length ? `<div class="list-stack">${links.map((entry) => renderLink(entry, i18n)).join('')}</div>` : renderInlineEmpty(i18n.t('evidenceInspector.noLinks'))}
  </section>
  <section class="panel-block">
    <h4>${escapeHtml(i18n.t('evidenceInspector.verificationPanel'))}</h4>
    <div class="list-stack">
      <div class="list-card">${escapeHtml(i18n.t('evidenceInspector.lastVerifiedAt'))}: ${escapeHtml(formatTimestamp(memory.lastVerifiedAt, i18n))}</div>
      <div class="list-card">${escapeHtml(i18n.t('evidenceInspector.lastVerificationResult'))}: ${escapeHtml(describeVerification(lastVerification, memory, i18n))}</div>
      ${renderVerificationChecks(lastVerification, i18n)}
    </div>
  </section>
</section>`;
}

function renderEvidence(entry: Record<string, unknown>, i18n: ReviewI18n): string {
    const reference = stringField(entry, 'reference');
    const detail = stringField(entry, 'detail');
    const kind = stringField(entry, 'kind') || i18n.t('evidenceInspector.notAvailable');
    const capturedAt = formatTimestamp(numberField(entry, 'captured_at'), i18n);
    return `<article class="list-card">
  <strong>${escapeHtml(kind)}</strong>
  <span>${escapeHtml(detail || i18n.t('evidenceInspector.notAvailable'))}</span>
  <small>${escapeHtml(capturedAt)}</small>
  ${reference ? renderReferenceButton(reference, i18n.t('evidenceInspector.openReference')) : ''}
</article>`;
}

function renderProvenance(entry: Record<string, unknown>, i18n: ReviewI18n): string {
    const source = stringField(entry, 'source') || i18n.t('evidenceInspector.notAvailable');
    const note = stringField(entry, 'note') || i18n.t('evidenceInspector.notAvailable');
    const reference = stringField(entry, 'reference');
    const capturedAt = formatTimestamp(numberField(entry, 'captured_at'), i18n);
    return `<article class="list-card">
  <strong>${escapeHtml(source)}</strong>
  <span>${escapeHtml(note)}</span>
  <small>${escapeHtml(capturedAt)}</small>
  ${reference ? renderReferenceButton(reference, i18n.t('evidenceInspector.openReference')) : ''}
</article>`;
}

function renderEvent(event: ReviewEventTraceEntry, i18n: ReviewI18n): string {
    return `<article class="list-card">
  <strong>${escapeHtml(event.kind)}</strong>
  <span>${escapeHtml(event.summary)}</span>
  <small>${escapeHtml(event.timestamp)}</small>
  <div class="row-actions">
    <button data-command="openEventTraceForEvent" data-event-id="${escapeHtml(event.eventId)}">${escapeHtml(i18n.t('evidenceInspector.openTrace'))}</button>
  </div>
</article>`;
}

function renderLink(entry: Record<string, unknown>, i18n: ReviewI18n): string {
    const target = stringField(entry, 'target_memory_id') || stringField(entry, 'target');
    const type = stringField(entry, 'link_type') || i18n.t('evidenceInspector.notAvailable');
    const reason = stringField(entry, 'reason') || i18n.t('evidenceInspector.notAvailable');
    const strength = stringField(entry, 'strength') || stringField(entry, 'link_strength') || '—';
    return `<article class="list-card">
  <strong>${escapeHtml(type)}</strong>
  <span>${escapeHtml(target || i18n.t('evidenceInspector.notAvailable'))}</span>
  <small>${escapeHtml(i18n.t('evidenceInspector.linkReason', { value: reason }))}</small>
  <small>${escapeHtml(i18n.t('evidenceInspector.linkStrength', { value: strength }))}</small>
</article>`;
}

function renderVerificationChecks(
    verification: ReviewVerifyExplainResponse | undefined,
    i18n: ReviewI18n
): string {
    if (!verification?.checks.length) {
        return `<div class="list-card">${escapeHtml(i18n.t('evidenceInspector.noVerificationChecks'))}</div>`;
    }
    return verification.checks.map((check) => `<article class="list-card">
  <strong>${escapeHtml(check.kind)}</strong>
  <span>${escapeHtml(check.detail)}</span>
  <small>${escapeHtml(check.target)}</small>
</article>`).join('');
}

function renderReferenceButton(reference: string, label: string): string {
    return `<div class="row-actions">
  <button data-command="openMemoryReference" data-value="${escapeHtml(reference)}">${escapeHtml(label)}</button>
</div>`;
}

function renderInlineEmpty(message: string): string {
    return `<div class="placeholder compact"><h3>${escapeHtml(message)}</h3></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-evidence-inspector"><div class="banner error" role="alert">
  <strong>${escapeHtml(i18n.t('evidenceInspector.errorTitle'))}</strong>
  <p>${escapeHtml(message)}</p>
</div></section>`;
}

function summaryCard(label: string, value: string, raw = false): string {
    return `<div class="card"><small>${escapeHtml(label)}</small><strong>${raw ? value : escapeHtml(value)}</strong></div>`;
}

function kvItem(label: string, value: string): string {
    return `<div><span>${escapeHtml(label)}</span><strong>${escapeHtml(value)}</strong></div>`;
}

function formatConfidence(memory: ReviewMemory): string {
    return `${Math.round(memory.confidence * 100)}%`;
}

function describeVerification(
    verification: ReviewVerifyExplainResponse | undefined,
    memory: ReviewMemory,
    i18n: ReviewI18n
): string {
    if (verification?.summaryLines.length) {
        return verification.summaryLines[0];
    }
    if (i18n.has(`reviewStatus.${memory.verificationStatus}`)) {
        return i18n.t(`reviewStatus.${memory.verificationStatus}`);
    }
    return memory.verificationStatus;
}

function formatClass(memoryClass: string, i18n: ReviewI18n): string {
    return i18n.has(`staleView.class.${memoryClass}`) ? i18n.t(`staleView.class.${memoryClass}`) : memoryClass;
}

function formatScope(scope: string, i18n: ReviewI18n): string {
    return i18n.has(`staleView.scope.${scope}`) ? i18n.t(`staleView.scope.${scope}`) : scope;
}

function formatTimestamp(value: number | undefined, i18n: ReviewI18n): string {
    if (!value) {
        return i18n.t('evidenceInspector.notAvailable');
    }
    return new Date(value * 1000).toLocaleString();
}

function asObjectArray(values: unknown[]): Record<string, unknown>[] {
    return values.filter((value): value is Record<string, unknown> => typeof value === 'object' && value !== null);
}

function stringField(record: Record<string, unknown>, key: string): string {
    return typeof record[key] === 'string' ? record[key] as string : '';
}

function numberField(record: Record<string, unknown>, key: string): number | undefined {
    return typeof record[key] === 'number' ? record[key] as number : undefined;
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
