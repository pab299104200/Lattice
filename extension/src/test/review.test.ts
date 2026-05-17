import * as assert from 'assert';
import * as vscode from 'vscode';
import { suite, test } from 'mocha';
import { createReviewI18n } from '../review/i18n';
import { mountContradictionQueue } from '../review/contradictionQueue';
import { mountPromotionQueue } from '../review/promotionQueue';
import {
    getReviewPanelProviderForTests,
    ReviewRouteId,
    ReviewStateMessage,
    setBridgeForTests,
} from '../review/reviewPanel';
import {
    ReviewBridgeCapabilities,
    ReviewContradictionDecisionArgs,
    ReviewResult,
    ReviewRpcBridgeContract,
} from '../review/rpcBridge';
import {
    ReviewConflictList,
    ReviewConsolidationQueueData,
    ReviewConsolidationReport,
    ReviewEventTracePage,
    ReviewEvolutionProposal,
    ReviewIndexingHealth,
    ReviewMemory,
    ReviewMemoryEvidenceBundle,
    ReviewMemoryList,
    ReviewOverview,
    ReviewRetrievalExplanation,
    ReviewVerifyExplainResponse,
    ReviewWorkspaceGraphHealth,
} from '../review/rpcPayloads';

type QueueRoute = 'promotionQueue' | 'contradictionQueue';
type ErrorSpy = {
    calls: string[];
    restore: () => void;
};

class StubBridge implements ReviewRpcBridgeContract {
    private mode: 'success' | 'empty' | 'error' = 'success';

    public dispose(): void {}

    public setMode(mode: 'success' | 'empty' | 'error'): void {
        this.mode = mode;
    }

    public setWebview(): void {}

    public getCapabilities(): ReviewBridgeCapabilities {
        const direct = { mode: 'direct', reason: 'stub' } as const;
        const unsupported = { mode: 'unsupported', reason: 'stub' } as const;
        return {
            memoryInbox: direct,
            promotionQueue: direct,
            contradictionQueue: direct,
            staleView: direct,
            evidenceInspector: direct,
            eventTrace: direct,
            retrievalExplanation: unsupported,
            usefulnessMetrics: direct,
            workspaceGraphHealth: direct,
            indexingHealth: direct,
            consolidationQueue: direct,
        };
    }

    public async getOverview(): Promise<ReviewResult<ReviewOverview>> {
        return ok({
            indexStatus: baseIndexStatus(),
            memoryList: await unwrapAsync(this.listMemories()),
            metrics: {
                scope: 'session',
                renderMode: 'compact',
                incomplete: false,
                notes: [],
                signals: [{ signal: 'memory_later_used_rate', value: 0.8 }],
            },
            sessionMetrics: {
                totalToolCalls: 3,
                workflowToolCalls: 2,
                totalPayloadTokens: 120,
                averagePayloadTokensPerTool: 40,
                totalPayloadBytes: 512,
                averagePayloadBytesPerTool: 170,
                contextHandleReuses: 1,
                contextHandleReuseRate: 0.5,
            },
        });
    }

    public async listMemories(): Promise<ReviewResult<ReviewMemoryList>> {
        return maybeFail({
            count: this.mode === 'empty' ? 0 : 1,
            memories: this.mode === 'empty' ? [] : [baseMemory()],
        }, this.mode);
    }

    public async listPromotionProposals(): Promise<ReviewResult<ReviewConsolidationReport>> {
        return maybeFail({
            sessionId: 'session-1',
            mode: 'manual_review',
            renderMode: 'compact',
            incomplete: false,
            notes: [],
            proposals: this.mode === 'empty' ? [] : [baseConsolidationProposal()],
            categories: [],
        }, this.mode);
    }

    public async applyPromotion(
        _proposalId: string,
        _reason?: string
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return ok(baseEvolutionProposal());
    }

    public async rejectPromotion(
        _proposalId: string,
        _reason: string
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return ok(baseEvolutionProposal());
    }

    public async listContradictions(): Promise<ReviewResult<ReviewConflictList>> {
        return maybeFail({
            anchor: 'memory-1',
            total: this.mode === 'empty' ? 0 : 1,
            renderMode: 'compact',
            summaryLines: [],
            conflicts: this.mode === 'empty' ? [] : [baseConflict()],
        }, this.mode);
    }

    public async applyContradictionResolution(
        _args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return ok(baseEvolutionProposal());
    }

    public async rejectContradictionResolution(
        _args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return ok(baseEvolutionProposal());
    }

    public async listStaleMemories(): Promise<ReviewResult<ReviewMemoryList>> {
        return maybeFail({
            count: this.mode === 'empty' ? 0 : 1,
            memories: this.mode === 'empty' ? [] : [{ ...baseMemory(), verificationStatus: 'stale', staleReason: 'stale source' }],
        }, this.mode);
    }

    public async getMemoryEvidence(memoryId: string): Promise<ReviewResult<ReviewMemoryEvidenceBundle>> {
        return maybeFail({
            memory: { ...baseMemory(), id: memoryId },
            provenanceEvents: this.mode === 'empty' ? [] : [baseEvent('memory_retrieved')],
            lastVerification: baseVerification(),
        }, this.mode);
    }

    public async verifyMemory(): Promise<ReviewResult<ReviewVerifyExplainResponse>> {
        return ok(baseVerification());
    }

    public async getEventTrace(): Promise<ReviewResult<ReviewEventTracePage>> {
        return maybeFail({
            scope: { kind: 'memory', value: 'memory-1' },
            renderMode: 'full',
            events: this.mode === 'empty' ? [] : [baseEvent('memory_retrieved')],
        }, this.mode);
    }

    public async getRetrievalExplanation(requestId: string): Promise<ReviewResult<ReviewRetrievalExplanation>> {
        return maybeFail({
            requestId,
            supported: false,
            reason: this.mode === 'empty'
                ? 'No retrieval candidates recorded yet.'
                : 'No public retrieval explanation tool.',
            request: {
                toolName: 'get_context_capsule',
                taskStatement: 'Explain retrieval',
                intentClassification: 'diagnostic',
                timestamp: '2026-05-17T12:00:00Z',
            },
            anchors: this.mode === 'empty' ? [] : [{
                kind: 'memory',
                label: 'memory-1',
                provenance: 'stub',
            }],
            candidates: [],
            excludedCandidates: [],
        }, this.mode);
    }

    public async listConsolidationJobs(): Promise<ReviewResult<ReviewConsolidationQueueData>> {
        return maybeFail({
            workspaceId: '/workspace',
            jobs: this.mode === 'empty' ? [] : [{
                jobId: 'job-1',
                status: 'pending',
                kind: 'promotion',
                mode: 'manual_review',
                sessionId: 'session-1',
                taskId: 'task-1',
                createdAt: '2026-05-17T12:00:00Z',
                resultSummary: 'Waiting for review',
            }],
            queueDepth: baseQueueDepth(),
        }, this.mode);
    }

    public async getConsolidationQueueDepth() {
        return maybeFail(baseQueueDepth(), this.mode);
    }

    public async getIndexingHealth(): Promise<ReviewResult<ReviewIndexingHealth>> {
        return maybeFail({
            snapshot: baseIndexStatus(),
            parserHealth: this.mode === 'empty' ? [] : [{
                language: 'typescript',
                parsedCount: 12,
                failedCount: 0,
            }],
            watcherStatus: 'watching',
            lastFullRescanAt: '2026-05-17T12:00:00Z',
            vectorIndex: {
                totalEmbeddings: 10,
                lastRebuildAt: '2026-05-17T12:00:00Z',
                averageFreshness: 'fresh',
                staleEmbeddings: 0,
                notes: [],
            },
            ftsIndex: {
                rows: this.mode === 'empty' ? [] : [{ label: 'docs', count: 4 }],
                lastRebuildAt: '2026-05-17T12:00:00Z',
                orphanRows: 0,
                notes: [],
            },
            eventLog: {
                lastHourCount: 1,
                last24HoursCount: 2,
                last7DaysCount: 3,
                lastCompactionAt: '2026-05-17T12:00:00Z',
                spilloverRows: 0,
                notes: [],
            },
        }, this.mode);
    }

    public async getWorkspaceGraphHealth(): Promise<ReviewResult<ReviewWorkspaceGraphHealth>> {
        return maybeFail({
            snapshot: baseIndexStatus(),
            nodeFamilies: this.mode === 'empty' ? [] : [{ family: 'File', count: 10 }],
            edgeFamilies: this.mode === 'empty' ? [] : [{ family: 'contains', count: 9 }],
            brokenReferences: [],
            staleEdges: [],
            orphanSymbols: [],
            notes: [],
        }, this.mode);
    }

    public async retryConsolidationSession(): Promise<ReviewResult<ReviewConsolidationReport>> {
        return ok({
            sessionId: 'session-1',
            mode: 'manual_review',
            renderMode: 'compact',
            incomplete: false,
            notes: [],
            proposals: [baseConsolidationProposal()],
            categories: [],
        });
    }
}

suite('review panel smoke', () => {
    const bridge = new StubBridge();
    setBridgeForTests(() => bridge);

    test('activates the extension and registers lattice.openReviewPanel', async () => {
        const extension = await activateExtension();
        assert.ok(extension);
        const commands = await vscode.commands.getCommands(true);
        assert.ok(commands.includes('lattice.openReviewPanel'));
    });

    test('focuses the registered review panel without error', async () => {
        await activateExtension();
        await assert.doesNotReject(async () => {
            await vscode.commands.executeCommand('lattice.reviewPanel.focus');
        });
    });

    registerRouteTests(bridge);
    registerQueueTests(bridge);
});

function registerRouteTests(bridge: StubBridge): void {
    for (const spec of routeSpecs()) {
        test(`mounts ${spec.route} without throwing`, async () => {
            bridge.setMode('success');
            const state = await buildRouteState(spec);
            assertRouteMarkup(state, spec.testId);
        });

        test(`mounts ${spec.route} with empty data`, async () => {
            bridge.setMode('empty');
            const state = await buildRouteState(spec);
            assertRouteMarkup(state, spec.testId);
            assert.ok(hasEmptyStateText(spec.route, state.routeView?.html ?? ''));
        });

        test(`mounts ${spec.route} with bridge error`, async () => {
            bridge.setMode('error');
            const errorSpy = spyShowErrorMessage();
            try {
                const state = await buildRouteState(spec);
                assertRouteMarkup(state, spec.testId);
                assert.ok((state.routeView?.html ?? '').includes('role="alert"'));
                assert.ok(errorSpy.calls.length > 0);
            } finally {
                errorSpy.restore();
            }
        });
    }
}

function registerQueueTests(bridge: StubBridge): void {
    for (const spec of queueSpecs()) {
        test(`mounts ${spec.route} without throwing`, async () => {
            bridge.setMode('success');
            const html = await mountQueue(spec.route, bridge);
            assert.ok(html.includes(spec.testId));
        });

        test(`mounts ${spec.route} with empty data`, async () => {
            bridge.setMode('empty');
            const html = await mountQueue(spec.route, bridge);
            assert.ok(html.includes(spec.testId));
            assert.ok(hasQueueEmptyStateText(spec.route, html));
        });

        test(`mounts ${spec.route} with bridge error`, async () => {
            bridge.setMode('error');
            const errorMessages: string[] = [];
            const html = await mountQueue(spec.route, bridge, errorMessages);
            assert.ok(html.includes(spec.testId));
            assert.ok(html.includes('role="alert"'));
            assert.ok(errorMessages.length > 0);
        });
    }
}

function routeSpecs(): Array<{
    route: ReviewRouteId;
    testId: string;
    memoryId?: string;
    requestId?: string;
}> {
    return [
        { route: 'memoryInbox', testId: 'review-memory-inbox' },
        { route: 'staleView', testId: 'review-stale-view' },
        { route: 'evidenceInspector', testId: 'review-evidence-inspector', memoryId: 'memory-1' },
        { route: 'eventTrace', testId: 'review-event-trace', memoryId: 'memory-1' },
        { route: 'retrievalExplanation', testId: 'review-retrieval-explanation', requestId: 'request-1' },
        { route: 'consolidationQueue', testId: 'review-consolidation-queue' },
        { route: 'indexingHealth', testId: 'review-indexing-health' },
        { route: 'workspaceGraphHealth', testId: 'review-workspace-graph-health' },
    ];
}

function queueSpecs(): Array<{ route: QueueRoute; testId: string }> {
    return [
        { route: 'promotionQueue', testId: 'review-promotion-queue' },
        { route: 'contradictionQueue', testId: 'review-contradiction-queue' },
    ];
}

async function activateExtension() {
    const extension = vscode.extensions.getExtension('lattice.lattice');
    assert.ok(extension, 'lattice.lattice extension not found');
    return extension.activate();
}

async function buildRouteState(spec: {
    route: ReviewRouteId;
    memoryId?: string;
    requestId?: string;
}): Promise<ReviewStateMessage> {
    await activateExtension();
    const provider = getReviewPanelProviderForTests();
    assert.ok(provider, 'review panel provider should exist after activation');
    return provider.buildStateForTests({
        route: spec.route,
        memoryId: spec.memoryId,
        requestId: spec.requestId,
    });
}

function assertRouteMarkup(state: ReviewStateMessage, expectedTestId: string): void {
    const html = state.routeView?.html ?? '';
    assert.ok(html.includes(`data-testid="${expectedTestId}"`), `expected ${expectedTestId} in route markup`);
}

function hasEmptyStateText(route: ReviewRouteId, html: string): boolean {
    const i18n = createReviewI18n();
    const expectations: Partial<Record<ReviewRouteId, string[]>> = {
        memoryInbox: [i18n.t('memoryInbox.emptyTitle')],
        staleView: [i18n.t('staleView.empty')],
        evidenceInspector: [i18n.t('evidenceInspector.noEvidence')],
        eventTrace: [i18n.t('eventTraceView.empty')],
        retrievalExplanation: [i18n.t('retrievalExplanationView.unsupportedTitle')],
        consolidationQueue: [i18n.t('consolidationQueueView.empty')],
        indexingHealth: [i18n.t('indexingHealthView.empty')],
        workspaceGraphHealth: [i18n.t('workspaceGraphHealthView.brokenReferences.empty')],
    };
    return (expectations[route] ?? []).some((value) => html.includes(value));
}

async function mountQueue(
    route: QueueRoute,
    bridge: StubBridge,
    reportedErrors: string[] = []
): Promise<string> {
    const document = new FakeDocument();
    const host = document.createElement('div');
    const globals = globalThis as Record<string, unknown>;
    const originalDocument = globals.document;
    globals.document = document;
    globals.renderStatusBadge = (status: string) => `<span>${status}</span>`;
    globals.openProposalDialog = () => ({ close() {}, showModal() {} });
    globals.reportReviewViewError = (_route: string, message: string) => {
        reportedErrors.push(message);
    };
    try {
        if (route === 'promotionQueue') {
            mountPromotionQueue(host as unknown as Parameters<typeof mountPromotionQueue>[0], {
                listPromotionProposals: async () => unwrap(await bridge.listPromotionProposals()) as unknown as Record<string, unknown>,
                applyPromotion: async () => unwrap(await bridge.applyPromotion('proposal-1')),
                rejectPromotion: async () => unwrap(await bridge.rejectPromotion('proposal-1', 'reason')),
            }, createReviewI18n());
        } else {
            mountContradictionQueue(host as unknown as Parameters<typeof mountContradictionQueue>[0], {
                listContradictions: async () => unwrap(await bridge.listContradictions()) as unknown as Record<string, unknown>,
                getMemoryEvidence: async (memoryId: string) => unwrap(await bridge.getMemoryEvidence(memoryId)) as unknown as Record<string, unknown>,
                applyContradictionResolution: async () => unwrap(await bridge.applyContradictionResolution(baseDecisionArgs())),
                rejectContradictionResolution: async () => unwrap(await bridge.rejectContradictionResolution(baseDecisionArgs())),
            }, createReviewI18n());
        }
        await flushPromises();
        return host.outerHTML;
    } finally {
        globals.document = originalDocument;
        delete globals.renderStatusBadge;
        delete globals.openProposalDialog;
        delete globals.reportReviewViewError;
    }
}

function hasQueueEmptyStateText(route: QueueRoute, html: string): boolean {
    const i18n = createReviewI18n();
    const emptyState = route === 'promotionQueue'
        ? i18n.t('promotionQueue.empty')
        : i18n.t('contradictionQueue.empty');
    return html.includes(emptyState);
}

function spyShowErrorMessage(): ErrorSpy {
    const original = vscode.window.showErrorMessage.bind(vscode.window);
    const calls: string[] = [];
    const replacement = async (message: string) => {
        calls.push(message);
        return undefined;
    };
    Object.defineProperty(vscode.window, 'showErrorMessage', {
        value: replacement,
        configurable: true,
    });
    return {
        calls,
        restore() {
            Object.defineProperty(vscode.window, 'showErrorMessage', {
                value: original,
                configurable: true,
            });
        },
    };
}

function baseMemory(): ReviewMemory {
    return {
        id: 'memory-1',
        sessionId: 'session-1',
        content: 'Memory content',
        memoryClass: 'observation',
        assertionType: 'observation',
        scope: 'session',
        confidence: 0.8,
        confidenceReason: 'Observed in tests',
        verificationStatus: 'verified',
        freshnessStatus: 'fresh',
        contradictionState: 'clear',
        supersessionState: 'active',
        inclusionReason: 'Relevant',
        evidenceStrength: 0.6,
        linkedFiles: ['extension/src/review/reviewPanel.ts'],
        linkedSymbols: ['ReviewPanelProvider'],
        linkedDocs: [],
        linkedTests: [],
        linkedMemories: [],
        validityConditions: [],
        invalidationTriggers: [],
        createdAt: 1_715_600_000,
        lastVerifiedAt: 1_715_600_100,
        evidence: [],
        links: [],
        provenance: [],
        accessHistory: [],
        usefulnessScores: [],
    };
}

function baseIndexStatus() {
    return {
        status: 'ready',
        version: '1',
        workspace: '/workspace',
        nodes: 10,
        edges: 9,
        files: 4,
        languages: { typescript: 4 },
        multiRepo: false,
        workspaces: ['/workspace'],
        repos: [],
    };
}

function baseEvent(kind: string) {
    return {
        eventId: 'event-1',
        expansionHandle: 'handle-1',
        kind,
        timestamp: '2026-05-17T12:00:00Z',
        actor: 'assistant',
        branch: 'main',
        sessionId: 'session-1',
        taskId: 'task-1',
        workspaceId: '/workspace',
        summary: 'Event summary',
        references: [],
        payload: { ok: true },
    };
}

function baseVerification(): ReviewVerifyExplainResponse {
    return {
        status: 'verified',
        confidenceDelta: 0,
        expansionHandle: 'handle-1',
        summaryLines: ['Verified'],
        renderMode: 'compact',
        checks: [],
    };
}

function baseConflict() {
    return {
        source: 'memory-1',
        target: 'memory-2',
        linkType: 'contradicts',
        linkStrength: 0.9,
        createdBy: 'review-test',
        createdAt: 1_715_600_000,
        linkVerificationStatus: 'pending',
        reason: 'Conflicting evidence',
    };
}

function baseConsolidationProposal() {
    return {
        proposalId: 'proposal-1',
        jobId: 'job-1',
        proposalKind: 'promotion',
        taskId: 'task-1',
        category: 'memory',
        summary: 'Promote memory',
        decision: 'pending',
        proposedClass: 'pattern',
        currentScope: 'session',
        targetScope: 'repo',
        confidence: 0.8,
        evidenceCount: 2,
    };
}

function baseQueueDepth() {
    return {
        currentDepth: 1,
        maxDepth: 4,
        droppedCount: 0,
        notes: [],
    };
}

function baseEvolutionProposal(): ReviewEvolutionProposal {
    return {
        proposalId: 'proposal-1',
        action: 'apply',
        proposalKind: 'promotion',
        decision: 'accepted',
        priorState: {},
        proposedState: {},
    };
}

function baseDecisionArgs(): ReviewContradictionDecisionArgs {
    return {
        sourceMemoryId: 'memory-1',
        targetMemoryId: 'memory-2',
        linkType: 'contradicts',
    };
}

function ok<T>(value: T): Promise<ReviewResult<T>> {
    return Promise.resolve({ ok: true, value });
}

function maybeFail<T>(value: T, mode: 'success' | 'empty' | 'error'): Promise<ReviewResult<T>> {
    if (mode === 'error') {
        return Promise.resolve({
            ok: false,
            error: { code: 'rpc_error', message: 'Review bridge failure' },
        });
    }
    return ok(value);
}

async function unwrapAsync<T>(result: Promise<ReviewResult<T>>): Promise<T> {
    return unwrap(await result);
}

function unwrap<T>(result: ReviewResult<T>): T {
    if (!result.ok) {
        throw new Error(result.error.message);
    }
    return result.value;
}

async function flushPromises(): Promise<void> {
    await new Promise((resolve) => setTimeout(resolve, 0));
    await new Promise((resolve) => setTimeout(resolve, 0));
}

class FakeDocument {
    public createElement(tagName: string): FakeElement {
        return new FakeElement(tagName);
    }
}

class FakeElement {
    public children: FakeElement[] = [];
    public className = '';
    public textContent = '';
    public type = '';
    private attributes = new Map<string, string>();
    private inlineHtml = '';

    constructor(private readonly tagName: string) {}

    public append(...nodes: Array<FakeElement | string>): void {
        for (const node of nodes) {
            if (typeof node === 'string') {
                this.inlineHtml += escapeHtml(node);
            } else {
                this.children.push(node);
            }
        }
    }

    public appendChild(node: FakeElement): FakeElement {
        this.children.push(node);
        return node;
    }

    public addEventListener(): void {}

    public setAttribute(name: string, value: string): void {
        this.attributes.set(name, value);
    }

    public set innerHTML(value: string) {
        this.inlineHtml = value;
        this.children = [];
    }

    public get innerHTML(): string {
        if (this.inlineHtml) {
            return this.inlineHtml;
        }
        return [
            this.textContent ? escapeHtml(this.textContent) : '',
            ...this.children.map((child) => child.outerHTML),
        ].join('');
    }

    public get outerHTML(): string {
        const attributes = [
            this.className ? ` class="${escapeHtml(this.className)}"` : '',
            this.type ? ` type="${escapeHtml(this.type)}"` : '',
            ...[...this.attributes.entries()].map(([key, value]) => ` ${key}="${escapeHtml(value)}"`),
        ].join('');
        return `<${this.tagName}${attributes}>${this.innerHTML}</${this.tagName}>`;
    }
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
