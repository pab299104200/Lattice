import * as vscode from 'vscode';
import { DaemonManager, DaemonStatus } from './daemon';

export class StatusBarProvider implements vscode.Disposable {
    private statusBarItem: vscode.StatusBarItem;
    private nodeCount = 0;
    private indexingPercent: number | null = null;
    private currentStatus: DaemonStatus = 'stopped';
    private disposables: vscode.Disposable[] = [];

    constructor(daemon: DaemonManager) {
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
            this.render();
        });
        this.disposables.push(sub);
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

    private render(): void {
        switch (this.currentStatus) {
            case 'running':
                if (this.indexingPercent !== null) {
                    this.statusBarItem.text = `$(sync~spin) Lattice: Indexing ${this.indexingPercent}%`;
                    this.statusBarItem.tooltip = `Lattice is indexing... ${this.indexingPercent}% complete`;
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
        this.statusBarItem.dispose();
        for (const d of this.disposables) {
            d.dispose();
        }
        this.disposables = [];
    }
}
