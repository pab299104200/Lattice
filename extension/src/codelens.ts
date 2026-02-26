import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

interface FileSymbol {
    name: string;
    kind: string;
    line: number;
    character: number;
    dependentCount: number;
    dependentFileCount: number;
}

interface FileSymbolsResponse {
    symbols: FileSymbol[];
}

export class LatticeCodeLensProvider implements vscode.CodeLensProvider {
    private readonly _onDidChangeCodeLenses = new vscode.EventEmitter<void>();
    public readonly onDidChangeCodeLenses = this._onDidChangeCodeLenses.event;

    constructor(private readonly daemon: DaemonManager) {}

    /**
     * Force a refresh of all CodeLenses.
     */
    public refresh(): void {
        this._onDidChangeCodeLenses.fire();
    }

    public async provideCodeLenses(
        document: vscode.TextDocument,
        _token: vscode.CancellationToken
    ): Promise<vscode.CodeLens[]> {
        if (this.daemon.getStatus() !== 'running') {
            return [];
        }

        try {
            const relPath = vscode.workspace.asRelativePath(document.uri).replace(/\\/g, '/');
            const result = await this.daemon.sendRequest('lattice/file_symbols', {
                file: relPath,
            });

            const response = result as FileSymbolsResponse;
            if (!response?.symbols) {
                return [];
            }

            const lenses: vscode.CodeLens[] = [];

            for (const symbol of response.symbols) {
                if (symbol.dependentCount <= 0) {
                    continue;
                }

                const range = new vscode.Range(
                    symbol.line,
                    symbol.character,
                    symbol.line,
                    symbol.character + symbol.name.length
                );

                const fileLabel = symbol.dependentFileCount === 1 ? 'file' : 'files';
                const depLabel = symbol.dependentCount === 1 ? 'dependent' : 'dependents';

                const lens = new vscode.CodeLens(range, {
                    title: `Lattice: ${symbol.dependentCount} ${depLabel} across ${symbol.dependentFileCount} ${fileLabel}`,
                    command: 'lattice.showDependents',
                    arguments: [document.uri.fsPath, symbol.name],
                    tooltip: `Show dependents of ${symbol.name}`,
                });

                lenses.push(lens);
            }

            return lenses;
        } catch (err) {
            console.error(`[lattice] CodeLens error: ${err}`);
            return [];
        }
    }

    public dispose(): void {
        this._onDidChangeCodeLenses.dispose();
    }
}
