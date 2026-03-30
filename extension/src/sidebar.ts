import * as cp from 'child_process';
import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

interface IndexStats {
    nodes: number;
    files: number;
    edges: number;
    repos?: Array<{ name: string; files: number; nodes: number; edges: number }>;
}

interface McpToolContentItem {
    type?: string;
    text?: string;
}

interface McpToolResponse {
    content?: McpToolContentItem[];
}

interface SessionMetricsSnapshot {
    total_tool_calls: number;
    workflow_tool_calls: number;
    total_payload_tokens: number;
    average_payload_tokens_per_tool: number;
    total_payload_bytes: number;
    average_payload_bytes_per_tool: number;
    context_handle_reuses: number;
    context_handle_reuse_rate: number;
    compact_task_count: number;
    tiny_task_count: number;
    widened_task_count: number;
    dense_wire_count: number;
    single_anchor_task_count: number;
    compact_to_expand_count: number;
    compact_to_expand_rate: number;
    follow_up_avoided_count: number;
    follow_up_avoidance_rate: number;
    outcome_memory_reuse_count: number;
}

interface KnowledgePreviewItem {
    symbol: string;
    file: string;
    line: number;
    reasons: string[];
}

interface KnowledgeSnapshot {
    staleDocCount: number;
    changedFileCount: number;
    changedSymbolCount: number;
    source: string;
    docs: KnowledgePreviewItem[];
}

interface StaleDocsToolReport {
    docs?: Array<{
        symbol?: string;
        file?: string;
        line?: number;
        reasons?: string[];
    }>;
    resolved_files?: string[];
    resolved_symbols?: string[];
    stats?: {
        candidate_docs?: number;
    };
}

const EMPTY_METRICS: SessionMetricsSnapshot = {
    total_tool_calls: 0,
    workflow_tool_calls: 0,
    total_payload_tokens: 0,
    average_payload_tokens_per_tool: 0,
    total_payload_bytes: 0,
    average_payload_bytes_per_tool: 0,
    context_handle_reuses: 0,
    context_handle_reuse_rate: 0,
    compact_task_count: 0,
    tiny_task_count: 0,
    widened_task_count: 0,
    dense_wire_count: 0,
    single_anchor_task_count: 0,
    compact_to_expand_count: 0,
    compact_to_expand_rate: 0,
    follow_up_avoided_count: 0,
    follow_up_avoidance_rate: 0,
    outcome_memory_reuse_count: 0,
};

const EMPTY_KNOWLEDGE: KnowledgeSnapshot = {
    staleDocCount: 0,
    changedFileCount: 0,
    changedSymbolCount: 0,
    source: 'Uses staged diff, working tree, or the current editor context.',
    docs: [],
};

export class LatticeSidebarProvider implements vscode.WebviewViewProvider {
    public static readonly viewType = 'lattice.sidebar';

    private webviewView: vscode.WebviewView | undefined;
    private currentStatus: DaemonStatus = 'stopped';
    private stats: IndexStats = { nodes: 0, files: 0, edges: 0 };
    private metrics: SessionMetricsSnapshot = EMPTY_METRICS;
    private knowledge: KnowledgeSnapshot = EMPTY_KNOWLEDGE;
    private disposables: vscode.Disposable[] = [];
    private pollTimer: ReturnType<typeof setInterval> | undefined;

    constructor(private readonly daemon: DaemonManager) {
        this.currentStatus = daemon.getStatus();

        const sub = daemon.onStatusChange((status) => {
            this.currentStatus = status;
            if (status === 'running') {
                this.refreshFromDaemon();
                this.startPolling();
            } else {
                this.stopPolling();
            }
            this.postUpdate();
        });
        this.disposables.push(sub);
    }

    public resolveWebviewView(
        webviewView: vscode.WebviewView,
        _context: vscode.WebviewViewResolveContext,
        _token: vscode.CancellationToken
    ): void {
        this.webviewView = webviewView;

        webviewView.webview.options = {
            enableScripts: true,
        };

        webviewView.webview.onDidReceiveMessage((message) => {
            switch (message.command) {
                case 'reindex':
                    vscode.commands.executeCommand('lattice.reindex');
                    break;
                case 'clearMemory':
                    this.clearMemory();
                    break;
                case 'findStaleDocs':
                    vscode.commands.executeCommand('lattice.findStaleDocs');
                    break;
                case 'docsCapsule':
                    vscode.commands.executeCommand('lattice.getDocsCapsule');
                    break;
                case 'showBacklinks':
                    vscode.commands.executeCommand('lattice.showBacklinks');
                    break;
                case 'showOutgoingLinks':
                    vscode.commands.executeCommand('lattice.showOutgoingLinks');
                    break;
                case 'openDocsWorkbench':
                    vscode.commands.executeCommand('lattice.openDocsWorkbench');
                    break;
            }
        });

        webviewView.onDidDispose(() => {
            this.webviewView = undefined;
        });

        // Refresh status from daemon every time view becomes visible
        webviewView.onDidChangeVisibility(() => {
            if (webviewView.visible) {
                this.refreshFromDaemon();
            }
        });

        this.renderHtml();
        // Fetch fresh data on first render and start polling if indexing
        this.refreshFromDaemon();
        if (this.currentStatus === 'running') {
            this.startPolling();
        }
    }

    /**
     * Query the daemon for current status and stats, then push to webview.
     */
    private async refreshFromDaemon(): Promise<void> {
        this.currentStatus = this.daemon.getStatus();
        if (this.currentStatus === 'running') {
            try {
                const [statusResult, metricsResult, knowledgeResult] = await Promise.all([
                    this.daemon.sendRequest('lattice/status'),
                    this.getSessionMetrics(),
                    this.getKnowledgeSnapshot(),
                ]);
                if (statusResult && typeof statusResult === 'object') {
                    const result = statusResult as any;
                    this.stats = {
                        nodes: result.nodes ?? result.node_count ?? 0,
                        files: result.files ?? result.file_count ?? 0,
                        edges: result.edges ?? result.edge_count ?? 0,
                        repos: result.repos,
                    };
                    // Stop polling once indexing is complete
                    if (result.status !== 'indexing') {
                        this.stopPolling();
                    }
                }
                this.metrics = metricsResult;
                this.knowledge = knowledgeResult;
            } catch {
                // ignore — just use cached stats
            }
        }
        this.postUpdate();
    }

    /**
     * Start polling the daemon for stats updates every 3 seconds.
     * Used during indexing to keep the sidebar current.
     */
    private startPolling(): void {
        if (this.pollTimer) {
            return; // Already polling
        }
        this.pollTimer = setInterval(() => {
            this.refreshFromDaemon();
        }, 3000);
    }

    /**
     * Stop polling when indexing is complete or daemon stops.
     */
    private stopPolling(): void {
        if (this.pollTimer) {
            clearInterval(this.pollTimer);
            this.pollTimer = undefined;
        }
    }

    /**
     * Update displayed stats and refresh the webview.
     */
    public updateStats(stats: IndexStats): void {
        this.stats = stats;
        this.postUpdate();
    }

    /**
     * Force a full re-render of the webview.
     */
    public refresh(): void {
        this.renderHtml();
    }

    private postUpdate(): void {
        if (this.webviewView) {
            this.webviewView.webview.postMessage({
                type: 'update',
                status: this.currentStatus,
                stats: this.stats,
                metrics: this.metrics,
                knowledge: this.knowledge,
            });
        }
    }

    private async getSessionMetrics(): Promise<SessionMetricsSnapshot> {
        const parsed = await this.callToolJson('get_session_metrics', {}, 30_000);
        if (!parsed || typeof parsed !== 'object') {
            return EMPTY_METRICS;
        }

        try {
            return {
                ...EMPTY_METRICS,
                ...(parsed as Partial<SessionMetricsSnapshot>),
            };
        } catch {
            return EMPTY_METRICS;
        }
    }

    private async getKnowledgeSnapshot(): Promise<KnowledgeSnapshot> {
        const editor = vscode.window.activeTextEditor;
        const workspaceRoot = getWorkspaceRoot(editor);
        const focusedSymbol = getFocusedEditorSymbol(editor);
        const activeFile = getActiveEditorFile(editor);

        let files: string[] = [];
        let source = EMPTY_KNOWLEDGE.source;

        if (workspaceRoot) {
            const gitChanged = await getPreferredGitChangedFiles(workspaceRoot).catch(() => undefined);
            if (gitChanged && gitChanged.files.length > 0) {
                files = gitChanged.files;
                source = gitChanged.source;
            }
        }

        if (files.length === 0 && activeFile) {
            files = [activeFile];
            source = `active file ${activeFile}`;
        }

        const symbols = focusedSymbol ? [focusedSymbol] : [];
        if (files.length === 0 && symbols.length === 0) {
            return {
                ...EMPTY_KNOWLEDGE,
                source,
            };
        }

        const parsed = await this.callToolJson('find_stale_docs', {
            files,
            symbols,
            limit: 12,
        }, 30_000).catch(() => undefined) as StaleDocsToolReport | undefined;

        if (!parsed || typeof parsed !== 'object') {
            return {
                ...EMPTY_KNOWLEDGE,
                source,
            };
        }

        const docs = Array.isArray(parsed.docs)
            ? parsed.docs.slice(0, 3).map((doc) => ({
                symbol: typeof doc.symbol === 'string' ? doc.symbol : 'Untitled',
                file: typeof doc.file === 'string' ? doc.file : '',
                line: typeof doc.line === 'number' ? doc.line : 1,
                reasons: Array.isArray(doc.reasons)
                    ? doc.reasons.filter((reason): reason is string => typeof reason === 'string').slice(0, 2)
                    : [],
            }))
            : [];

        return {
            staleDocCount: parsed.stats?.candidate_docs ?? (Array.isArray(parsed.docs) ? parsed.docs.length : 0),
            changedFileCount: Array.isArray(parsed.resolved_files) ? parsed.resolved_files.length : files.length,
            changedSymbolCount: Array.isArray(parsed.resolved_symbols) ? parsed.resolved_symbols.length : symbols.length,
            source,
            docs,
        };
    }

    private async callToolJson(
        name: string,
        args: Record<string, unknown>,
        timeoutMs = 30_000
    ): Promise<unknown> {
        const response = await this.daemon.sendRequest('tools/call', {
            name,
            arguments: args,
        }, timeoutMs) as McpToolResponse;

        const text = response.content?.find((item) => item.type === 'text')?.text;
        if (typeof text !== 'string') {
            return undefined;
        }

        try {
            return JSON.parse(text) as unknown;
        } catch {
            return undefined;
        }
    }

    private async clearMemory(): Promise<void> {
        if (this.daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }
        try {
            await this.daemon.sendRequest('lattice/clear');
            vscode.window.showInformationMessage('Lattice: Memory cleared');
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Clear memory failed — ${msg}`);
        }
    }

    private renderHtml(): void {
        if (!this.webviewView) {
            return;
        }

        this.webviewView.webview.html = this.getHtml();
        // Send initial data after render — use increasing delays to catch webview readiness
        setTimeout(() => this.postUpdate(), 200);
        setTimeout(() => this.postUpdate(), 500);
        setTimeout(() => this.postUpdate(), 1500);
    }

    private getHtml(): string {
        return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <style>
        body {
            font-family: var(--vscode-font-family);
            font-size: var(--vscode-font-size);
            color: var(--vscode-foreground);
            padding: 12px;
            margin: 0;
        }
        .section {
            margin-bottom: 16px;
        }
        .section-title {
            font-weight: 600;
            font-size: 11px;
            text-transform: uppercase;
            letter-spacing: 0.5px;
            color: var(--vscode-descriptionForeground);
            margin-bottom: 8px;
        }
        .status-row {
            display: flex;
            align-items: center;
            gap: 8px;
            margin-bottom: 4px;
        }
        .status-dot {
            width: 8px;
            height: 8px;
            border-radius: 50%;
            flex-shrink: 0;
        }
        .status-dot.running { background-color: #4ec9b0; }
        .status-dot.starting { background-color: #dcdcaa; }
        .status-dot.stopped { background-color: #808080; }
        .status-dot.error { background-color: #f44747; }
        .status-label {
            font-size: 13px;
        }
        .stats-grid {
            display: grid;
            grid-template-columns: 1fr 1fr 1fr;
            gap: 8px;
        }
        .stat-card {
            background: var(--vscode-editor-background);
            border: 1px solid var(--vscode-widget-border, #333);
            border-radius: 4px;
            padding: 8px;
            text-align: center;
        }
        .stat-value {
            font-size: 18px;
            font-weight: 600;
            color: var(--vscode-foreground);
        }
        .stat-label {
            font-size: 10px;
            text-transform: uppercase;
            color: var(--vscode-descriptionForeground);
            margin-top: 2px;
        }
        .metrics-grid {
            display: grid;
            grid-template-columns: repeat(3, 1fr);
            gap: 8px;
        }
        .metric-card {
            background: var(--vscode-editor-background);
            border: 1px solid var(--vscode-widget-border, #333);
            border-radius: 4px;
            padding: 8px;
        }
        .metric-value {
            font-size: 16px;
            font-weight: 600;
            color: var(--vscode-foreground);
        }
        .metric-label {
            font-size: 10px;
            text-transform: uppercase;
            color: var(--vscode-descriptionForeground);
            margin-top: 2px;
        }
        .metric-note {
            margin-top: 8px;
            font-size: 11px;
            color: var(--vscode-descriptionForeground);
            line-height: 1.4;
        }
        .metric-list {
            margin-top: 10px;
            display: flex;
            flex-direction: column;
            gap: 6px;
        }
        .metric-row {
            display: flex;
            justify-content: space-between;
            gap: 12px;
            font-size: 12px;
            line-height: 1.35;
        }
        .metric-row span:first-child {
            color: var(--vscode-descriptionForeground);
        }
        .metric-row span:last-child {
            text-align: right;
            color: var(--vscode-foreground);
        }
        .actions {
            display: flex;
            flex-direction: column;
            gap: 8px;
        }
        button {
            width: 100%;
            padding: 9px 12px;
            border: 1px solid var(--vscode-widget-border, #333);
            border-radius: 6px;
            background: var(--vscode-button-secondaryBackground, var(--vscode-editor-background));
            color: var(--vscode-button-secondaryForeground, var(--vscode-foreground));
            font-family: var(--vscode-font-family);
            font-size: 13px;
            line-height: 1.2;
            text-align: center;
            cursor: pointer;
        }
        button:hover {
            background: var(--vscode-button-secondaryHoverBackground, var(--vscode-list-hoverBackground));
            border-color: var(--vscode-focusBorder, #007acc);
        }
        button.primary {
            background: var(--vscode-button-background, #0e639c);
            color: var(--vscode-button-foreground, #fff);
            border-color: transparent;
        }
        button.primary:hover {
            background: var(--vscode-button-hoverBackground, #1177bb);
        }
        button.utility {
            background: transparent;
            color: var(--vscode-descriptionForeground);
        }
        button.utility:hover {
            background: var(--vscode-list-hoverBackground, rgba(255, 255, 255, 0.06));
            color: var(--vscode-foreground);
        }
        .preview-list {
            margin-top: 10px;
            display: flex;
            flex-direction: column;
            gap: 8px;
        }
        .preview-item {
            background: var(--vscode-editor-background);
            border: 1px solid var(--vscode-widget-border, #333);
            border-radius: 6px;
            padding: 8px;
        }
        .preview-title {
            font-size: 12px;
            font-weight: 600;
            color: var(--vscode-foreground);
            line-height: 1.35;
        }
        .preview-meta {
            margin-top: 4px;
            font-size: 11px;
            color: var(--vscode-descriptionForeground);
            line-height: 1.4;
        }
    </style>
</head>
<body>
    <div class="section">
        <div class="section-title">Status</div>
        <div class="status-row">
            <div id="statusDot" class="status-dot stopped"></div>
            <span id="statusLabel" class="status-label">Stopped</span>
        </div>
    </div>

    <div class="section">
        <div class="section-title">Index Statistics</div>
        <div class="stats-grid">
            <div class="stat-card">
                <div id="nodeCount" class="stat-value">0</div>
                <div class="stat-label">Nodes</div>
            </div>
            <div class="stat-card">
                <div id="fileCount" class="stat-value">0</div>
                <div class="stat-label">Files</div>
            </div>
            <div class="stat-card">
                <div id="edgeCount" class="stat-value">0</div>
                <div class="stat-label">Edges</div>
            </div>
        </div>
    </div>

    <div id="reposSection" class="section" style="display:none">
        <div class="section-title">Repositories</div>
        <div id="reposList"></div>
    </div>

    <div class="section">
        <div class="section-title">Knowledge Freshness</div>
        <div class="stats-grid">
            <div class="stat-card">
                <div id="staleDocCount" class="stat-value">0</div>
                <div class="stat-label">Stale Docs</div>
            </div>
            <div class="stat-card">
                <div id="changedFileCount" class="stat-value">0</div>
                <div class="stat-label">Changed Files</div>
            </div>
            <div class="stat-card">
                <div id="changedSymbolCount" class="stat-value">0</div>
                <div class="stat-label">Changed Symbols</div>
            </div>
        </div>
        <div id="knowledgeSource" class="metric-note">Uses staged diff, working tree, or the current editor context.</div>
        <div id="knowledgeEmpty" class="metric-note">No stale docs detected for the current change set.</div>
        <div id="knowledgeList" class="preview-list" style="display:none"></div>
        <div class="actions" style="margin-top:10px">
            <button class="primary" onclick="openDocsWorkbench()">Open Docs Graph</button>
            <button onclick="findStaleDocs()">Find Stale Docs</button>
            <button onclick="docsCapsule()">Docs Capsule</button>
            <button class="utility" onclick="showBacklinks()">Backlinks Here</button>
            <button class="utility" onclick="showOutgoingLinks()">Outgoing Links Here</button>
        </div>
    </div>

    <div class="section">
        <div class="section-title">Agent Efficiency</div>
        <div class="metrics-grid">
            <div class="metric-card">
                <div id="avgTokens" class="metric-value">0</div>
                <div class="metric-label">Avg Tokens/Tool</div>
            </div>
            <div class="metric-card">
                <div id="totalTokens" class="metric-value">0</div>
                <div class="metric-label">Session Tokens</div>
            </div>
            <div class="metric-card">
                <div id="followUpAvoided" class="metric-value">0%</div>
                <div class="metric-label">Follow-ups Avoided</div>
            </div>
        </div>
        <div id="metricsEmpty" class="metric-note">Run a few workflow tools to populate assistant-efficiency metrics.</div>
        <div id="metricsDetails" class="metric-list" style="display:none">
            <div class="metric-row">
                <span>Delivery mix</span>
                <span id="deliveryMix">0 tiny · 0 compact · 0 full</span>
            </div>
            <div class="metric-row">
                <span>Dense / single-anchor</span>
                <span id="denseSingle">0 dense · 0 single</span>
            </div>
            <div class="metric-row">
                <span>Expand rate</span>
                <span id="expandRate">0 expands · 0%</span>
            </div>
            <div class="metric-row">
                <span>Handle reuse</span>
                <span id="handleReuse">0 reuses · 0%</span>
            </div>
            <div class="metric-row">
                <span>Outcome memory reuse</span>
                <span id="outcomeReuse">0</span>
            </div>
        </div>
    </div>

        <div class="section">
        <div class="section-title">Maintenance</div>
        <div class="actions">
            <button id="reindexBtn" class="utility" onclick="reindex()">Re-index Workspace</button>
            <button id="clearBtn" class="utility" onclick="clearMemory()">Clear Memory</button>
        </div>
    </div>

    <script>
        const vscode = acquireVsCodeApi();

        const statusDot = document.getElementById('statusDot');
        const statusLabel = document.getElementById('statusLabel');
        const nodeCount = document.getElementById('nodeCount');
        const fileCount = document.getElementById('fileCount');
        const edgeCount = document.getElementById('edgeCount');
        const avgTokens = document.getElementById('avgTokens');
        const totalTokens = document.getElementById('totalTokens');
        const followUpAvoided = document.getElementById('followUpAvoided');
        const metricsEmpty = document.getElementById('metricsEmpty');
        const metricsDetails = document.getElementById('metricsDetails');
        const deliveryMix = document.getElementById('deliveryMix');
        const denseSingle = document.getElementById('denseSingle');
        const expandRate = document.getElementById('expandRate');
        const handleReuse = document.getElementById('handleReuse');
        const outcomeReuse = document.getElementById('outcomeReuse');
        const staleDocCount = document.getElementById('staleDocCount');
        const changedFileCount = document.getElementById('changedFileCount');
        const changedSymbolCount = document.getElementById('changedSymbolCount');
        const knowledgeSource = document.getElementById('knowledgeSource');
        const knowledgeEmpty = document.getElementById('knowledgeEmpty');
        const knowledgeList = document.getElementById('knowledgeList');

        const statusLabels = {
            running: 'Running',
            starting: 'Starting...',
            stopped: 'Stopped',
            error: 'Error'
        };

        function percent(rate) {
            return Math.round((rate || 0) * 100) + '%';
        }

        function escapeHtml(str) {
            const div = document.createElement('div');
            div.textContent = str;
            return div.innerHTML;
        }

        window.addEventListener('message', (event) => {
            const message = event.data;
            if (message.type === 'update') {
                // Update status
                statusDot.className = 'status-dot ' + message.status;
                statusLabel.textContent = statusLabels[message.status] || message.status;

                // Update stats
                if (message.stats) {
                    nodeCount.textContent = message.stats.nodes.toLocaleString();
                    fileCount.textContent = message.stats.files.toLocaleString();
                    edgeCount.textContent = message.stats.edges.toLocaleString();

                    // Update per-repo cards if available
                    const reposSection = document.getElementById('reposSection');
                    const reposList = document.getElementById('reposList');
                    if (message.stats.repos && message.stats.repos.length > 0) {
                        reposSection.style.display = 'block';
                        reposList.innerHTML = message.stats.repos.map(r =>
                            '<div class="stat-card" style="margin-bottom:6px;text-align:left;padding:6px 8px">' +
                            '<strong>' + escapeHtml(r.name) + '</strong><br>' +
                            '<span style="font-size:11px;color:var(--vscode-descriptionForeground)">' +
                            escapeHtml(String(r.nodes)) + ' nodes · ' + escapeHtml(String(r.files)) + ' files · ' + escapeHtml(String(r.edges)) + ' edges</span></div>'
                        ).join('');
                    } else {
                        reposSection.style.display = 'none';
                    }
                }

                const metrics = message.metrics || {};
                const totalToolCalls = metrics.total_tool_calls || 0;
                avgTokens.textContent = (metrics.average_payload_tokens_per_tool || 0).toLocaleString();
                totalTokens.textContent = (metrics.total_payload_tokens || 0).toLocaleString();
                followUpAvoided.textContent = percent(metrics.follow_up_avoidance_rate || 0);

                if (totalToolCalls > 0) {
                    const tinyCount = metrics.tiny_task_count || 0;
                    const compactCount = Math.max((metrics.compact_task_count || 0) - tinyCount, 0);
                    const fullCount = metrics.widened_task_count || 0;
                    metricsEmpty.style.display = 'none';
                    metricsDetails.style.display = 'flex';
                    deliveryMix.textContent = tinyCount + ' tiny · ' + compactCount + ' compact · ' + fullCount + ' full';
                    denseSingle.textContent = (metrics.dense_wire_count || 0) + ' dense · ' + (metrics.single_anchor_task_count || 0) + ' single';
                    expandRate.textContent = (metrics.compact_to_expand_count || 0) + ' expands · ' + percent(metrics.compact_to_expand_rate || 0);
                    handleReuse.textContent = (metrics.context_handle_reuses || 0) + ' reuses · ' + percent(metrics.context_handle_reuse_rate || 0);
                    outcomeReuse.textContent = (metrics.outcome_memory_reuse_count || 0).toLocaleString();
                } else {
                    metricsEmpty.style.display = 'block';
                    metricsDetails.style.display = 'none';
                }

                const knowledge = message.knowledge || {};
                staleDocCount.textContent = (knowledge.staleDocCount || 0).toLocaleString();
                changedFileCount.textContent = (knowledge.changedFileCount || 0).toLocaleString();
                changedSymbolCount.textContent = (knowledge.changedSymbolCount || 0).toLocaleString();
                knowledgeSource.textContent = knowledge.source || 'Uses staged diff, working tree, or the current editor context.';

                const docs = Array.isArray(knowledge.docs) ? knowledge.docs : [];
                if (docs.length > 0) {
                    knowledgeEmpty.style.display = 'none';
                    knowledgeList.style.display = 'flex';
                    knowledgeList.innerHTML = docs.map(doc => {
                        const reasons = Array.isArray(doc.reasons) ? doc.reasons.map(reason => escapeHtml(reason)).join(' · ') : '';
                        const location = escapeHtml((doc.file || '') + ':' + String(doc.line || 1));
                        return '<div class="preview-item">' +
                            '<div class="preview-title">' + escapeHtml(doc.symbol || 'Untitled') + '</div>' +
                            '<div class="preview-meta">' + location + '</div>' +
                            (reasons ? '<div class="preview-meta">' + reasons + '</div>' : '') +
                            '</div>';
                    }).join('');
                } else {
                    knowledgeList.style.display = 'none';
                    knowledgeEmpty.style.display = 'block';
                    knowledgeEmpty.textContent = (knowledge.changedFileCount || knowledge.changedSymbolCount)
                        ? 'No stale docs detected for the current change set.'
                        : 'Open a file or stage a diff to check doc freshness.';
                }
            }
        });

        function reindex() {
            vscode.postMessage({ command: 'reindex' });
        }

        function clearMemory() {
            vscode.postMessage({ command: 'clearMemory' });
        }

        function findStaleDocs() {
            vscode.postMessage({ command: 'findStaleDocs' });
        }

        function docsCapsule() {
            vscode.postMessage({ command: 'docsCapsule' });
        }

        function showBacklinks() {
            vscode.postMessage({ command: 'showBacklinks' });
        }

        function showOutgoingLinks() {
            vscode.postMessage({ command: 'showOutgoingLinks' });
        }

        function openDocsWorkbench() {
            vscode.postMessage({ command: 'openDocsWorkbench' });
        }
    </script>
</body>
</html>`;
    }

    public dispose(): void {
        this.stopPolling();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}

function getActiveEditorFile(editor: vscode.TextEditor | undefined): string | undefined {
    if (!editor || editor.document.uri.scheme !== 'file') {
        return undefined;
    }

    return toRelativeWorkspacePath(editor.document.uri);
}

function getFocusedEditorSymbol(editor: vscode.TextEditor | undefined): string | undefined {
    if (!editor) {
        return undefined;
    }

    const selectionText = editor.document.getText(editor.selection).trim();
    if (selectionText && isCompactSymbol(selectionText)) {
        return selectionText;
    }

    const wordRange = editor.document.getWordRangeAtPosition(editor.selection.active);
    if (!wordRange) {
        return undefined;
    }

    const word = editor.document.getText(wordRange).trim();
    return isCompactSymbol(word) ? word : undefined;
}

function isCompactSymbol(value: string): boolean {
    return value.length >= 2 && value.length <= 120 && /^[A-Za-z_][\w.$:-]*$/.test(value);
}

function getWorkspaceRoot(editor: vscode.TextEditor | undefined): string | undefined {
    const folder = editor ? vscode.workspace.getWorkspaceFolder(editor.document.uri) : undefined;
    return folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

function toRelativeWorkspacePath(uri: vscode.Uri): string {
    return vscode.workspace.asRelativePath(uri, false).replace(/\\/g, '/');
}

async function getPreferredGitChangedFiles(cwd: string): Promise<{ files: string[]; source: string }> {
    const stagedFiles = await runExecFile('git', ['diff', '--cached', '--name-only'], cwd)
        .catch(() => '');
    const parsedStaged = parseGitChangedFiles(stagedFiles);
    if (parsedStaged.length > 0) {
        return { files: parsedStaged, source: 'staged diff' };
    }

    const workingFiles = await runExecFile('git', ['diff', '--name-only'], cwd)
        .catch(() => '');
    const parsedWorking = parseGitChangedFiles(workingFiles);
    if (parsedWorking.length > 0) {
        return { files: parsedWorking, source: 'working tree diff' };
    }

    throw new Error('No staged or working tree diff found');
}

function parseGitChangedFiles(stdout: string): string[] {
    const seen = new Set<string>();
    const files: string[] = [];

    for (const line of stdout.split(/\r?\n/)) {
        const trimmed = line.trim().replace(/\\/g, '/');
        if (!trimmed || seen.has(trimmed)) {
            continue;
        }

        seen.add(trimmed);
        files.push(trimmed);
    }

    return files;
}

function runExecFile(command: string, args: string[], cwd: string): Promise<string> {
    return new Promise((resolve, reject) => {
        cp.execFile(
            command,
            args,
            {
                cwd,
                encoding: 'utf8',
                maxBuffer: 2 * 1024 * 1024,
            },
            (error, stdout, stderr) => {
                if (error) {
                    reject(new Error(stderr.trim() || error.message));
                    return;
                }

                resolve(stdout);
            }
        );
    });
}
