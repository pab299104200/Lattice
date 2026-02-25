import * as vscode from 'vscode';
import { DaemonManager } from './daemon';
import { StatusBarProvider } from './statusbar';

let daemon: DaemonManager | undefined;

export async function activate(context: vscode.ExtensionContext) {
    console.log('Lattice extension activating...');

    // Create and start daemon manager
    daemon = new DaemonManager(context.extensionPath);
    context.subscriptions.push(daemon);

    // Create status bar
    const statusBar = new StatusBarProvider(daemon);
    context.subscriptions.push(statusBar);

    daemon.onStatusChange((status) => {
        console.log(`[lattice] daemon status: ${status}`);
    });

    // Start daemon (non-blocking — don't await so activation isn't held up)
    daemon.start().catch((err) => {
        console.error(`[lattice] failed to start daemon: ${err.message}`);
        vscode.window.showErrorMessage(`Lattice: Failed to start daemon — ${err.message}`);
    });

    // Register commands
    const reindexCmd = vscode.commands.registerCommand('lattice.reindex', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }
        try {
            vscode.window.showInformationMessage('Lattice: Re-indexing workspace...');
            await daemon.sendRequest('lattice/reindex');
            vscode.window.showInformationMessage('Lattice: Re-index complete');
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Re-index failed — ${msg}`);
        }
    });

    const statusCmd = vscode.commands.registerCommand('lattice.showStatus', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showInformationMessage(`Lattice: Daemon status — ${daemon?.getStatus() ?? 'unknown'}`);
            return;
        }
        try {
            const result = await daemon.sendRequest('lattice/status');
            const info = typeof result === 'object' && result !== null ? JSON.stringify(result, null, 2) : String(result);
            vscode.window.showInformationMessage(`Lattice Status:\n${info}`);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Status request failed — ${msg}`);
        }
    });

    context.subscriptions.push(reindexCmd, statusCmd);
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
    if (daemon) {
        daemon.dispose();
        daemon = undefined;
    }
}
