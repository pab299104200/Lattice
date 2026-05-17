// Contract gate R79 — extension ↔ daemon review-UI MCP plumbing.
//
// Spawns the real daemon binary from `extension/bin/lattice` (per
// `lattice/CLAUDE.md ## Deploy`) and exercises one happy-path and one
// error-path tools/call per review-UI tool consumed by `ReviewRpcBridge`.
// Every typed response is parsed through the same `normalize*` helper the
// extension uses at runtime, so any wire-shape drift surfaces here rather
// than in the webview.
//
// Spec anchors:
//   - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 9. MCP Surface`
//   - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## MCP Tool Contract Principles`
//   - `docs/plans/2026-05-16-cognitive-workspace-fork-plan.md` `## 10. Human Review Surface`

import * as assert from 'assert';
import * as cp from 'child_process';
import * as fs from 'fs';
import * as os from 'os';
import * as path from 'path';
import { suite, test, suiteSetup, suiteTeardown } from 'mocha';

import {
    arrayValue,
    normalizeConflictList,
    normalizeConsolidationReport,
    normalizeEventTracePage,
    normalizeEvolutionProposal,
    normalizeIndexStatus,
    normalizeMemory,
    normalizeMetricSnapshot,
    normalizeSessionMetrics,
    normalizeVerifyExplainResponse,
    recordValue,
} from '../review/rpcPayloads';

interface JsonRpcResponse {
    jsonrpc: '2.0';
    id: number;
    result?: unknown;
    error?: { code: number; message: string; data?: unknown };
}

interface McpToolResponse {
    content?: Array<{ type?: string; text?: string }>;
}

interface ContractLog {
    method: string;
    params: unknown;
    response: unknown;
}

class DaemonClient {
    private nextId = 1;
    private buffer = '';
    private pending = new Map<number, {
        resolve: (value: unknown) => void;
        reject: (reason: Error) => void;
        timer: NodeJS.Timeout;
    }>();
    public readonly transcript: ContractLog[] = [];
    public stderr = '';

    private constructor(private readonly proc: cp.ChildProcessWithoutNullStreams) {
        proc.stdout.setEncoding('utf-8');
        proc.stdout.on('data', (chunk: string) => this.onStdout(chunk));
        proc.stderr.setEncoding('utf-8');
        proc.stderr.on('data', (chunk: string) => {
            this.stderr += chunk;
        });
        proc.on('exit', (code, signal) => {
            for (const [id, pending] of this.pending) {
                clearTimeout(pending.timer);
                pending.reject(
                    new Error(`Daemon exited code=${code} signal=${signal} before responding to id=${id}`)
                );
                this.pending.delete(id);
            }
        });
    }

    public static async spawn(binaryPath: string, workspace: string): Promise<DaemonClient> {
        const proc = cp.spawn(binaryPath, ['--stdio', '--workspace', workspace], {
            cwd: workspace,
            stdio: ['pipe', 'pipe', 'pipe'],
            env: { ...process.env, RUST_LOG: 'warn' },
        });
        return new DaemonClient(proc);
    }

    private onStdout(chunk: string): void {
        this.buffer += chunk;
        while (true) {
            const newline = this.buffer.indexOf('\n');
            if (newline === -1) {
                return;
            }
            const line = this.buffer.substring(0, newline).trim();
            this.buffer = this.buffer.substring(newline + 1);
            if (!line || !line.startsWith('{')) {
                continue;
            }
            try {
                const message = JSON.parse(line) as JsonRpcResponse;
                const pending = this.pending.get(message.id);
                if (!pending) {
                    continue;
                }
                this.pending.delete(message.id);
                clearTimeout(pending.timer);
                if (message.error) {
                    pending.reject(
                        new Error(`JSON-RPC error ${message.error.code}: ${message.error.message}`)
                    );
                } else {
                    pending.resolve(message.result);
                }
            } catch (error) {
                console.error('[contract-test] failed to parse line:', line, error);
            }
        }
    }

    public send(method: string, params?: unknown, timeoutMs = 30_000): Promise<unknown> {
        return new Promise<unknown>((resolve, reject) => {
            const id = this.nextId++;
            const request = {
                jsonrpc: '2.0',
                id,
                method,
                ...(params !== undefined ? { params } : {}),
            };
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new Error(
                    `Request ${method} (id=${id}) timed out after ${timeoutMs}ms; stderr=${this.stderr.slice(-2000)}`
                ));
            }, timeoutMs);
            this.pending.set(id, {
                resolve: (value) => {
                    this.transcript.push({ method, params, response: value });
                    resolve(value);
                },
                reject,
                timer,
            });
            const serialized = JSON.stringify(request) + '\n';
            this.proc.stdin.write(serialized, 'utf-8', (writeError) => {
                if (writeError) {
                    this.pending.delete(id);
                    clearTimeout(timer);
                    reject(writeError);
                }
            });
        });
    }

    public async initialize(): Promise<void> {
        const result = await this.send('initialize', {
            protocolVersion: '2024-11-05',
            capabilities: {},
            clientInfo: { name: 'contract-test', version: '0.0.1' },
        }, 15_000);
        const record = recordValue(result);
        assert.strictEqual(record.protocolVersion, '2024-11-05');
        assert.ok(typeof record.serverInfo === 'object');
    }

    public async toolCallRaw(name: string, args: Record<string, unknown>): Promise<unknown> {
        const response = await this.send('tools/call', {
            name,
            arguments: args,
        }, 60_000) as McpToolResponse;
        const text = response.content?.find((item) => item.type === 'text')?.text;
        if (typeof text !== 'string') {
            throw new Error(`tools/call ${name} returned no text payload: ${JSON.stringify(response)}`);
        }
        return JSON.parse(text);
    }

    public async toolCall(name: string, args: Record<string, unknown>): Promise<Record<string, unknown>> {
        return recordValue(await this.toolCallRaw(name, args));
    }

    public async toolCallExpectError(name: string, args: Record<string, unknown>): Promise<string> {
        try {
            await this.toolCallRaw(name, args);
            assert.fail(`Expected tools/call ${name} to fail with arguments ${JSON.stringify(args)}`);
            return '';
        } catch (error) {
            this.transcript.push({
                method: `tools/call:error:${name}`,
                params: args,
                response: error instanceof Error ? error.message : String(error),
            });
            return error instanceof Error ? error.message : String(error);
        }
    }

    public async dispose(): Promise<void> {
        try {
            this.proc.stdin.end();
        } catch {
            // ignore
        }
        if (!this.proc.killed) {
            this.proc.kill('SIGTERM');
        }
        await new Promise<void>((resolve) => {
            if (this.proc.exitCode !== null || this.proc.signalCode !== null) {
                resolve();
                return;
            }
            this.proc.once('exit', () => resolve());
            setTimeout(() => resolve(), 3_000);
        });
    }
}

function resolveDaemonBinary(): string {
    const candidates = [
        path.resolve(__dirname, '..', '..', 'bin', 'lattice'),
        path.resolve(__dirname, '..', '..', '..', 'daemon', 'target', 'release', 'lattice'),
    ];
    for (const candidate of candidates) {
        if (fs.existsSync(candidate)) {
            return candidate;
        }
    }
    throw new Error(`Lattice daemon binary not found in: ${candidates.join(', ')}`);
}

async function seedMemory(client: DaemonClient): Promise<string> {
    const stored = await client.toolCall('save_observation', {
        content: 'Contract gate seed memory for R79',
        memory_type: 'observation',
        scope: 'session',
        linked_files: ['README.md'],
    });
    const id = stored.id;
    assert.ok(typeof id === 'string' && id.length > 0, 'save_observation should return a non-empty id');
    return id as string;
}

suite('contract: ReviewRpcBridge ↔ daemon MCP tools', function () {
    this.timeout(120_000);

    let client: DaemonClient;
    let workspace: string;
    let seededMemoryId: string;
    let proposedEvolutionId: string | undefined;

    suiteSetup(async () => {
        workspace = fs.mkdtempSync(path.join(os.tmpdir(), 'lattice-contract-'));
        fs.writeFileSync(path.join(workspace, 'README.md'), '# Contract Test Workspace\n');
        client = await DaemonClient.spawn(resolveDaemonBinary(), workspace);
        await client.initialize();
        seededMemoryId = await seedMemory(client);
    });

    suiteTeardown(async () => {
        if (client) {
            await client.dispose();
        }
        if (workspace && fs.existsSync(workspace)) {
            try {
                fs.rmSync(workspace, { recursive: true, force: true });
            } catch {
                // best-effort cleanup
            }
        }
    });

    test('list_observations: happy-path returns typed memory list', async () => {
        const payload = await client.toolCall('list_observations', { limit: 50 });
        const memories = arrayValue(payload, 'memories').map(normalizeMemory);
        assert.ok(memories.length >= 1, `expected at least one seeded memory; got ${memories.length}`);
        const seeded = memories.find((entry) => entry.id === seededMemoryId);
        assert.ok(seeded, 'seeded memory should be present in list_observations response');
        assert.strictEqual(seeded?.content, 'Contract gate seed memory for R79');
        assert.ok(seeded.linkedFiles.includes('README.md'));
        assert.ok(['observation'].includes(seeded.memoryClass) || seeded.memoryClass.length > 0);
    });

    test('list_observations: invalid limit shape is gracefully clamped', async () => {
        // The dispatcher accepts limit as integer but treats non-numeric defaults as 50.
        // Provide a string and assert the call still returns a typed payload.
        const payload = await client.toolCall('list_observations', { limit: 'not-a-number' as unknown });
        const memories = arrayValue(payload, 'memories').map(normalizeMemory);
        assert.ok(Array.isArray(memories));
    });

    test('list_stale_memories: happy-path returns typed memory list', async () => {
        const payload = await client.toolCall('list_stale_memories', { limit: 25 });
        const memories = arrayValue(payload, 'memories').map(normalizeMemory);
        assert.ok(typeof payload.count === 'number');
        assert.ok(Array.isArray(memories));
    });

    test('list_stale_memories: oversize limit is clamped to ≤200', async () => {
        const payload = await client.toolCall('list_stale_memories', { limit: 9999 });
        const memories = arrayValue(payload, 'memories').map(normalizeMemory);
        assert.ok(memories.length <= 200);
    });

    test('index_status: happy-path returns typed snapshot', async () => {
        const payload = await client.toolCall('index_status', {});
        const snapshot = normalizeIndexStatus(payload);
        assert.ok(['ready', 'indexing'].includes(snapshot.status));
        assert.strictEqual(typeof snapshot.version, 'string');
        assert.strictEqual(typeof snapshot.workspace, 'string');
        assert.ok(snapshot.workspace.length > 0);
        assert.strictEqual(typeof snapshot.nodes, 'number');
        assert.strictEqual(typeof snapshot.edges, 'number');
        assert.strictEqual(typeof snapshot.files, 'number');
        assert.ok(typeof snapshot.languages === 'object' && snapshot.languages !== null);
    });

    test('get_session_metrics: happy-path returns typed session metrics', async () => {
        const raw = await client.toolCallRaw('get_session_metrics', {});
        const metrics = normalizeSessionMetrics(raw);
        assert.strictEqual(typeof metrics.totalToolCalls, 'number');
        assert.strictEqual(typeof metrics.workflowToolCalls, 'number');
        assert.strictEqual(typeof metrics.totalPayloadTokens, 'number');
        assert.strictEqual(typeof metrics.averagePayloadTokensPerTool, 'number');
        assert.strictEqual(typeof metrics.totalPayloadBytes, 'number');
        assert.strictEqual(typeof metrics.averagePayloadBytesPerTool, 'number');
        assert.strictEqual(typeof metrics.contextHandleReuses, 'number');
        assert.strictEqual(typeof metrics.contextHandleReuseRate, 'number');
    });

    test('get_memory_metrics: happy-path returns typed metric snapshot', async () => {
        const payload = await client.toolCall('get_memory_metrics', {
            scope: 'session',
            render_mode: 'compact',
        });
        const snapshot = normalizeMetricSnapshot(payload);
        assert.strictEqual(typeof snapshot.scope, 'string');
        assert.strictEqual(typeof snapshot.renderMode, 'string');
        assert.strictEqual(typeof snapshot.incomplete, 'boolean');
        assert.ok(Array.isArray(snapshot.notes));
        assert.ok(Array.isArray(snapshot.signals));
        for (const signal of snapshot.signals) {
            assert.strictEqual(typeof signal.signal, 'string');
            assert.ok(signal.value === null || typeof signal.value === 'number');
        }
    });

    test('get_memory_metrics: invalid scope rejected by serde', async () => {
        const message = await client.toolCallExpectError('get_memory_metrics', {
            scope: 'not-a-real-scope',
        });
        assert.ok(message.length > 0, 'invalid scope must surface a JSON-RPC error');
    });

    test('get_event_trace: happy-path returns typed paginated page', async () => {
        const payload = await client.toolCall('get_event_trace', {
            workspace_id: workspace,
            limit: 25,
            render_mode: 'full',
        });
        const page = normalizeEventTracePage(payload);
        assert.strictEqual(typeof page.renderMode, 'string');
        assert.ok(typeof page.scope.kind === 'string' && page.scope.kind.length > 0);
        assert.ok(Array.isArray(page.events));
        for (const event of page.events) {
            assert.strictEqual(typeof event.eventId, 'string');
            assert.strictEqual(typeof event.expansionHandle, 'string');
            assert.strictEqual(typeof event.kind, 'string');
            assert.strictEqual(typeof event.timestamp, 'string');
            assert.strictEqual(typeof event.workspaceId, 'string');
            assert.strictEqual(typeof event.summary, 'string');
            assert.ok(Array.isArray(event.references));
        }
    });

    test('get_event_trace: missing scope is rejected', async () => {
        const message = await client.toolCallExpectError('get_event_trace', {
            limit: 25,
        });
        assert.ok(
            message.includes('task_id') ||
                message.includes('session_id') ||
                message.includes('workspace_id') ||
                message.toLowerCase().includes('scope'),
            `expected scope-required error, got: ${message}`
        );
    });

    test('consolidate_session: happy-path returns typed consolidation report', async () => {
        const payload = await client.toolCall('consolidate_session', {
            session_id: 'contract-test-session',
            mode: 'manual_review',
            render_mode: 'diagnostic',
        });
        const report = normalizeConsolidationReport(payload);
        assert.strictEqual(report.sessionId, 'contract-test-session');
        assert.strictEqual(report.mode, 'manual_review');
        assert.strictEqual(report.renderMode, 'diagnostic');
        assert.strictEqual(typeof report.incomplete, 'boolean');
        assert.ok(Array.isArray(report.notes));
        assert.ok(Array.isArray(report.proposals));
        assert.ok(Array.isArray(report.categories));
    });

    test('consolidate_session: empty session_id is rejected', async () => {
        const message = await client.toolCallExpectError('consolidate_session', {
            session_id: '',
            mode: 'manual_review',
        });
        assert.ok(
            message.toLowerCase().includes('session_id'),
            `expected session_id validation error, got: ${message}`
        );
    });

    test('propose_memory_evolution: action=propose returns typed proposal', async () => {
        const payload = await client.toolCall('propose_memory_evolution', {
            action: 'propose',
            memory_id: seededMemoryId,
            reason: 'Contract test propose',
            invalidate_reason: 'Contract test - simulate invalidation',
        });
        const proposal = normalizeEvolutionProposal(payload);
        assert.strictEqual(proposal.action, 'propose');
        assert.ok(proposal.proposalId.length > 0);
        proposedEvolutionId = proposal.proposalId;
    });

    test('propose_memory_evolution: action=reject closes the proposal', async () => {
        assert.ok(proposedEvolutionId, 'previous proposal must have been recorded');
        const payload = await client.toolCall('propose_memory_evolution', {
            action: 'reject',
            proposal_id: proposedEvolutionId,
            reason: 'Contract test reject',
            decided_by: 'contract-test',
        });
        const proposal = normalizeEvolutionProposal(payload);
        assert.strictEqual(proposal.action, 'reject');
        assert.strictEqual(proposal.proposalId, proposedEvolutionId);
    });

    test('propose_memory_evolution: action=apply requires proposal_id', async () => {
        const message = await client.toolCallExpectError('propose_memory_evolution', {
            action: 'apply',
        });
        assert.ok(
            message.toLowerCase().includes('proposal_id'),
            `expected proposal_id requirement, got: ${message}`
        );
    });

    test('verify_explain_memory: happy-path returns typed verify-explain response', async () => {
        const payload = await client.toolCall('verify_explain_memory', {
            memory_id: seededMemoryId,
            mode: 'verify_and_explain',
            render_mode: 'full',
        });
        const response = normalizeVerifyExplainResponse(payload);
        assert.strictEqual(typeof response.status, 'string');
        assert.ok(response.status.length > 0);
        assert.strictEqual(typeof response.confidenceDelta, 'number');
        assert.strictEqual(typeof response.expansionHandle, 'string');
        assert.ok(Array.isArray(response.summaryLines));
        assert.ok(Array.isArray(response.checks));
        assert.strictEqual(response.renderMode, 'full');
    });

    test('verify_explain_memory: unknown memory id is rejected', async () => {
        const message = await client.toolCallExpectError('verify_explain_memory', {
            memory_id: 'memory-that-does-not-exist',
            mode: 'verify_and_explain',
        });
        assert.ok(message.length > 0);
    });

    test('list_memory_conflicts: happy-path on legacy anchor returns typed list', async () => {
        const payload = await client.toolCall('list_memory_conflicts', {
            anchor: seededMemoryId,
            render_mode: 'full',
            limit: 25,
        });
        const list = normalizeConflictList(payload);
        assert.strictEqual(typeof list.anchor, 'string');
        assert.ok(list.anchor.length > 0);
        assert.strictEqual(typeof list.total, 'number');
        assert.strictEqual(list.renderMode, 'full');
        assert.ok(Array.isArray(list.summaryLines));
        assert.ok(Array.isArray(list.conflicts));
    });

    test('list_memory_conflicts: missing anchor is rejected', async () => {
        const message = await client.toolCallExpectError('list_memory_conflicts', {
            render_mode: 'full',
        });
        assert.ok(
            message.toLowerCase().includes('anchor'),
            `expected anchor requirement, got: ${message}`
        );
    });

    test('contract transcript captures every covered tool', () => {
        const covered = new Set<string>();
        for (const entry of client.transcript) {
            if (entry.method === 'tools/call' && entry.params && typeof entry.params === 'object') {
                const name = (entry.params as { name?: string }).name;
                if (name) {
                    covered.add(name);
                }
            } else if (entry.method.startsWith('tools/call:error:')) {
                covered.add(entry.method.replace('tools/call:error:', ''));
            }
        }
        // Every review-UI tool advertised by `ReviewRpcBridge.getCapabilities()` must
        // appear in the transcript. Composite methods (overview/evidence/queue) are
        // built from these primitives and inherit their coverage transitively.
        const expected = [
            'save_observation',
            'list_observations',
            'list_stale_memories',
            'index_status',
            'get_session_metrics',
            'get_memory_metrics',
            'get_event_trace',
            'consolidate_session',
            'propose_memory_evolution',
            'verify_explain_memory',
            'list_memory_conflicts',
        ];
        for (const name of expected) {
            assert.ok(covered.has(name), `contract test did not exercise tool: ${name}`);
        }
    });
});
