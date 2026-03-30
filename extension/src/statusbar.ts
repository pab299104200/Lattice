import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

export class StatusBarProvider implements vscode.Disposable {
    private statusBarItem: vscode.StatusBarItem;
    private nodeCount = 0;
    private indexingPercent: number | null = null;
    private currentStatus: DaemonStatus = 'stopped';
    private disposables: vscode.Disposable[] = [];
    private pollTimer: ReturnType<typeof setInterval> | undefined;

    constructor(private readonly daemon: DaemonManager) {
        this.statusBarItem = vscode.window.createStatusBarItem(
            vscode.StatusBarAlignment.Left,
            100
        );
        this.statusBarItem.command = 'lattice.showStatus';
        this.statusBarItem.show();

        this.currentStatus = daemon.getStatus();
        this.render();

        const sub = daemon.onStatusChange((status) => {
            this.currentStatus = status;
            if (status === 'running') {
                void this.refreshFromDaemon();
                this.startPolling();
            } else {
                this.stopPolling();
                this.indexingPercent = null;
            }
            this.render();
        });
        this.disposables.push(sub);

        if (this.currentStatus === 'running') {
            void this.refreshFromDaemon();
            this.startPolling();
        }
    }

    /**
     * Update the displayed node count.
     */
    public updateNodeCount(count: number): void {
        this.nodeCount = count;
        this.render();
    }

    /**
     * Update the indexing progress indicator.
     * Pass null to clear the progress display.
     */
    public updateIndexingProgress(percent: number | null): void {
        this.indexingPercent = percent;
        this.render();
    }

    private async refreshFromDaemon(): Promise<void> {
        if (this.daemon.getStatus() !== 'running') {
            return;
        }

        try {
            const result = await this.daemon.sendRequest('lattice/status') as {
                status?: string;
                nodes?: number;
                node_count?: number;
            };
            const nodes = result.nodes ?? result.node_count ?? 0;
            this.nodeCount = nodes;
            this.indexingPercent = result.status === 'indexing' ? 0 : null;
            this.render();
        } catch {
            // Keep the last known count if the daemon is busy or restarting.
        }
    }

    private startPolling(): void {
        if (this.pollTimer) {
            return;
        }

        this.pollTimer = setInterval(() => {
            void this.refreshFromDaemon();
        }, 3000);
    }

    private stopPolling(): void {
        if (this.pollTimer) {
            clearInterval(this.pollTimer);
            this.pollTimer = undefined;
        }
    }

    private render(): void {
        switch (this.currentStatus) {
            case 'running':
                if (this.indexingPercent !== null) {
                    this.statusBarItem.text = `$(sync~spin) Lattice: ${this.nodeCount} nodes`;
                    this.statusBarItem.tooltip = `Lattice is indexing — currently ${this.nodeCount} nodes indexed`;
                } else {
                    this.statusBarItem.text = `$(check) Lattice: ${this.nodeCount} nodes`;
                    this.statusBarItem.tooltip = `Lattice daemon running — ${this.nodeCount} nodes indexed`;
                }
                this.statusBarItem.backgroundColor = undefined;
                break;

            case 'starting':
                this.statusBarItem.text = '$(sync~spin) Lattice: Starting...';
                this.statusBarItem.tooltip = 'Lattice daemon is starting...';
                this.statusBarItem.backgroundColor = undefined;
                break;

            case 'stopped':
                this.statusBarItem.text = '$(error) Lattice: Stopped';
                this.statusBarItem.tooltip = 'Lattice daemon is stopped';
                this.statusBarItem.backgroundColor = new vscode.ThemeColor(
                    'statusBarItem.warningBackground'
                );
                break;

            case 'error':
                this.statusBarItem.text = '$(error) Lattice: Error';
                this.statusBarItem.tooltip = 'Lattice daemon encountered an error';
                this.statusBarItem.backgroundColor = new vscode.ThemeColor(
                    'statusBarItem.errorBackground'
                );
                break;
        }
    }

    public dispose(): void {
        this.stopPolling();
        this.statusBarItem.dispose();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}
