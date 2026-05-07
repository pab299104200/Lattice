import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

interface SymbolInfo {
    name: string;
    dependentCount: number;
    topCallers?: string[];
    hotspot?: number;
    lastModified: string | null;
}

export class LatticeHoverProvider implements vscode.HoverProvider {
    constructor(private readonly daemon: DaemonManager) {}

    public async provideHover(
        document: vscode.TextDocument,
        position: vscode.Position,
        _token: vscode.CancellationToken
    ): Promise<vscode.Hover | null> {
        if (this.daemon.getStatus() !== 'running') {
            return null;
        }

        const wordRange = document.getWordRangeAtPosition(position);
        if (!wordRange) {
            return null;
        }

        const word = document.getText(wordRange);
        if (!word || word.length < 2) {
            return null;
        }

        try {
            const relPath = vscode.workspace.asRelativePath(document.uri).replace(/\\/g, '/');
            const result = await this.daemon.sendRequest('lattice/symbol_info', {
                file: relPath,
                name: word,
                line: position.line,
                character: position.character,
            });

            const info = result as SymbolInfo;
            if (!info || info.dependentCount === 0) {
                return null;
            }

            const markdown = this.buildMarkdown(info);
            return new vscode.Hover(markdown, wordRange);
        } catch (err) {
            console.error(`[lattice] Hover error: ${err}`);
            return null;
        }
    }

    private buildMarkdown(info: SymbolInfo): vscode.MarkdownString {
        const md = new vscode.MarkdownString();
        md.isTrusted = true;
        md.supportThemeIcons = true;

        md.appendMarkdown(`**Lattice Impact: \`${info.name}\`**\n\n`);

        // Build info table
        md.appendMarkdown('| Metric | Value |\n');
        md.appendMarkdown('|--------|-------|\n');
        md.appendMarkdown(`| Dependents | ${info.dependentCount} |\n`);

        // Top callers
        if (info.topCallers && info.topCallers.length > 0) {
            const callerList = info.topCallers
                .slice(0, 3)
                .map((caller) => `\`${caller}\``)
                .join(', ');
            md.appendMarkdown(`| Top callers | ${callerList} |\n`);
        } else {
            md.appendMarkdown('| Top callers | _none_ |\n');
        }

        // Hotspot indicator
        const hotspotIcon = (info.hotspot ?? 0) > 0 ? '$(circle-filled)' : '$(circle-outline)';
        md.appendMarkdown(`| Hotspot | ${hotspotIcon} |\n`);

        // Last modified
        const lastMod = info.lastModified ?? '_unknown_';
        md.appendMarkdown(`| Last modified | ${lastMod} |\n`);

        return md;
    }
}
