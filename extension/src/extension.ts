import * as vscode from 'vscode';
import { DaemonManager } from './daemon';
import { StatusBarProvider } from './statusbar';
import { LatticeCodeLensProvider } from './codelens';
import { LatticeHoverProvider } from './hover';
import { LatticeSidebarProvider } from './sidebar';

let daemon: DaemonManager | undefined;

export async function activate(context: vscode.ExtensionContext) {
    console.log('Lattice extension activating...');

    // Create daemon manager
    daemon = new DaemonManager(context.extensionPath);
    context.subscriptions.push(daemon);

    // Create UI providers (before daemon starts so they can receive status updates)
    const statusBar = new StatusBarProvider(daemon);
    context.subscriptions.push(statusBar);

    const sidebarProvider = new LatticeSidebarProvider(daemon);
    const sidebarRegistration = vscode.window.registerWebviewViewProvider(
        LatticeSidebarProvider.viewType,
        sidebarProvider
    );
    context.subscriptions.push(sidebarProvider, sidebarRegistration);

    // When daemon becomes running, fetch stats and update UI
    daemon.onStatusChange(async (status) => {
        console.log(`[lattice] daemon status: ${status}`);
        if (status === 'running') {
            try {
                const result = await daemon!.sendRequest('lattice/status') as any;
                if (result && typeof result === 'object') {
                    const nodes = result.nodes ?? result.node_count ?? 0;
                    const files = result.files ?? result.file_count ?? 0;
                    const edges = result.edges ?? result.edge_count ?? 0;
                    sidebarProvider.updateStats({ nodes, files, edges });
                    statusBar.updateNodeCount(nodes);
                    console.log(`[lattice] indexed: ${nodes} nodes, ${files} files, ${edges} edges`);
                }
            } catch (err) {
                console.error('[lattice] failed to fetch stats:', err);
            }
        }
    });

    // Start daemon (non-blocking)
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
            // Refresh stats after re-index
            const result = await daemon.sendRequest('lattice/status') as any;
            if (result && typeof result === 'object') {
                const nodes = result.nodes ?? result.node_count ?? 0;
                const files = result.files ?? result.file_count ?? 0;
                const edges = result.edges ?? result.edge_count ?? 0;
                sidebarProvider.updateStats({ nodes, files, edges });
                statusBar.updateNodeCount(nodes);
            }
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

    // CodeLens provider
    const codeLensProvider = new LatticeCodeLensProvider(daemon);
    const codeLensRegistration = vscode.languages.registerCodeLensProvider(
        { scheme: 'file' },
        codeLensProvider
    );
    context.subscriptions.push(codeLensProvider, codeLensRegistration);

    // Output channel for dependent results
    const outputChannel = vscode.window.createOutputChannel('Lattice');
    context.subscriptions.push(outputChannel);

    const showDependentsCmd = vscode.commands.registerCommand(
        'lattice.showDependents',
        async (filePath: string, symbolName: string) => {
            if (!daemon || daemon.getStatus() !== 'running') {
                vscode.window.showWarningMessage('Lattice: Daemon is not running');
                return;
            }
            try {
                const result = await daemon.sendRequest('lattice/dependents', {
                    path: filePath,
                    symbol: symbolName,
                });
                const dependents = result as { dependents?: Array<{ file: string; line: number; name: string }> };
                outputChannel.clear();
                outputChannel.appendLine(`Dependents of "${symbolName}" (${filePath}):`);
                outputChannel.appendLine('---');
                if (dependents?.dependents && dependents.dependents.length > 0) {
                    for (const dep of dependents.dependents) {
                        outputChannel.appendLine(`  ${dep.file}:${dep.line} — ${dep.name}`);
                    }
                } else {
                    outputChannel.appendLine('  No dependents found.');
                }
                outputChannel.show(true);
            } catch (err) {
                const msg = err instanceof Error ? err.message : String(err);
                vscode.window.showErrorMessage(`Lattice: Failed to get dependents — ${msg}`);
            }
        }
    );

    // Hover provider
    const hoverProvider = new LatticeHoverProvider(daemon);
    const hoverRegistration = vscode.languages.registerHoverProvider(
        { scheme: 'file' },
        hoverProvider
    );
    context.subscriptions.push(hoverRegistration);

    context.subscriptions.push(reindexCmd, statusCmd, showDependentsCmd);
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
    if (daemon) {
        daemon.dispose();
        daemon = undefined;
    }
}
