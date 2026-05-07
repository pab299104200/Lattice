import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

interface FileSymbol {
    name: string;
    line: number;
    character?: number;
    dependentCount: number;
    dependentFileCount?: number;
    fileCount?: number;
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

        if (isMarkdownDocument(document)) {
            return provideMarkdownCodeLenses(document);
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

                const line = Math.max((symbol.line ?? 1) - 1, 0);
                const character = Math.max(symbol.character ?? 0, 0);
                const dependentFileCount = symbol.dependentFileCount ?? symbol.fileCount ?? 0;
                const range = new vscode.Range(
                    line,
                    character,
                    line,
                    character + symbol.name.length
                );

                const fileLabel = dependentFileCount === 1 ? 'file' : 'files';
                const depLabel = symbol.dependentCount === 1 ? 'dependent' : 'dependents';

                const lens = new vscode.CodeLens(range, {
                    title: `Lattice: ${symbol.dependentCount} ${depLabel} across ${dependentFileCount} ${fileLabel}`,
                    command: 'lattice.showDependents',
                    arguments: [relPath, symbol.name],
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

function isMarkdownDocument(document: vscode.TextDocument): boolean {
    const relPath = vscode.workspace.asRelativePath(document.uri).replace(/\\/g, '/');
    return document.languageId === 'markdown' || relPath.endsWith('.md');
}

function provideMarkdownCodeLenses(document: vscode.TextDocument): vscode.CodeLens[] {
    const relPath = vscode.workspace.asRelativePath(document.uri).replace(/\\/g, '/');
    const headings = collectMarkdownHeadings(document);
    const lenses: vscode.CodeLens[] = [];
    const fileRange = new vscode.Range(0, 0, 0, 0);

    addMarkdownActionLenses(lenses, fileRange, {
        target: relPath,
        kind: 'file',
        label: relPath,
        file: relPath,
        line: 1,
    });

    if (headings.length === 0) {
        return lenses;
    }

    for (const heading of headings) {
        const range = new vscode.Range(heading.line, 0, heading.line, 0);
        addMarkdownActionLenses(lenses, range, {
            target: `${relPath}#${heading.title}`,
            kind: 'section',
            label: `${relPath}#${heading.title}`,
            file: relPath,
            line: heading.line + 1,
        });
    }

    return lenses;
}

function addMarkdownActionLenses(
    lenses: vscode.CodeLens[],
    range: vscode.Range,
    target: { target: string; kind: string; label: string; file: string; line: number }
): void {
    lenses.push(
        new vscode.CodeLens(range, {
            title: 'Lattice: Open Docs Graph',
            command: 'lattice.openDocsWorkbench',
            arguments: [target],
            tooltip: `Open the local docs graph for ${target.label}`,
        }),
        new vscode.CodeLens(range, {
            title: 'Backlinks',
            command: 'lattice.showBacklinks',
            arguments: [target],
            tooltip: `Show Markdown backlinks for ${target.label}`,
        }),
        new vscode.CodeLens(range, {
            title: 'Outgoing',
            command: 'lattice.showOutgoingLinks',
            arguments: [target],
            tooltip: `Show outgoing links and mentions for ${target.label}`,
        })
    );
}

function collectMarkdownHeadings(document: vscode.TextDocument): Array<{ line: number; title: string }> {
    const headings: Array<{ line: number; title: string }> = [];
    for (let line = 0; line < document.lineCount; line++) {
        const text = document.lineAt(line).text.trim();
        const match = text.match(/^#{1,6}\s+(.+?)\s*#*$/);
        if (!match?.[1]) {
            continue;
        }
        headings.push({
            line,
            title: match[1].trim(),
        });
    }
    return headings;
}
