import * as crypto from 'crypto';
import * as vscode from 'vscode';
import { ReviewI18n } from './i18n';

export function renderReviewPanelHtml(webview: vscode.Webview, i18n: ReviewI18n): string {
    const nonce = crypto.randomBytes(16).toString('hex');
    const bootstrap = JSON.stringify({
        title: i18n.t('reviewPanel.title'),
        subtitle: i18n.t('reviewPanel.subtitle'),
        loading: i18n.t('reviewPanel.loading'),
        empty: i18n.t('reviewPanel.empty'),
        error: i18n.t('reviewPanel.error'),
        refresh: i18n.t('reviewPanel.refresh'),
        retry: i18n.t('reviewPanel.retry'),
        overview: i18n.t('reviewPanel.overview'),
        overviewDescription: i18n.t('reviewPanel.overviewDescription'),
        routePlaceholder: i18n.t('reviewPanel.routePlaceholder'),
        routeUnavailable: i18n.t('reviewPanel.routeUnavailable'),
        capabilityDirect: i18n.t('reviewPanel.capabilityDirect'),
        capabilityComposed: i18n.t('reviewPanel.capabilityComposed'),
        capabilityUnavailable: i18n.t('reviewPanel.capabilityUnavailable'),
        summaryIndexing: i18n.t('reviewPanel.summaryIndexing'),
        summaryMemories: i18n.t('reviewPanel.summaryMemories'),
        summaryMetrics: i18n.t('reviewPanel.summaryMetrics'),
        summaryGraph: i18n.t('reviewPanel.summaryGraph'),
        daemonStatus: i18n.t('reviewOverview.daemonStatus'),
        memoryCount: i18n.t('reviewOverview.memoryCount'),
        signalCount: i18n.t('reviewOverview.signalCount'),
        nodeCount: i18n.t('reviewOverview.nodeCount'),
        fileCount: i18n.t('reviewOverview.fileCount'),
        edgeCount: i18n.t('reviewOverview.edgeCount'),
        languages: i18n.t('reviewOverview.languages'),
        none: i18n.t('reviewOverview.none'),
        unknown: i18n.t('reviewOverview.unknown'),
        routeSupport: i18n.t('reviewOverview.routeSupport'),
    });
    const csp = [
        "default-src 'none'",
        `img-src ${webview.cspSource} data:`,
        `style-src ${webview.cspSource} 'unsafe-inline'`,
        `script-src 'nonce-${nonce}'`,
    ].join('; ');

    return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="${csp}" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>${escapeHtml(i18n.t('reviewPanel.title'))}</title>
  <style>
    :root {
      color-scheme: light dark;
      --border: color-mix(in srgb, var(--vscode-panel-border) 72%, transparent);
      --muted: var(--vscode-descriptionForeground);
      --surface: color-mix(in srgb, var(--vscode-editor-background) 92%, var(--vscode-sideBar-background));
      --surface-alt: color-mix(in srgb, var(--vscode-editor-background) 84%, var(--vscode-sideBar-background));
      --accent: var(--vscode-button-background);
      --accent-foreground: var(--vscode-button-foreground);
      --warning: var(--vscode-editorWarning-foreground);
      --error: var(--vscode-errorForeground);
      --success: var(--vscode-testing-iconPassed);
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: var(--vscode-font-family);
      color: var(--vscode-foreground);
      background: radial-gradient(circle at top right, color-mix(in srgb, var(--accent) 12%, transparent), transparent 42%),
        linear-gradient(180deg, var(--surface), var(--vscode-editor-background));
    }
    button, input, textarea, select { font: inherit; color: inherit; }
    button {
      border: 1px solid var(--border);
      background: var(--surface-alt);
      color: inherit;
      border-radius: 10px;
      padding: 8px 12px;
      cursor: pointer;
    }
    button:hover { border-color: var(--accent); }
    button[disabled] { opacity: 0.6; cursor: default; }
    .shell { display: grid; grid-template-columns: 220px 1fr; min-height: 100vh; }
    .nav {
      border-right: 1px solid var(--border);
      padding: 16px 12px;
      background: color-mix(in srgb, var(--surface-alt) 84%, transparent);
    }
    .nav h1 { font-size: 18px; margin: 0; }
    .nav p {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.4;
      margin: 8px 0 16px;
    }
    .nav-list { display: flex; flex-direction: column; gap: 6px; }
    .nav-list button { text-align: left; width: 100%; }
    .nav-list button.active {
      background: var(--accent);
      color: var(--accent-foreground);
      border-color: transparent;
    }
    .content { padding: 18px; display: flex; flex-direction: column; gap: 16px; }
    .toolbar { display: flex; justify-content: space-between; align-items: center; gap: 12px; }
    .toolbar h2 { margin: 0; font-size: 18px; }
    .toolbar p { margin: 4px 0 0; color: var(--muted); font-size: 12px; }
    .summary-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(160px, 1fr)); gap: 12px; }
    .card, .placeholder, .banner, .table-card {
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 14px;
      background: color-mix(in srgb, var(--surface-alt) 72%, transparent);
    }
    .card small, .placeholder small {
      display: block;
      color: var(--muted);
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.04em;
      font-size: 11px;
    }
    .card strong { display: block; font-size: 22px; line-height: 1.2; }
    .banner.error {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 10%, transparent);
    }
    .placeholder h3 { margin: 0 0 8px; font-size: 16px; }
    .placeholder p { margin: 0 0 12px; color: var(--muted); line-height: 1.5; }
    .support {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 8px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 12px;
    }
    .support.direct { border-color: color-mix(in srgb, var(--success) 50%, var(--border)); }
    .support.composed { border-color: color-mix(in srgb, var(--warning) 50%, var(--border)); }
    .support.unsupported { border-color: color-mix(in srgb, var(--error) 50%, var(--border)); }
    .filter-bar, .filter-summary, .pagination-bar, .pagination-controls, .page-size-group, .row-actions {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      align-items: center;
    }
    .filter-summary { margin-top: 10px; color: var(--muted); font-size: 12px; }
    .table-card { display: flex; flex-direction: column; gap: 14px; }
    .table-wrap { overflow-x: auto; }
    .review-table, .data-table { width: 100%; border-collapse: collapse; }
    .review-table th, .review-table td, .data-table th, .data-table td {
      text-align: left;
      padding: 10px 12px;
      border-bottom: 1px solid var(--border);
      vertical-align: top;
      font-size: 12px;
    }
    .review-table tbody tr, .row-link { cursor: pointer; }
    .review-table tbody tr:hover, .row-link:hover {
      background: color-mix(in srgb, var(--accent) 8%, transparent);
    }
    .sort-button, .link-button, .table-sort {
      border: 0;
      background: transparent;
      padding: 0;
      color: inherit;
      border-radius: 0;
    }
    .sort-button:hover, .link-button:hover, .table-sort:hover {
      color: var(--accent);
      border-color: transparent;
    }
    .content-cell { max-width: 420px; line-height: 1.5; }
    .route-stack, .panel-block, .cell-stack, .list-stack { display: flex; flex-direction: column; gap: 12px; }
    .route-toolbar { display: flex; justify-content: space-between; align-items: flex-start; gap: 12px; }
    .route-toolbar h3, .panel-block h4 { margin: 0; }
    .route-toolbar p, .panel-block p { margin: 0; color: var(--muted); line-height: 1.5; }
    .toolbar-actions { display: flex; flex-wrap: wrap; gap: 8px; }
    .filter-pill {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 10px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: color-mix(in srgb, var(--surface) 86%, transparent);
      font-size: 12px;
    }
    .table-shell {
      overflow-x: auto;
      border: 1px solid var(--border);
      border-radius: 12px;
      background: color-mix(in srgb, var(--surface) 88%, transparent);
    }
    .data-table { min-width: 860px; }
    .status-badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 3px 9px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 11px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: 0.04em;
    }
    .status-badge--success {
      border-color: color-mix(in srgb, var(--success) 64%, var(--border));
      background: color-mix(in srgb, var(--success) 12%, transparent);
    }
    .status-badge--warning {
      border-color: color-mix(in srgb, var(--warning) 64%, var(--border));
      background: color-mix(in srgb, var(--warning) 12%, transparent);
    }
    .status-badge--error {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 12%, transparent);
    }
    .status-badge--info {
      border-color: color-mix(in srgb, var(--accent) 64%, var(--border));
      background: color-mix(in srgb, var(--accent) 12%, transparent);
    }
    .status-badge--neutral { background: color-mix(in srgb, var(--surface) 86%, transparent); }
    .inline-error { color: var(--error); font-size: 12px; line-height: 1.4; }
    .panel-block {
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 14px;
      background: color-mix(in srgb, var(--surface-alt) 72%, transparent);
    }
    .list-card {
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 12px;
      display: flex;
      flex-direction: column;
      gap: 6px;
      background: color-mix(in srgb, var(--surface) 90%, transparent);
    }
    .list-card small { color: var(--muted); }
    .placeholder.compact { padding: 16px; }
    .empty-state {
      border: 1px dashed var(--border);
      border-radius: 12px;
      padding: 20px;
      text-align: center;
    }
    .empty-state h3 { margin: 0 0 8px; font-size: 16px; }
    .empty-state p { margin: 0; color: var(--muted); }
    .pagination-bar { justify-content: space-between; color: var(--muted); font-size: 12px; }
    .page-size-group button.active {
      background: var(--accent);
      color: var(--accent-foreground);
      border-color: transparent;
    }
    .sr-only {
      position: absolute;
      width: 1px;
      height: 1px;
      padding: 0;
      margin: -1px;
      overflow: hidden;
      clip: rect(0, 0, 0, 0);
      border: 0;
      white-space: nowrap;
    }
    .kv { display: grid; grid-template-columns: repeat(auto-fit, minmax(180px, 1fr)); gap: 10px; }
    .kv div {
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 10px 12px;
      background: color-mix(in srgb, var(--surface) 86%, transparent);
    }
    .kv span { display: block; color: var(--muted); font-size: 11px; margin-bottom: 6px; }
    .skeleton {
      height: 76px;
      border-radius: 14px;
      border: 1px solid var(--border);
      background: linear-gradient(90deg, transparent, color-mix(in srgb, var(--accent) 14%, transparent), transparent);
      background-size: 240px 100%;
      animation: shimmer 1.2s infinite linear;
    }
    @keyframes shimmer { from { background-position: -240px 0; } to { background-position: 240px 0; } }
    @media (max-width: 900px) {
      .shell { grid-template-columns: 1fr; }
      .nav { border-right: 0; border-bottom: 1px solid var(--border); }
    }
  </style>
</head>
<body>
  <div id="app"></div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const copy = ${bootstrap};
    const state = { loading: true, error: '', payload: undefined };

    function root() { return document.getElementById('app'); }
    function render() {
      const host = root();
      if (!host) { return; }
      if (state.loading) {
        host.innerHTML = '<div class="content"><div class="toolbar"><div><h2>' + escape(copy.loading) + '</h2></div></div><div class="summary-grid"><div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div></div></div>';
        return;
      }
      if (state.error) {
        host.innerHTML = '<div class="content"><div class="banner error" role="alert">' + escape(state.error) + '</div><button data-command="refresh">' + escape(copy.retry) + '</button></div>';
        return;
      }
      if (!state.payload) {
        host.innerHTML = '<div class="content"><div class="placeholder"><h3>' + escape(copy.empty) + '</h3></div></div>';
        return;
      }
      host.innerHTML = renderShell(state.payload);
    }
    function renderShell(payload) {
      const routeCards = payload.routes.map((route) => {
        const active = payload.activeRoute === route.id ? 'active' : '';
        return '<button class="' + active + '" data-command="navigate" data-route="' + escapeAttr(route.id) + '">' + escape(route.label) + '</button>';
      }).join('');
      return '<div class="shell"><aside class="nav"><h1>' + escape(copy.title) + '</h1><p>' + escape(copy.subtitle) + '</p><div class="nav-list">' + routeCards + '</div></aside><main class="content">' + renderToolbar(payload) + renderSummary(payload.overview) + renderRoute(payload) + '</main></div>';
    }
    function renderToolbar(payload) {
      const route = payload.routes.find((entry) => entry.id === payload.activeRoute);
      const description = route ? route.description : copy.overviewDescription;
      const title = route ? route.label : copy.overview;
      return '<div class="toolbar"><div><h2>' + escape(title) + '</h2><p>' + escape(description) + '</p></div><button data-command="refresh">' + escape(copy.refresh) + '</button></div>';
    }
    function renderSummary(overview) {
      const languages = Object.keys(overview.indexStatus.languages || {}).join(', ') || copy.none;
      return '<section class="summary-grid" aria-label="' + escapeAttr(copy.overview) + '">' +
        summaryCard(copy.summaryIndexing, overview.indexStatus.status || copy.unknown) +
        summaryCard(copy.summaryMemories, String(overview.memoryList.count || 0)) +
        summaryCard(copy.summaryMetrics, String((overview.metrics.signals || []).length)) +
        summaryCard(copy.summaryGraph, String(overview.indexStatus.nodes || 0)) +
        '</section><section class="kv">' +
        kvItem(copy.daemonStatus, overview.indexStatus.status || copy.unknown) +
        kvItem(copy.memoryCount, String(overview.memoryList.count || 0)) +
        kvItem(copy.signalCount, String((overview.metrics.signals || []).length)) +
        kvItem(copy.nodeCount, String(overview.indexStatus.nodes || 0)) +
        kvItem(copy.fileCount, String(overview.indexStatus.files || 0)) +
        kvItem(copy.edgeCount, String(overview.indexStatus.edges || 0)) +
        kvItem(copy.languages, languages) +
        '</section>';
    }
    function renderRoute(payload) {
      if (payload.routeView && payload.routeView.html) { return payload.routeView.html; }
      const route = payload.routes.find((entry) => entry.id === payload.activeRoute);
      const support = payload.capabilities[payload.activeRoute];
      const supportLabel = support.mode === 'direct' ? copy.capabilityDirect : support.mode === 'composed' ? copy.capabilityComposed : copy.capabilityUnavailable;
      return '<section class="placeholder" data-testid="review-route-' + escapeAttr(payload.activeRoute) + '"><div class="support ' + escapeAttr(support.mode) + '">' + escape(copy.routeSupport) + ': ' + escape(supportLabel) + '</div><h3>' + escape(route ? route.label : copy.overview) + '</h3><p>' + escape(route ? route.description : copy.overviewDescription) + '</p><p>' + escape(support.mode === 'unsupported' ? copy.routeUnavailable : copy.routePlaceholder) + '</p><small>' + escape(support.reason) + '</small></section>';
    }
    function summaryCard(label, value) { return '<div class="card"><small>' + escape(label) + '</small><strong>' + escape(value) + '</strong></div>'; }
    function kvItem(label, value) { return '<div><span>' + escape(label) + '</span><strong>' + escape(value) + '</strong></div>'; }
    function escape(value) { return String(value).replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;').replaceAll("'", '&#39;'); }
    function escapeAttr(value) { return escape(value); }
    document.addEventListener('click', (event) => {
      const target = event.target;
      if (!(target instanceof Element)) { return; }
      const action = target.closest('[data-command]');
      if (!action) { return; }
      const command = action.getAttribute('data-command');
      vscode.postMessage({
        command,
        route: action.getAttribute('data-route'),
        filterKey: action.getAttribute('data-filter-key'),
        sortKey: action.getAttribute('data-sort-key'),
        sortBy: action.getAttribute('data-sort-by'),
        memoryId: action.getAttribute('data-memory-id'),
        value: action.getAttribute('data-value'),
        eventId: action.getAttribute('data-event-id'),
        page: numberOrUndefined(action.getAttribute('data-page')),
        pageSize: numberOrUndefined(action.getAttribute('data-page-size')),
      });
    });
    function numberOrUndefined(value) {
      if (value === null || value === '') { return undefined; }
      const parsed = Number(value);
      return Number.isFinite(parsed) ? parsed : undefined;
    }
    window.addEventListener('message', (event) => {
      const message = event.data;
      if (message.type === 'state') {
        state.loading = false;
        state.error = '';
        state.payload = message;
        render();
        return;
      }
      if (message.type === 'error') {
        state.loading = false;
        state.error = message.message || copy.error;
        render();
      }
    });
    render();
    vscode.postMessage({ command: 'ready' });
  </script>
</body>
</html>`;
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
