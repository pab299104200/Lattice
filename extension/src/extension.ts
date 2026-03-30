import * as vscode from 'vscode';
import * as cp from 'child_process';
import { DaemonManager } from './daemon';
import { StatusBarProvider } from './statusbar';
import { LatticeCodeLensProvider } from './codelens';
import { LatticeHoverProvider } from './hover';
import { LatticeSidebarProvider } from './sidebar';
import {
    DocsTargetSelection,
    LatticeDocsWorkbench,
    normalizeDocsTargetSelection,
} from './docsWorkbench';

let daemon: DaemonManager | undefined;
let lastContextHandle: string | undefined;
let lastContextOrigin: string | undefined;

interface McpToolContentItem {
    type?: string;
    text?: string;
}

interface McpToolResponse {
    content?: McpToolContentItem[];
}

interface DaemonStatusSnapshot {
    status?: string;
    nodes?: number;
    node_count?: number;
    files?: number;
    file_count?: number;
    edges?: number;
    edge_count?: number;
}

interface GitChangedFiles {
    files: string[];
    source: string;
}

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

    const docsWorkbench = new LatticeDocsWorkbench(daemon);
    context.subscriptions.push(docsWorkbench);

    let statsPollTimer: ReturnType<typeof setInterval> | undefined;
    const applyStatsSnapshot = (result: DaemonStatusSnapshot): void => {
        const nodes = result.nodes ?? result.node_count ?? 0;
        const files = result.files ?? result.file_count ?? 0;
        const edges = result.edges ?? result.edge_count ?? 0;
        sidebarProvider.updateStats({ nodes, files, edges });
        statusBar.updateNodeCount(nodes);
    };
    const syncStatsFromDaemon = async (shouldLog = false): Promise<void> => {
        if (!daemon || daemon.getStatus() !== 'running') {
            return;
        }

        try {
            const result = await daemon.sendRequest('lattice/status') as DaemonStatusSnapshot;
            applyStatsSnapshot(result);
            if (shouldLog) {
                const nodes = result.nodes ?? result.node_count ?? 0;
                const files = result.files ?? result.file_count ?? 0;
                const edges = result.edges ?? result.edge_count ?? 0;
                console.log(`[lattice] indexed: ${nodes} nodes, ${files} files, ${edges} edges`);
            }
        } catch (err) {
            console.error('[lattice] failed to fetch stats:', err);
        }
    };
    const startStatsPolling = (): void => {
        if (statsPollTimer) {
            return;
        }
        statsPollTimer = setInterval(() => {
            void syncStatsFromDaemon();
        }, 3000);
    };
    const stopStatsPolling = (): void => {
        if (statsPollTimer) {
            clearInterval(statsPollTimer);
            statsPollTimer = undefined;
        }
    };
    context.subscriptions.push(new vscode.Disposable(() => stopStatsPolling()));

    // When daemon becomes running, fetch stats and update UI
    daemon.onStatusChange(async (status) => {
        console.log(`[lattice] daemon status: ${status}`);
        if (status === 'running') {
            await syncStatsFromDaemon(true);
            startStatsPolling();
        } else {
            stopStatsPolling();
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
            await daemon.sendRequest('lattice/reindex', undefined, 300_000);
            // Poll status until indexing stabilizes
            let lastNodes = -1;
            let stableCount = 0;
            for (let i = 0; i < 60; i++) {
                await new Promise(r => setTimeout(r, 2000));
                const result = await daemon.sendRequest('lattice/status') as DaemonStatusSnapshot;
                if (result && typeof result === 'object') {
                    applyStatsSnapshot(result);
                    const nodes = result.nodes ?? result.node_count ?? 0;
                    if (nodes === lastNodes && nodes > 0) {
                        stableCount++;
                        if (stableCount >= 2) { break; }
                    } else {
                        stableCount = 0;
                    }
                    lastNodes = nodes;
                }
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
                    file: filePath,
                    name: symbolName,
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

    const prepareChangeCmd = vscode.commands.registerCommand('lattice.prepareChange', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const query = await vscode.window.showInputBox({
            prompt: 'Describe the change you want Lattice to prepare',
            placeHolder: 'e.g. fix memory recall timeout on new session',
            ignoreFocusOut: true,
        });
        if (!query || !query.trim()) {
            return;
        }

        const activeFile = getActiveEditorFile();
        const focusedSymbol = getFocusedEditorSymbol(vscode.window.activeTextEditor);
        const args: Record<string, unknown> = {
            query: query.trim(),
            mode: 'compact',
        };
        if (activeFile) {
            args.entry_files = [activeFile];
        }
        if (focusedSymbol) {
            args.entry_symbols = [focusedSymbol];
        }

        try {
            const result = await callTool(daemon, 'prepare_change', args);
            showToolResult(outputChannel, `Lattice prepare_change: ${query.trim()}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Prepare change failed — ${msg}`);
        }
    });

    const impactFromDiffCmd = vscode.commands.registerCommand('lattice.impactFromDiff', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const workspaceRoot = getWorkspaceRoot(vscode.window.activeTextEditor);
        if (!workspaceRoot) {
            vscode.window.showWarningMessage('Lattice: Open a workspace folder to analyze git diff impact');
            return;
        }

        try {
            const diffInfo = await getPreferredGitDiff(workspaceRoot);
            const result = await callTool(daemon, 'impact_from_diff', {
                diff: diffInfo.diff,
                mode: 'compact',
                hops: 2,
            });
            showToolResult(outputChannel, `Lattice impact_from_diff: ${diffInfo.source}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Diff impact failed — ${msg}`);
        }
    });

    const workingSetCmd = vscode.commands.registerCommand('lattice.getWorkingSetContext', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const query = await vscode.window.showInputBox({
            prompt: 'Optional task hint for the current working set',
            placeHolder: 'Leave empty to just use the visible editors',
            ignoreFocusOut: true,
        });
        if (query === undefined) {
            return;
        }

        const args: Record<string, unknown> = {
            mode: 'compact',
        };
        const files = collectVisibleEditorFiles();
        const focusedSymbol = getFocusedEditorSymbol(vscode.window.activeTextEditor);
        if (query.trim()) {
            args.query = query.trim();
        }
        if (files.length > 0) {
            args.files = files;
        }
        if (focusedSymbol) {
            args.symbols = [focusedSymbol];
        }

        try {
            const result = await callTool(daemon, 'get_working_set_context', args);
            const title = query.trim()
                ? `Lattice working set: ${query.trim()}`
                : 'Lattice working set context';
            showToolResult(outputChannel, title, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Working set request failed — ${msg}`);
        }
    });

    const diagnoseFailureCmd = vscode.commands.registerCommand('lattice.diagnoseFailure', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        let input = getSelectedEditorText(vscode.window.activeTextEditor);
        if (!input) {
            input = await vscode.window.showInputBox({
                prompt: 'Paste an error line, failing test output, or stack trace excerpt',
                placeHolder: "e.g. thread 'main' panicked at src/rpc/mcp.rs:101",
                ignoreFocusOut: true,
            });
        }
        if (!input || !input.trim()) {
            return;
        }

        try {
            const result = await callTool(daemon, 'diagnose_failure', {
                input: input.trim(),
                mode: 'compact',
            });
            showToolResult(outputChannel, 'Lattice diagnose_failure', result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Failure diagnosis failed — ${msg}`);
        }
    });

    const expandContextCmd = vscode.commands.registerCommand('lattice.expandContext', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }
        if (!lastContextHandle) {
            vscode.window.showInformationMessage(
                'Lattice: Run prepare change, diff impact, working set, or diagnose failure first to capture a context handle'
            );
            return;
        }

        const focus = await vscode.window.showInputBox({
            prompt: `Expand context from ${lastContextOrigin ?? 'the last workflow result'}`,
            placeHolder: 'symbol:search_across_sessions or file:daemon/crates/lattice-core/src/memory/store.rs',
            ignoreFocusOut: true,
        });
        if (!focus || !focus.trim()) {
            return;
        }

        try {
            const result = await callTool(daemon, 'expand_context', {
                handle: lastContextHandle,
                focus: focus.trim(),
                max_tokens: 1200,
            });
            showToolResult(outputChannel, `Lattice expand_context: ${focus.trim()}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Context expansion failed — ${msg}`);
        }
    });

    const docsCapsuleCmd = vscode.commands.registerCommand('lattice.getDocsCapsule', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const query = await vscode.window.showInputBox({
            prompt: 'Ask for engineering docs, runbooks, or decisions',
            placeHolder: 'e.g. auth login flow and operational checklist',
            ignoreFocusOut: true,
        });
        if (!query || !query.trim()) {
            return;
        }

        const files = collectVisibleEditorFiles();
        const focusedSymbol = getFocusedEditorSymbol(vscode.window.activeTextEditor);
        const args: Record<string, unknown> = {
            query: query.trim(),
            limit: 6,
        };
        if (files.length > 0) {
            args.files = files;
        }
        if (focusedSymbol) {
            args.symbols = [focusedSymbol];
        }

        try {
            const result = await callTool(daemon, 'get_docs_capsule', args);
            showToolResult(outputChannel, `Lattice docs capsule: ${query.trim()}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Docs capsule failed — ${msg}`);
        }
    });

    const backlinksCmd = vscode.commands.registerCommand('lattice.showBacklinks', async (input?: unknown) => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const selection = normalizeDocsTargetSelection(input)
            ?? inferDocsTargetFromEditor(vscode.window.activeTextEditor)
            ?? await promptForDocsTarget('Show backlinks for a symbol, file, or docs section');
        if (!selection) {
            return;
        }

        try {
            const result = await callTool(daemon, 'get_backlinks', {
                target: selection.target,
                kind: selection.kind,
                limit: 20,
            });
            showToolResult(outputChannel, `Lattice backlinks: ${selection.label}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Backlinks lookup failed — ${msg}`);
        }
    });

    const outgoingLinksCmd = vscode.commands.registerCommand('lattice.showOutgoingLinks', async (input?: unknown) => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const selection = normalizeDocsTargetSelection(input)
            ?? inferDocsFileOrSectionTarget(vscode.window.activeTextEditor)
            ?? await promptForDocsTarget('Show outgoing links for a docs file or section');
        if (!selection) {
            return;
        }

        try {
            const result = await callTool(daemon, 'get_outgoing_links', {
                target: selection.target,
                kind: selection.kind,
                limit: 20,
            });
            showToolResult(outputChannel, `Lattice outgoing links: ${selection.label}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Outgoing links lookup failed — ${msg}`);
        }
    });

    const findStaleDocsCmd = vscode.commands.registerCommand('lattice.findStaleDocs', async () => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        const staleInputs = await collectStaleDocInputs(vscode.window.activeTextEditor);
        if (staleInputs.files.length === 0 && staleInputs.symbols.length === 0) {
            vscode.window.showInformationMessage(
                'Lattice: No git diff or active editor context available to check stale docs'
            );
            return;
        }

        try {
            const result = await callTool(daemon, 'find_stale_docs', {
                files: staleInputs.files,
                symbols: staleInputs.symbols,
                limit: 12,
            });
            showToolResult(outputChannel, `Lattice stale docs: ${staleInputs.source}`, result);
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Stale-doc check failed — ${msg}`);
        }
    });

    const openDocsWorkbenchCmd = vscode.commands.registerCommand('lattice.openDocsWorkbench', async (input?: unknown) => {
        if (!daemon || daemon.getStatus() !== 'running') {
            vscode.window.showWarningMessage('Lattice: Daemon is not running');
            return;
        }

        try {
            await docsWorkbench.createOrShow(normalizeDocsTargetSelection(input));
        } catch (err) {
            const msg = err instanceof Error ? err.message : String(err);
            vscode.window.showErrorMessage(`Lattice: Docs graph failed — ${msg}`);
        }
    });

    // Hover provider
    const hoverProvider = new LatticeHoverProvider(daemon);
    const hoverRegistration = vscode.languages.registerHoverProvider(
        { scheme: 'file' },
        hoverProvider
    );
    context.subscriptions.push(hoverRegistration);

    context.subscriptions.push(
        reindexCmd,
        statusCmd,
        showDependentsCmd,
        prepareChangeCmd,
        impactFromDiffCmd,
        workingSetCmd,
        diagnoseFailureCmd,
        expandContextCmd,
        docsCapsuleCmd,
        backlinksCmd,
        outgoingLinksCmd,
        findStaleDocsCmd,
        openDocsWorkbenchCmd
    );
}

export function deactivate() {
    console.log('Lattice extension deactivating...');
    if (daemon) {
        daemon.dispose();
        daemon = undefined;
    }
}

async function callTool(
    daemonManager: DaemonManager,
    name: string,
    args: Record<string, unknown>,
    timeoutMs = 120_000
): Promise<unknown> {
    const response = await daemonManager.sendRequest('tools/call', {
        name,
        arguments: args,
    }, timeoutMs) as McpToolResponse;

    const text = response.content?.find((item) => item.type === 'text')?.text;
    if (typeof text !== 'string') {
        throw new Error(`Unexpected response payload from ${name}`);
    }

    try {
        return JSON.parse(text) as unknown;
    } catch {
        return text;
    }
}

function showToolResult(
    outputChannel: vscode.OutputChannel,
    title: string,
    result: unknown
): void {
    rememberContextHandle(result);

    outputChannel.clear();
    outputChannel.appendLine(title);
    outputChannel.appendLine('='.repeat(title.length));
    outputChannel.appendLine('');
    if (lastContextHandle && hasContextHandle(result)) {
        outputChannel.appendLine(`Context handle: ${lastContextHandle}`);
        if (lastContextOrigin) {
            outputChannel.appendLine(`Context origin: ${lastContextOrigin}`);
        }
        outputChannel.appendLine('');
    }
    outputChannel.appendLine(
        typeof result === 'string' ? result : JSON.stringify(result, null, 2)
    );
    outputChannel.show(true);
}

function getActiveEditorFile(): string | undefined {
    const editor = vscode.window.activeTextEditor;
    if (!editor || editor.document.uri.scheme !== 'file') {
        return undefined;
    }

    return toRelativeWorkspacePath(editor.document.uri);
}

function collectVisibleEditorFiles(): string[] {
    const files = new Set<string>();
    for (const editor of vscode.window.visibleTextEditors) {
        if (editor.document.uri.scheme !== 'file') {
            continue;
        }
        files.add(toRelativeWorkspacePath(editor.document.uri));
    }
    return Array.from(files);
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

function getSelectedEditorText(editor: vscode.TextEditor | undefined): string | undefined {
    if (!editor || editor.selection.isEmpty) {
        return undefined;
    }

    const text = editor.document.getText(editor.selection).trim();
    return text || undefined;
}

function isCompactSymbol(value: string): boolean {
    return value.length >= 2 && value.length <= 120 && /^[A-Za-z_][\w.$:-]*$/.test(value);
}

function hasContextHandle(result: unknown): result is { context_handle: string } {
    if (!result || typeof result !== 'object') {
        return false;
    }

    return typeof (result as { context_handle?: unknown }).context_handle === 'string';
}

function rememberContextHandle(result: unknown): void {
    if (!result || typeof result !== 'object') {
        return;
    }

    const contextHandle = (result as { context_handle?: unknown }).context_handle;
    if (typeof contextHandle !== 'string') {
        return;
    }

    lastContextHandle = contextHandle;
    const contextOrigin = (result as { context_origin?: unknown }).context_origin;
    lastContextOrigin = typeof contextOrigin === 'string' ? contextOrigin : undefined;
}

function getWorkspaceRoot(editor: vscode.TextEditor | undefined): string | undefined {
    const folder = editor ? vscode.workspace.getWorkspaceFolder(editor.document.uri) : undefined;
    return folder?.uri.fsPath ?? vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
}

function toRelativeWorkspacePath(uri: vscode.Uri): string {
    return vscode.workspace.asRelativePath(uri, false).replace(/\\/g, '/');
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

    const docsTarget = inferDocsFileOrSectionTarget(editor);
    if (docsTarget) {
        return docsTarget;
    }

    const activeFile = getActiveEditorFile();
    if (!activeFile) {
        return undefined;
    }

    return {
        target: activeFile,
        kind: 'file',
        label: activeFile,
    };
}

function inferDocsFileOrSectionTarget(editor: vscode.TextEditor | undefined): DocsTargetSelection | undefined {
    if (!editor || editor.document.uri.scheme !== 'file') {
        return undefined;
    }

    const file = toRelativeWorkspacePath(editor.document.uri);
    const isMarkdown = editor.document.languageId === 'markdown' || file.endsWith('.md');
    if (!isMarkdown) {
        return {
            target: file,
            kind: 'file',
            label: file,
        };
    }

    const sectionTitle = getCurrentMarkdownHeading(editor);
    if (sectionTitle) {
        return {
            target: `${file}#${sectionTitle}`,
            kind: 'section',
            label: `${file}#${sectionTitle}`,
        };
    }

    return {
        target: file,
        kind: 'file',
        label: file,
    };
}

function getCurrentMarkdownHeading(editor: vscode.TextEditor): string | undefined {
    for (let line = editor.selection.active.line; line >= 0; line--) {
        const text = editor.document.lineAt(line).text.trim();
        const match = text.match(/^#{1,6}\s+(.+?)\s*#*$/);
        if (match?.[1]) {
            return match[1].trim();
        }
    }

    return undefined;
}

async function promptForDocsTarget(prompt: string): Promise<DocsTargetSelection | undefined> {
    const target = await vscode.window.showInputBox({
        prompt,
        placeHolder: 'e.g. loginUser or docs/auth-guide.md#Login Flow',
        ignoreFocusOut: true,
    });
    if (!target || !target.trim()) {
        return undefined;
    }

    return {
        target: target.trim(),
        kind: 'auto',
        label: target.trim(),
    };
}

async function collectStaleDocInputs(
    editor: vscode.TextEditor | undefined
): Promise<{ files: string[]; symbols: string[]; source: string }> {
    const workspaceRoot = getWorkspaceRoot(editor);
    const focusedSymbol = getFocusedEditorSymbol(editor);
    const activeFile = editor && editor.document.uri.scheme === 'file'
        ? toRelativeWorkspacePath(editor.document.uri)
        : undefined;

    if (workspaceRoot) {
        const gitChanged = await getPreferredGitChangedFiles(workspaceRoot).catch(() => undefined);
        if (gitChanged && gitChanged.files.length > 0) {
            return {
                files: gitChanged.files,
                symbols: focusedSymbol ? [focusedSymbol] : [],
                source: gitChanged.source,
            };
        }
    }

    return {
        files: activeFile ? [activeFile] : [],
        symbols: focusedSymbol ? [focusedSymbol] : [],
        source: activeFile ? `active file ${activeFile}` : 'current editor context',
    };
}

async function getPreferredGitDiff(cwd: string): Promise<{ diff: string; source: string }> {
    const stagedDiff = await runExecFile('git', ['diff', '--cached', '--no-ext-diff', '--unified=3'], cwd)
        .catch(() => '');
    if (stagedDiff.trim()) {
        return { diff: stagedDiff, source: 'staged diff' };
    }

    const workingDiff = await runExecFile('git', ['diff', '--no-ext-diff', '--unified=3'], cwd)
        .catch(() => '');
    if (workingDiff.trim()) {
        return { diff: workingDiff, source: 'working tree diff' };
    }

    throw new Error('No staged or working tree diff found');
}

async function getPreferredGitChangedFiles(cwd: string): Promise<GitChangedFiles> {
    const stagedFiles = await runExecFile('git', ['diff', '--cached', '--name-only'], cwd)
        .catch(() => '');
    const parsedStaged = parseGitChangedFiles(stagedFiles);
    if (parsedStaged.length > 0) {
        return { files: parsedStaged, source: 'staged diff' };
    }

    const workingFiles = await runExecFile('git', ['diff', '--name-only'], cwd)
        .catch(() => '');
    const parsedWorking = parseGitChangedFiles(workingFiles);
    if (parsedWorking.length > 0) {
        return { files: parsedWorking, source: 'working tree diff' };
    }

    throw new Error('No staged or working tree diff found');
}

function parseGitChangedFiles(stdout: string): string[] {
    const seen = new Set<string>();
    const files: string[] = [];

    for (const line of stdout.split(/\r?\n/)) {
        const trimmed = line.trim().replace(/\\/g, '/');
        if (!trimmed || seen.has(trimmed)) {
            continue;
        }
        seen.add(trimmed);
        files.push(trimmed);
    }

    return files;
}

function runExecFile(command: string, args: string[], cwd: string): Promise<string> {
    return new Promise((resolve, reject) => {
        cp.execFile(
            command,
            args,
            {
                cwd,
                encoding: 'utf8',
                maxBuffer: 5 * 1024 * 1024,
            },
            (error, stdout, stderr) => {
                if (error) {
                    reject(new Error(stderr.trim() || error.message));
                    return;
                }
                resolve(stdout);
            }
        );
    });
}
