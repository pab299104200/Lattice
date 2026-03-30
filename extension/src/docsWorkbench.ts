import * as path from 'path';
import * as vscode from 'vscode';
import { DaemonManager } from './daemon';

type DocsTargetKind = 'auto' | 'file' | 'symbol' | 'doc' | 'section';

export interface DocsTargetSelection {
    target: string;
    kind: DocsTargetKind;
    label: string;
    file?: string;
    line?: number;
}

interface McpToolContentItem {
    type?: string;
    text?: string;
}

interface McpToolResponse {
    content?: McpToolContentItem[];
}

interface LinkReference {
    symbol: string;
    kind: string;
    file: string;
    line: number;
    relationship: string;
    preview: string;
}

interface BacklinksReport {
    requested_target: string;
    resolved_target: string;
    resolved_kind: string;
    file?: string;
    line?: number;
    backlinks: LinkReference[];
    count: number;
}

interface OutgoingLinksReport {
    requested_target: string;
    resolved_target: string;
    resolved_kind: string;
    file?: string;
    line?: number;
    links: LinkReference[];
    count: number;
}

interface GraphNodePayload {
    id: string;
    label: string;
    file: string;
    line: number;
    kind: string;
    relationship: string;
    preview: string;
    target: DocsTargetSelection;
}

interface GraphStatePayload {
    status: 'ready' | 'loading' | 'error' | 'idle';
    focus?: {
        label: string;
        target: string;
        kind: string;
        file?: string;
        line?: number;
    };
    pinned: boolean;
    backlinksCount: number;
    outgoingCount: number;
    backlinks: GraphNodePayload[];
    outgoing: GraphNodePayload[];
    message?: string;
}

export class LatticeDocsWorkbench implements vscode.Disposable {
    private panel: vscode.WebviewPanel | undefined;
    private currentTarget: DocsTargetSelection | undefined;
    private pinnedTarget: DocsTargetSelection | undefined;
    private selectionTimer: ReturnType<typeof setTimeout> | undefined;
    private refreshCounter = 0;
    private readonly disposables: vscode.Disposable[] = [];

    constructor(private readonly daemon: DaemonManager) {
        this.disposables.push(
            vscode.window.onDidChangeActiveTextEditor(() => this.scheduleFollowEditor()),
            vscode.window.onDidChangeTextEditorSelection(() => this.scheduleFollowEditor())
        );
    }

    public async createOrShow(explicitTarget?: DocsTargetSelection): Promise<void> {
        if (!this.panel) {
            this.panel = vscode.window.createWebviewPanel(
                'lattice.docsWorkbench',
                'Lattice Docs Graph',
                vscode.ViewColumn.Beside,
                {
                    enableScripts: true,
                    retainContextWhenHidden: true,
                }
            );
            this.panel.onDidDispose(() => {
                this.panel = undefined;
            }, undefined, this.disposables);
            this.panel.webview.onDidReceiveMessage((message) => {
                void this.handleMessage(message);
            }, undefined, this.disposables);
            this.panel.webview.html = this.getHtml();
        } else {
            this.panel.reveal(vscode.ViewColumn.Beside);
        }

        if (explicitTarget) {
            this.currentTarget = explicitTarget;
            this.pinnedTarget = explicitTarget;
        } else if (!this.pinnedTarget) {
            this.currentTarget = inferDocsTargetFromEditor(vscode.window.activeTextEditor);
        }

        await this.refresh();
    }

    public dispose(): void {
        if (this.selectionTimer) {
            clearTimeout(this.selectionTimer);
            this.selectionTimer = undefined;
        }
        for (const disposable of this.disposables) {
            disposable.dispose();
        }
        this.panel?.dispose();
        this.panel = undefined;
    }

    private scheduleFollowEditor(): void {
        if (!this.panel || !this.panel.visible || this.pinnedTarget) {
            return;
        }

        if (this.selectionTimer) {
            clearTimeout(this.selectionTimer);
        }

        this.selectionTimer = setTimeout(() => {
            this.selectionTimer = undefined;
            const inferredTarget = inferDocsTargetFromEditor(vscode.window.activeTextEditor);
            if (!targetsEqual(this.currentTarget, inferredTarget)) {
                this.currentTarget = inferredTarget;
                void this.refresh();
            }
        }, 180);
    }

    private async handleMessage(message: unknown): Promise<void> {
        if (!message || typeof message !== 'object') {
            return;
        }

        const command = typeof (message as { command?: unknown }).command === 'string'
            ? (message as { command: string }).command
            : undefined;
        if (!command) {
            return;
        }

        switch (command) {
            case 'refresh':
                await this.refresh();
                break;
            case 'toggleFollow':
                if (this.pinnedTarget) {
                    this.pinnedTarget = undefined;
                    this.currentTarget = inferDocsTargetFromEditor(vscode.window.activeTextEditor);
                } else if (this.currentTarget) {
                    this.pinnedTarget = this.currentTarget;
                }
                await this.refresh();
                break;
            case 'focusTarget': {
                const target = normalizeDocsTargetSelection((message as { target?: unknown }).target);
                if (!target) {
                    return;
                }
                this.currentTarget = target;
                this.pinnedTarget = target;
                await this.refresh();
                break;
            }
            case 'openLocation': {
                const target = normalizeDocsTargetSelection((message as { target?: unknown }).target);
                if (!target) {
                    return;
                }
                await openDocsTarget(target);
                break;
            }
        }
    }

    private async refresh(): Promise<void> {
        if (!this.panel) {
            return;
        }

        if (this.daemon.getStatus() !== 'running') {
            this.postState({
                status: 'error',
                pinned: Boolean(this.pinnedTarget),
                backlinksCount: 0,
                outgoingCount: 0,
                backlinks: [],
                outgoing: [],
                message: 'Lattice daemon is not running.',
            });
            return;
        }

        const target = this.pinnedTarget
            ?? this.currentTarget
            ?? inferDocsTargetFromEditor(vscode.window.activeTextEditor);
        this.currentTarget = target;

        if (!target) {
            this.postState({
                status: 'idle',
                pinned: Boolean(this.pinnedTarget),
                backlinksCount: 0,
                outgoingCount: 0,
                backlinks: [],
                outgoing: [],
                message: 'Focus a symbol or a Markdown section to explore its local docs graph.',
            });
            return;
        }

        const refreshId = ++this.refreshCounter;
        this.postState({
            status: 'loading',
            pinned: Boolean(this.pinnedTarget),
            backlinksCount: 0,
            outgoingCount: 0,
            backlinks: [],
            outgoing: [],
            message: `Loading graph for ${target.label}...`,
        });

        try {
            const backlinksPromise: Promise<BacklinksReport> = this.callToolJson('get_backlinks', {
                target: target.target,
                kind: target.kind,
                limit: 14,
            }).then((value) => value as BacklinksReport);
            const outgoingPromise: Promise<OutgoingLinksReport> = this.callToolJson('get_outgoing_links', {
                target: target.target,
                kind: target.kind,
                limit: 14,
            }).then((value) => value as OutgoingLinksReport).catch(() => ({
                requested_target: target.target,
                resolved_target: target.target,
                resolved_kind: target.kind,
                file: target.file,
                line: target.line,
                links: [],
                count: 0,
            }));

            const [backlinks, outgoing] = await Promise.all([backlinksPromise, outgoingPromise]);
            if (refreshId !== this.refreshCounter) {
                return;
            }

            this.postState({
                status: 'ready',
                focus: {
                    label: target.label,
                    target: backlinks.resolved_target || outgoing.resolved_target || target.target,
                    kind: backlinks.resolved_kind || outgoing.resolved_kind || target.kind,
                    file: backlinks.file ?? outgoing.file ?? target.file,
                    line: backlinks.line ?? outgoing.line ?? target.line,
                },
                pinned: Boolean(this.pinnedTarget),
                backlinksCount: backlinks.count ?? backlinks.backlinks.length,
                outgoingCount: outgoing.count ?? outgoing.links.length,
                backlinks: backlinks.backlinks.map((item, index) => this.toGraphNode(item, index)),
                outgoing: outgoing.links.map((item, index) => this.toGraphNode(item, index)),
            });
        } catch (err) {
            if (refreshId !== this.refreshCounter) {
                return;
            }

            const message = err instanceof Error ? err.message : String(err);
            this.postState({
                status: 'error',
                pinned: Boolean(this.pinnedTarget),
                backlinksCount: 0,
                outgoingCount: 0,
                backlinks: [],
                outgoing: [],
                message,
            });
        }
    }

    private toGraphNode(item: LinkReference, index: number): GraphNodePayload {
        return {
            id: `${item.file}:${item.line}:${item.symbol}:${index}`,
            label: item.symbol,
            file: item.file,
            line: item.line,
            kind: item.kind,
            relationship: item.relationship,
            preview: item.preview,
            target: {
                target: item.kind === 'sec'
                    ? `${item.file}#${item.symbol}`
                    : item.kind === 'doc'
                        ? item.file
                        : item.symbol,
                kind: item.kind === 'sec'
                    ? 'section'
                    : item.kind === 'doc'
                        ? 'file'
                        : 'symbol',
                label: item.kind === 'sec'
                    ? `${item.file}#${item.symbol}`
                    : item.kind === 'doc'
                        ? item.file
                        : item.symbol,
                file: item.file,
                line: item.line,
            },
        };
    }

    private async callToolJson(name: string, args: Record<string, unknown>): Promise<unknown> {
        const response = await this.daemon.sendRequest('tools/call', {
            name,
            arguments: args,
        }, 60_000) as McpToolResponse;

        const text = response.content?.find((item) => item.type === 'text')?.text;
        if (typeof text !== 'string') {
            throw new Error(`Unexpected response payload from ${name}`);
        }

        return JSON.parse(text) as unknown;
    }

    private postState(payload: GraphStatePayload): void {
        this.panel?.webview.postMessage({
            type: 'graphState',
            payload,
        });
    }

    private getHtml(): string {
        return /* html */ `<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta http-equiv="Content-Security-Policy" content="default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline';">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <style>
        :root {
            color-scheme: light dark;
        }
        body {
            margin: 0;
            padding: 18px;
            font-family: var(--vscode-font-family);
            color: var(--vscode-foreground);
            background:
                radial-gradient(circle at top left, color-mix(in srgb, var(--vscode-textLink-foreground, #4fa3ff) 16%, transparent), transparent 36%),
                radial-gradient(circle at bottom right, color-mix(in srgb, var(--vscode-button-background, #0e639c) 12%, transparent), transparent 30%),
                var(--vscode-editor-background);
        }
        .shell {
            display: flex;
            flex-direction: column;
            gap: 14px;
        }
        .hero {
            border: 1px solid color-mix(in srgb, var(--vscode-widget-border, #444) 85%, transparent);
            border-radius: 16px;
            padding: 16px;
            background: color-mix(in srgb, var(--vscode-editorWidget-background, var(--vscode-editor-background)) 92%, transparent);
            box-shadow: 0 18px 40px rgba(0, 0, 0, 0.12);
        }
        .eyebrow {
            font-size: 11px;
            letter-spacing: 0.12em;
            text-transform: uppercase;
            color: var(--vscode-descriptionForeground);
        }
        .headline {
            margin-top: 6px;
            font-size: 24px;
            font-weight: 700;
            line-height: 1.15;
        }
        .subhead {
            margin-top: 6px;
            font-size: 12px;
            color: var(--vscode-descriptionForeground);
            line-height: 1.45;
        }
        .toolbar {
            margin-top: 14px;
            display: flex;
            flex-wrap: wrap;
            gap: 8px;
        }
        button {
            border-radius: 999px;
            border: 1px solid var(--vscode-widget-border, #444);
            background: color-mix(in srgb, var(--vscode-button-secondaryBackground, var(--vscode-editor-background)) 92%, transparent);
            color: var(--vscode-foreground);
            padding: 8px 12px;
            font-family: inherit;
            font-size: 12px;
            cursor: pointer;
        }
        button:hover {
            border-color: var(--vscode-focusBorder, #007acc);
            background: var(--vscode-button-secondaryHoverBackground, var(--vscode-list-hoverBackground));
        }
        button.primary {
            background: color-mix(in srgb, var(--vscode-button-background, #0e639c) 88%, transparent);
            color: var(--vscode-button-foreground, #fff);
        }
        .meta-row {
            display: flex;
            flex-wrap: wrap;
            gap: 8px;
            margin-top: 12px;
        }
        .pill {
            border-radius: 999px;
            padding: 6px 10px;
            font-size: 11px;
            border: 1px solid var(--vscode-widget-border, #444);
            background: color-mix(in srgb, var(--vscode-editorWidget-background, var(--vscode-editor-background)) 80%, transparent);
        }
        .graph {
            display: grid;
            grid-template-columns: minmax(0, 1fr) minmax(280px, 340px) minmax(0, 1fr);
            gap: 18px;
            align-items: start;
        }
        .lane {
            border: 1px solid color-mix(in srgb, var(--vscode-widget-border, #444) 80%, transparent);
            border-radius: 18px;
            padding: 14px;
            background: color-mix(in srgb, var(--vscode-sideBar-background, var(--vscode-editor-background)) 86%, transparent);
            min-height: 220px;
        }
        .lane-title {
            font-size: 11px;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--vscode-descriptionForeground);
            margin-bottom: 12px;
        }
        .lane-list {
            display: flex;
            flex-direction: column;
            gap: 10px;
        }
        .node {
            position: relative;
            border-radius: 14px;
            border: 1px solid var(--vscode-widget-border, #444);
            background: color-mix(in srgb, var(--vscode-editorWidget-background, var(--vscode-editor-background)) 88%, transparent);
            padding: 12px;
            overflow: hidden;
        }
        .node::before {
            content: '';
            position: absolute;
            inset: 0 auto 0 0;
            width: 4px;
            background: color-mix(in srgb, var(--vscode-textLink-foreground, #4fa3ff) 80%, transparent);
        }
        .node[data-relationship="mentions"]::before {
            background: color-mix(in srgb, var(--vscode-terminal-ansiGreen, #4ec9b0) 80%, transparent);
        }
        .node[data-relationship="links_to"]::before {
            background: color-mix(in srgb, var(--vscode-terminal-ansiBlue, #4fa3ff) 80%, transparent);
        }
        .node-title {
            font-size: 13px;
            font-weight: 700;
            line-height: 1.35;
        }
        .node-meta {
            margin-top: 4px;
            font-size: 11px;
            color: var(--vscode-descriptionForeground);
            line-height: 1.4;
        }
        .node-preview {
            margin-top: 8px;
            font-size: 12px;
            line-height: 1.45;
            color: color-mix(in srgb, var(--vscode-foreground) 92%, transparent);
        }
        .node-actions {
            margin-top: 10px;
            display: flex;
            gap: 8px;
        }
        .center {
            position: sticky;
            top: 18px;
        }
        .focus-card {
            border-radius: 22px;
            border: 1px solid color-mix(in srgb, var(--vscode-textLink-foreground, #4fa3ff) 45%, var(--vscode-widget-border, #444));
            background:
                linear-gradient(150deg, color-mix(in srgb, var(--vscode-button-background, #0e639c) 16%, transparent), transparent 60%),
                color-mix(in srgb, var(--vscode-editorWidget-background, var(--vscode-editor-background)) 90%, transparent);
            padding: 18px;
            box-shadow: 0 20px 40px rgba(0, 0, 0, 0.14);
        }
        .focus-kind {
            font-size: 11px;
            letter-spacing: 0.08em;
            text-transform: uppercase;
            color: var(--vscode-descriptionForeground);
        }
        .focus-title {
            margin-top: 8px;
            font-size: 22px;
            font-weight: 800;
            line-height: 1.2;
            word-break: break-word;
        }
        .focus-location {
            margin-top: 10px;
            font-size: 12px;
            line-height: 1.4;
            color: var(--vscode-descriptionForeground);
        }
        .focus-actions {
            margin-top: 14px;
            display: flex;
            flex-wrap: wrap;
            gap: 8px;
        }
        .empty {
            font-size: 12px;
            color: var(--vscode-descriptionForeground);
            line-height: 1.55;
        }
        @media (max-width: 980px) {
            .graph {
                grid-template-columns: 1fr;
            }
            .center {
                position: static;
            }
        }
    </style>
</head>
<body>
    <div class="shell">
        <div class="hero">
            <div class="eyebrow">Local Docs Graph</div>
            <div id="headline" class="headline">Focus a symbol or section</div>
            <div id="subhead" class="subhead">Track backlinks, outgoing links, and code mentions around the current editor context.</div>
            <div class="toolbar">
                <button id="refreshBtn" class="primary">Refresh</button>
                <button id="followBtn">Pin Current</button>
            </div>
            <div class="meta-row">
                <div id="backlinksPill" class="pill">0 incoming</div>
                <div id="outgoingPill" class="pill">0 outgoing</div>
                <div id="modePill" class="pill">Following editor</div>
            </div>
        </div>

        <div class="graph">
            <div class="lane">
                <div class="lane-title">Incoming References</div>
                <div id="incomingList" class="lane-list"></div>
            </div>

            <div class="center">
                <div class="focus-card">
                    <div id="focusKind" class="focus-kind">No target</div>
                    <div id="focusTitle" class="focus-title">Open a Markdown doc or select a symbol</div>
                    <div id="focusLocation" class="focus-location">The graph follows the active editor until you pin a target.</div>
                    <div class="focus-actions">
                        <button id="openFocusBtn">Open Target</button>
                    </div>
                </div>
            </div>

            <div class="lane">
                <div class="lane-title">Outgoing Links And Mentions</div>
                <div id="outgoingList" class="lane-list"></div>
            </div>
        </div>
    </div>

    <script>
        const vscode = acquireVsCodeApi();
        let graphState = {
            status: 'idle',
            pinned: false,
            backlinksCount: 0,
            outgoingCount: 0,
            backlinks: [],
            outgoing: []
        };

        const headline = document.getElementById('headline');
        const subhead = document.getElementById('subhead');
        const backlinksPill = document.getElementById('backlinksPill');
        const outgoingPill = document.getElementById('outgoingPill');
        const modePill = document.getElementById('modePill');
        const followBtn = document.getElementById('followBtn');
        const incomingList = document.getElementById('incomingList');
        const outgoingList = document.getElementById('outgoingList');
        const focusKind = document.getElementById('focusKind');
        const focusTitle = document.getElementById('focusTitle');
        const focusLocation = document.getElementById('focusLocation');
        const openFocusBtn = document.getElementById('openFocusBtn');

        function escapeHtml(value) {
            const div = document.createElement('div');
            div.textContent = value;
            return div.innerHTML;
        }

        function renderNodeList(container, nodes, emptyText) {
            if (!nodes || nodes.length === 0) {
                container.innerHTML = '<div class="empty">' + escapeHtml(emptyText) + '</div>';
                return;
            }

            container.innerHTML = nodes.map((node) => {
                const payload = encodeURIComponent(JSON.stringify(node.target));
                return '<div class="node" data-relationship="' + escapeHtml(node.relationship) + '">' +
                    '<div class="node-title">' + escapeHtml(node.label) + '</div>' +
                    '<div class="node-meta">' + escapeHtml(node.relationship) + ' · ' + escapeHtml(node.kind) + '</div>' +
                    '<div class="node-meta">' + escapeHtml(node.file + ':' + String(node.line || 1)) + '</div>' +
                    '<div class="node-preview">' + escapeHtml(node.preview || '') + '</div>' +
                    '<div class="node-actions">' +
                    '<button class="focus-node" data-target="' + payload + '">Focus</button>' +
                    '<button class="open-node" data-target="' + payload + '">Open</button>' +
                    '</div></div>';
            }).join('');
        }

        function render() {
            const focus = graphState.focus;
            headline.textContent = focus ? focus.label : 'Focus a symbol or section';
            subhead.textContent = graphState.message || 'Track backlinks, outgoing links, and code mentions around the current editor context.';
            backlinksPill.textContent = String(graphState.backlinksCount || 0) + ' incoming';
            outgoingPill.textContent = String(graphState.outgoingCount || 0) + ' outgoing';
            modePill.textContent = graphState.pinned ? 'Pinned target' : 'Following editor';
            followBtn.textContent = graphState.pinned ? 'Follow Editor' : 'Pin Current';

            focusKind.textContent = focus ? focus.kind : 'No target';
            focusTitle.textContent = focus ? focus.label : 'Open a Markdown doc or select a symbol';
            focusLocation.textContent = focus && focus.file
                ? focus.file + ':' + String(focus.line || 1)
                : 'The graph follows the active editor until you pin a target.';

            renderNodeList(incomingList, graphState.backlinks, 'No Markdown references point at this target yet.');
            renderNodeList(outgoingList, graphState.outgoing, 'No outgoing doc links or code mentions were found from this target.');
        }

        document.getElementById('refreshBtn').addEventListener('click', () => {
            vscode.postMessage({ command: 'refresh' });
        });
        followBtn.addEventListener('click', () => {
            vscode.postMessage({ command: 'toggleFollow' });
        });
        openFocusBtn.addEventListener('click', () => {
            if (!graphState.focus) {
                return;
            }
            vscode.postMessage({
                command: 'openLocation',
                target: {
                    target: graphState.focus.target,
                    kind: graphState.focus.kind,
                    label: graphState.focus.label,
                    file: graphState.focus.file,
                    line: graphState.focus.line
                }
            });
        });

        document.body.addEventListener('click', (event) => {
            const element = event.target;
            if (!(element instanceof HTMLElement)) {
                return;
            }

            const encodedTarget = element.getAttribute('data-target');
            if (!encodedTarget) {
                return;
            }

            const target = JSON.parse(decodeURIComponent(encodedTarget));
            if (element.classList.contains('focus-node')) {
                vscode.postMessage({ command: 'focusTarget', target });
            }
            if (element.classList.contains('open-node')) {
                vscode.postMessage({ command: 'openLocation', target });
            }
        });

        window.addEventListener('message', (event) => {
            const message = event.data;
            if (message.type === 'graphState') {
                graphState = message.payload;
                render();
            }
        });

        render();
    </script>
</body>
</html>`;
    }
}

export function normalizeDocsTargetSelection(value: unknown): DocsTargetSelection | undefined {
    if (!value || typeof value !== 'object') {
        return undefined;
    }

    const target = typeof (value as { target?: unknown }).target === 'string'
        ? (value as { target: string }).target.trim()
        : '';
    if (!target) {
        return undefined;
    }

    const kindValue = typeof (value as { kind?: unknown }).kind === 'string'
        ? (value as { kind: string }).kind
        : 'auto';
    const kind = ['auto', 'file', 'symbol', 'doc', 'section'].includes(kindValue)
        ? kindValue as DocsTargetKind
        : 'auto';

    const label = typeof (value as { label?: unknown }).label === 'string'
        ? (value as { label: string }).label
        : target;
    const file = typeof (value as { file?: unknown }).file === 'string'
        ? (value as { file: string }).file
        : undefined;
    const line = typeof (value as { line?: unknown }).line === 'number'
        ? (value as { line: number }).line
        : undefined;

    return {
        target,
        kind,
        label,
        file,
        line,
    };
}

function inferDocsTargetFromEditor(editor: vscode.TextEditor | undefined): DocsTargetSelection | undefined {
    const focusedSymbol = getFocusedEditorSymbol(editor);
    if (focusedSymbol) {
        return {
            target: focusedSymbol,
            kind: 'symbol',
            label: focusedSymbol,
        };
    }

    if (!editor || editor.document.uri.scheme !== 'file') {
        return undefined;
    }

    const file = toRelativeWorkspacePath(editor.document.uri);
    const heading = getCurrentMarkdownHeading(editor);
    if (heading) {
        return {
            target: `${file}#${heading}`,
            kind: 'section',
            label: `${file}#${heading}`,
            file,
            line: editor.selection.active.line + 1,
        };
    }

    return {
        target: file,
        kind: 'file',
        label: file,
        file,
        line: editor.selection.active.line + 1,
    };
}

function targetsEqual(left: DocsTargetSelection | undefined, right: DocsTargetSelection | undefined): boolean {
    return left?.target === right?.target && left?.kind === right?.kind;
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

function getCurrentMarkdownHeading(editor: vscode.TextEditor): string | undefined {
    const document = editor.document;
    const file = toRelativeWorkspacePath(document.uri);
    if (document.languageId !== 'markdown' && !file.endsWith('.md')) {
        return undefined;
    }

    for (let line = editor.selection.active.line; line >= 0; line--) {
        const text = document.lineAt(line).text.trim();
        const match = text.match(/^#{1,6}\s+(.+?)\s*#*$/);
        if (match?.[1]) {
            return match[1].trim();
        }
    }

    return undefined;
}

async function openDocsTarget(target: DocsTargetSelection): Promise<void> {
    const file = target.file ?? inferTargetFile(target);
    if (!file) {
        return;
    }

    const uri = await resolveWorkspaceFile(file);
    if (!uri) {
        vscode.window.showWarningMessage(`Lattice: Could not locate ${file} in the current workspace`);
        return;
    }

    const document = await vscode.workspace.openTextDocument(uri);
    const editor = await vscode.window.showTextDocument(document, { preview: false, preserveFocus: false });
    const line = Math.max((target.line ?? 1) - 1, 0);
    const position = new vscode.Position(line, 0);
    editor.selection = new vscode.Selection(position, position);
    editor.revealRange(new vscode.Range(position, position), vscode.TextEditorRevealType.InCenter);
}

function inferTargetFile(target: DocsTargetSelection): string | undefined {
    if (target.kind === 'doc' || target.kind === 'file') {
        return target.target.split('#')[0];
    }
    if (target.kind === 'section') {
        return target.target.split('#')[0];
    }
    return undefined;
}

async function resolveWorkspaceFile(file: string): Promise<vscode.Uri | undefined> {
    if (path.isAbsolute(file)) {
        return vscode.Uri.file(file);
    }

    const folders = vscode.workspace.workspaceFolders ?? [];
    for (const folder of folders) {
        const candidate = vscode.Uri.joinPath(folder.uri, file);
        try {
            await vscode.workspace.fs.stat(candidate);
            return candidate;
        } catch {
            continue;
        }
    }

    return undefined;
}

function toRelativeWorkspacePath(uri: vscode.Uri): string {
    return vscode.workspace.asRelativePath(uri, false).replace(/\\/g, '/');
}
