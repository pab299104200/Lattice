import { escapeAttr, escapeHtml } from './components/html';
import { ReviewI18n } from './i18n';
import { ReviewRpcBridgeContract } from './rpcBridge';
import { ReviewCountMetric, ReviewIndexingHealth, ReviewParserHealth } from './rpcPayloads';

type IndexingSection = 'pipeline' | 'vector' | 'fts' | 'eventLog';

export interface IndexingHealthViewState {
    inlineError?: string;
    health?: ReviewIndexingHealth;
    refreshingSection?: IndexingSection;
}

export interface IndexingHealthHost {
    readonly state: IndexingHealthViewState;
    remember(health: ReviewIndexingHealth): void;
    reportError?(message: string): void;
    setContent(markup: string): void;
}

export async function mountIndexingHealthView(
    host: IndexingHealthHost,
    bridge: ReviewRpcBridgeContract,
    i18n: ReviewI18n
): Promise<void> {
    try {
        const result = await bridge.getIndexingHealth();
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

function renderView(health: ReviewIndexingHealth, state: IndexingHealthViewState, i18n: ReviewI18n): string {
    const parserRows = health.parserHealth.length
        ? health.parserHealth.map((entry) => renderParserRow(entry, i18n)).join('')
        : `<tr><td colspan="4">${escapeHtml(i18n.t('indexingHealthView.empty'))}</td></tr>`;
    return `<section class="route-stack" data-testid="review-indexing-health">
  <div class="route-toolbar">
    <div>
      <h3>${escapeHtml(i18n.t('indexingHealthView.title'))}</h3>
      <p>${escapeHtml(i18n.t('indexingHealthView.subtitle', { workspace: health.snapshot.workspace }))}</p>
    </div>
  </div>
  ${state.inlineError ? `<div class="banner error" role="alert">${escapeHtml(state.inlineError)}</div>` : ''}
  <div class="summary-grid">
    ${summaryCard(i18n.t('indexingHealthView.summary.status'), labelForStatus(health.snapshot.status, i18n))}
    ${summaryCard(i18n.t('indexingHealthView.summary.nodes'), health.snapshot.nodes.toLocaleString())}
    ${summaryCard(i18n.t('indexingHealthView.summary.edges'), health.snapshot.edges.toLocaleString())}
    ${summaryCard(i18n.t('indexingHealthView.summary.files'), health.snapshot.files.toLocaleString())}
  </div>
  ${renderSection('pipeline', i18n.t('indexingHealthView.sections.pipeline'), renderPipeline(health, parserRows, i18n), state, i18n)}
  ${renderSection('vector', i18n.t('indexingHealthView.sections.vector'), renderMetricList(health.vectorIndex.notes, [
    metricRow(i18n.t('indexingHealthView.vector.totalEmbeddings'), formatNumber(health.vectorIndex.totalEmbeddings, i18n)),
    metricRow(i18n.t('indexingHealthView.vector.lastRebuild'), formatTimestamp(health.vectorIndex.lastRebuildAt, i18n)),
    metricRow(i18n.t('indexingHealthView.vector.averageFreshness'), health.vectorIndex.averageFreshness ?? i18n.t('indexingHealthView.notReported')),
    metricRow(i18n.t('indexingHealthView.vector.staleEmbeddings'), formatNumber(health.vectorIndex.staleEmbeddings, i18n)),
  ], i18n), state, i18n)}
  ${renderSection('fts', i18n.t('indexingHealthView.sections.fts'), renderFts(health.ftsIndex.rows, health.ftsIndex.lastRebuildAt, health.ftsIndex.orphanRows, health.ftsIndex.notes, i18n), state, i18n)}
  ${renderSection('eventLog', i18n.t('indexingHealthView.sections.eventLog'), renderEventLog(health, i18n), state, i18n)}
</section>`;
}

function renderSection(
    section: IndexingSection,
    title: string,
    body: string,
    state: IndexingHealthViewState,
    i18n: ReviewI18n
): string {
    const label = state.refreshingSection === section
        ? i18n.t('indexingHealthView.actions.refreshing')
        : i18n.t('indexingHealthView.actions.refresh');
    return `<section class="panel-block">
  <div class="route-toolbar compact">
    <h4>${escapeHtml(title)}</h4>
    <div class="toolbar-actions">
      <button type="button" data-command="refreshIndexingHealthSection" data-value="${escapeAttr(section)}" ${state.refreshingSection === section ? 'disabled' : ''}>${escapeHtml(label)}</button>
    </div>
  </div>
  ${body}
</section>`;
}

function renderPipeline(health: ReviewIndexingHealth, parserRows: string, i18n: ReviewI18n): string {
    return `<div class="summary-grid">
  ${summaryCard(i18n.t('indexingHealthView.pipeline.watcherStatus'), labelForWatcher(health.watcherStatus, i18n))}
  ${summaryCard(i18n.t('indexingHealthView.pipeline.lastFullRescan'), formatTimestamp(health.lastFullRescanAt, i18n))}
</div>
<div class="table-shell"><table class="data-table" aria-label="${escapeAttr(i18n.t('indexingHealthView.pipeline.tableLabel'))}">
  <thead><tr>
    <th>${escapeHtml(i18n.t('indexingHealthView.pipeline.language'))}</th>
    <th>${escapeHtml(i18n.t('indexingHealthView.pipeline.parsed'))}</th>
    <th>${escapeHtml(i18n.t('indexingHealthView.pipeline.failed'))}</th>
    <th>${escapeHtml(i18n.t('indexingHealthView.pipeline.lastError'))}</th>
  </tr></thead>
  <tbody>${parserRows}</tbody>
</table></div>`;
}

function renderParserRow(entry: ReviewParserHealth, i18n: ReviewI18n): string {
    return `<tr>
  <td>${escapeHtml(entry.language)}</td>
  <td>${escapeHtml(entry.parsedCount.toLocaleString())}</td>
  <td>${escapeHtml(formatNumber(entry.failedCount, i18n))}</td>
  <td>${escapeHtml(entry.lastError ?? i18n.t('indexingHealthView.notReported'))}</td>
</tr>`;
}

function renderFts(
    rows: ReviewCountMetric[],
    lastRebuildAt: string | undefined,
    orphanRows: number | undefined,
    notes: string[],
    i18n: ReviewI18n
): string {
    const renderedRows = rows.length
        ? rows.map((row) => `<tr><td>${escapeHtml(row.label)}</td><td>${escapeHtml(formatNumber(row.count, i18n))}</td><td>${escapeHtml(row.note ?? '')}</td></tr>`).join('')
        : `<tr><td colspan="3">${escapeHtml(i18n.t('indexingHealthView.notReported'))}</td></tr>`;
    return `<div class="summary-grid">
  ${summaryCard(i18n.t('indexingHealthView.fts.lastRebuild'), formatTimestamp(lastRebuildAt, i18n))}
  ${summaryCard(i18n.t('indexingHealthView.fts.orphanRows'), formatNumber(orphanRows, i18n))}
</div>
<div class="table-shell"><table class="data-table" aria-label="${escapeAttr(i18n.t('indexingHealthView.fts.tableLabel'))}">
  <thead><tr>
    <th>${escapeHtml(i18n.t('indexingHealthView.fts.tableName'))}</th>
    <th>${escapeHtml(i18n.t('indexingHealthView.fts.rowCount'))}</th>
    <th>${escapeHtml(i18n.t('indexingHealthView.fts.notes'))}</th>
  </tr></thead>
  <tbody>${renderedRows}</tbody>
</table></div>
${renderNotes(notes, i18n)}`;
}

function renderEventLog(health: ReviewIndexingHealth, i18n: ReviewI18n): string {
    return renderMetricList(health.eventLog.notes, [
        metricRow(i18n.t('indexingHealthView.eventLog.lastHour'), health.eventLog.lastHourCount.toLocaleString()),
        metricRow(i18n.t('indexingHealthView.eventLog.last24Hours'), health.eventLog.last24HoursCount.toLocaleString()),
        metricRow(i18n.t('indexingHealthView.eventLog.last7Days'), health.eventLog.last7DaysCount.toLocaleString()),
        metricRow(i18n.t('indexingHealthView.eventLog.lastCompaction'), formatTimestamp(health.eventLog.lastCompactionAt, i18n)),
        metricRow(i18n.t('indexingHealthView.eventLog.spilloverRows'), health.eventLog.spilloverRows.toLocaleString()),
    ], i18n);
}

function renderMetricList(notes: string[], rows: Array<{ label: string; value: string }>, i18n: ReviewI18n): string {
    return `<div class="list-stack">
  ${rows.map((row) => `<article class="list-card"><strong>${escapeHtml(row.label)}</strong><span>${escapeHtml(row.value)}</span></article>`).join('')}
</div>
${renderNotes(notes, i18n)}`;
}

function renderNotes(notes: string[], i18n: ReviewI18n): string {
    if (!notes.length) {
        return '';
    }
    return `<div class="placeholder compact"><h3>${escapeHtml(i18n.t('indexingHealthView.notesTitle'))}</h3><p>${escapeHtml(notes.join(' '))}</p></div>`;
}

function metricRow(label: string, value: string): { label: string; value: string } {
    return { label, value };
}

function labelForStatus(status: string, i18n: ReviewI18n): string {
    return status === 'indexing' ? i18n.t('indexingHealthView.status.indexing') : i18n.t('indexingHealthView.status.ready');
}

function labelForWatcher(status: string, i18n: ReviewI18n): string {
    return status === 'indexing' ? i18n.t('indexingHealthView.watcher.indexing') : i18n.t('indexingHealthView.watcher.watching');
}

function formatTimestamp(value: string | undefined, i18n: ReviewI18n): string {
    if (!value) {
        return i18n.t('indexingHealthView.notReported');
    }
    const parsed = Date.parse(value);
    return Number.isNaN(parsed) ? value : new Date(parsed).toLocaleString();
}

function formatNumber(value: number | undefined, i18n: ReviewI18n): string {
    return value === undefined ? i18n.t('indexingHealthView.notReported') : value.toLocaleString();
}

function summaryCard(label: string, value: string): string {
    return `<div class="card"><small>${escapeHtml(label)}</small><strong>${escapeHtml(value)}</strong></div>`;
}

function renderError(i18n: ReviewI18n, message: string): string {
    return `<section class="route-stack" data-testid="review-indexing-health"><div class="banner error" role="alert"><strong>${escapeHtml(i18n.t('indexingHealthView.errorTitle'))}</strong><p>${escapeHtml(message)}</p></div></section>`;
}
