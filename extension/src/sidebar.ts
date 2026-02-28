import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

interface IndexStats {
    nodes: number;
    files: number;
    edges: number;
    repos?: Array<{ name: string; files: number; nodes: number; edges: number }>;
}

export class LatticeSidebarProvider implements vscode.WebviewViewProvider {
    public static readonly viewType = 'lattice.sidebar';

    private webviewView: vscode.WebviewView | undefined;
    private currentStatus: DaemonStatus = 'stopped';
    private stats: IndexStats = { nodes: 0, files: 0, edges: 0 };
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
                const result = await this.daemon.sendRequest('lattice/status') as any;
                if (result && typeof result === 'object') {
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
            });
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
        .actions {
            display: flex;
            flex-direction: column;
            gap: 6px;
        }
        button {
            width: 100%;
            padding: 6px 12px;
            border: none;
            border-radius: 2px;
            background: var(--vscode-button-background);
            color: var(--vscode-button-foreground);
            font-family: var(--vscode-font-family);
            font-size: 13px;
            cursor: pointer;
        }
        button:hover {
            background: var(--vscode-button-hoverBackground);
        }
        button.secondary {
            background: var(--vscode-button-secondaryBackground);
            color: var(--vscode-button-secondaryForeground);
        }
        button.secondary:hover {
            background: var(--vscode-button-secondaryHoverBackground);
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
        <div class="section-title">Actions</div>
        <div class="actions">
            <button id="reindexBtn" onclick="reindex()">Re-index Workspace</button>
            <button id="clearBtn" class="secondary" onclick="clearMemory()">Clear Memory</button>
        </div>
    </div>

    <script>
        const vscode = acquireVsCodeApi();

        const statusDot = document.getElementById('statusDot');
        const statusLabel = document.getElementById('statusLabel');
        const nodeCount = document.getElementById('nodeCount');
        const fileCount = document.getElementById('fileCount');
        const edgeCount = document.getElementById('edgeCount');

        const statusLabels = {
            running: 'Running',
            starting: 'Starting...',
            stopped: 'Stopped',
            error: 'Error'
        };

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
            }
        });

        function reindex() {
            vscode.postMessage({ command: 'reindex' });
        }

        function clearMemory() {
            vscode.postMessage({ command: 'clearMemory' });
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
