import * as vscode from 'vscode';

export function activate(context: vscode.ExtensionContext) {
    console.log('Lattice extension activating...');

    const reindexCmd = vscode.commands.registerCommand('lattice.reindex', () => {
        vscode.window.showInformationMessage('Lattice: Re-indexing workspace...');
    });

    const statusCmd = vscode.commands.registerCommand('lattice.showStatus', () => {
        vscode.window.showInformationMessage('Lattice: Daemon not yet connected');
    });

    context.subscriptions.push(reindexCmd, statusCmd);
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
}
