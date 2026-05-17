import * as vscode from 'vscode';
import { DaemonManager } from '../daemon';
import {
    arrayValue,
    matchesMemoryFilters,
    normalizeConflictList,
    normalizeConsolidationReport,
    normalizeEventTracePage,
    normalizeEvolutionProposal,
    normalizeIndexStatus,
    normalizeMemory,
    normalizeMetricSnapshot,
    normalizeSessionMetrics,
    normalizeVerifyExplainResponse,
    numberValue,
    recordValue,
    ReviewConflictList,
    ReviewConsolidationJob,
    ReviewConsolidationQueueData,
    ReviewConsolidationReport,
    ReviewEventTracePage,
    ReviewEvolutionProposal,
    ReviewGraphDiagnosticRow,
    ReviewIndexingHealth,
    ReviewIndexStatus,
    ReviewMemory,
    ReviewMemoryEvidenceBundle,
    ReviewQueueDepthSnapshot,
    ReviewMemoryList,
    ReviewMetricSnapshot,
    ReviewOverview,
    ReviewRetrievalExplanation,
    ReviewSessionMetrics,
    ReviewVerifyExplainResponse,
    ReviewWorkspaceGraphHealth,
} from './rpcPayloads';

interface McpToolContentItem {
    type?: string;
    text?: string;
}

interface McpToolResponse {
    content?: McpToolContentItem[];
}

type ReviewErrorCode =
    | 'daemon_not_running'
    | 'rpc_error'
    | 'parse_error'
    | 'not_found'
    | 'unsupported';

export interface ReviewRpcError {
    code: ReviewErrorCode;
    message: string;
    cause?: unknown;
}

export type ReviewResult<T> =
    | { ok: true; value: T }
    | { ok: false; error: ReviewRpcError };

export interface ReviewRouteSupport {
    mode: 'direct' | 'composed' | 'unsupported';
    reason: string;
}

export interface ReviewBridgeCapabilities {
    memoryInbox: ReviewRouteSupport;
    promotionQueue: ReviewRouteSupport;
    contradictionQueue: ReviewRouteSupport;
    staleView: ReviewRouteSupport;
    evidenceInspector: ReviewRouteSupport;
    eventTrace: ReviewRouteSupport;
    retrievalExplanation: ReviewRouteSupport;
    usefulnessMetrics: ReviewRouteSupport;
    workspaceGraphHealth: ReviewRouteSupport;
    indexingHealth: ReviewRouteSupport;
    consolidationQueue: ReviewRouteSupport;
}

export interface ReviewContradictionDecisionArgs {
    proposalId?: string;
    sourceMemoryId: string;
    targetMemoryId: string;
    linkType: string;
    reason?: string;
    detectedBy?: string;
}

export interface ReviewRpcBridgeContract extends vscode.Disposable {
    setWebview(webview: vscode.Webview | undefined): void;
    getCapabilities(): ReviewBridgeCapabilities;
    getOverview(): Promise<ReviewResult<ReviewOverview>>;
    listMemories(filters?: {
        sessionId?: string;
        status?: string[];
        scope?: string[];
        memoryClass?: string[];
        limit?: number;
    }): Promise<ReviewResult<ReviewMemoryList>>;
    listPromotionProposals(sessionId?: string): Promise<ReviewResult<ReviewConsolidationReport>>;
    applyPromotion(proposalId: string, reason?: string): Promise<ReviewResult<ReviewEvolutionProposal>>;
    rejectPromotion(proposalId: string, reason: string): Promise<ReviewResult<ReviewEvolutionProposal>>;
    listContradictions(anchor?: string): Promise<ReviewResult<ReviewConflictList>>;
    applyContradictionResolution(
        args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>>;
    rejectContradictionResolution(
        args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>>;
    listStaleMemories(query?: string, limit?: number): Promise<ReviewResult<ReviewMemoryList>>;
    getMemoryEvidence(memoryId: string): Promise<ReviewResult<ReviewMemoryEvidenceBundle>>;
    verifyMemory(memoryId: string): Promise<ReviewResult<ReviewVerifyExplainResponse>>;
    getEventTrace(args: {
        taskId?: string;
        sessionId?: string;
        workspaceId?: string;
        kinds?: string[];
        since?: string;
        until?: string;
        cursor?: string;
        limit?: number;
        renderMode?: 'compact' | 'full' | 'diagnostic';
    }): Promise<ReviewResult<ReviewEventTracePage>>;
    getRetrievalExplanation(requestId: string): Promise<ReviewResult<ReviewRetrievalExplanation>>;
    listConsolidationJobs(sessionId?: string): Promise<ReviewResult<ReviewConsolidationQueueData>>;
    getConsolidationQueueDepth(): Promise<ReviewResult<ReviewQueueDepthSnapshot>>;
    getIndexingHealth(): Promise<ReviewResult<ReviewIndexingHealth>>;
    getWorkspaceGraphHealth(): Promise<ReviewResult<ReviewWorkspaceGraphHealth>>;
    retryConsolidationSession(
        sessionId: string,
        mode?: 'background' | 'manual_review' | 'replay' | 'post_task'
    ): Promise<ReviewResult<ReviewConsolidationReport>>;
}

export type { ReviewMemory } from './rpcPayloads';

const DEFAULT_CONSOLIDATION_QUEUE_DEPTH = 128;
const WORKSPACE_GRAPH_NODE_FAMILIES = [
    'File',
    'Directory',
    'Symbol',
    'Type',
    'Module',
    'Test',
    'Document',
    'Section',
    'ConfigKey',
    'Command',
    'Route',
    'Schema',
    'Package',
    'BuildTarget',
    'RuntimeSurface',
] as const;
const WORKSPACE_GRAPH_EDGE_FAMILIES = [
    'contains',
    'imports',
    'calls',
    'implements',
    'extends',
    'type_ref',
    'tested_by',
    'documents',
    'mentions',
    'depends_on',
    'configured_by',
    'generated_by',
    'co_changed_with',
    'stale_against',
] as const;

export class ReviewRpcBridge implements ReviewRpcBridgeContract {
    private webview: vscode.Webview | undefined;

    constructor(private readonly daemon: DaemonManager) {}

    public dispose(): void {
        this.webview = undefined;
    }

    public setWebview(webview: vscode.Webview | undefined): void {
        this.webview = webview;
    }

    public getCapabilities(): ReviewBridgeCapabilities {
        return {
            memoryInbox: support('direct', 'list_observations'),
            promotionQueue: support('direct', 'consolidate_session + propose_memory_evolution'),
            contradictionQueue: support('composed', 'list_memory_conflicts'),
            staleView: support('direct', 'list_stale_memories + verify_explain_memory'),
            evidenceInspector: support('composed', 'list_observations'),
            eventTrace: support('direct', 'get_event_trace'),
            retrievalExplanation: support('unsupported', 'no public retrieval-explanation MCP tool'),
            usefulnessMetrics: support('direct', 'get_memory_metrics'),
            workspaceGraphHealth: support('composed', 'index_status'),
            indexingHealth: support('direct', 'index_status'),
            consolidationQueue: support('direct', 'consolidate_session'),
        };
    }

    public async getOverview(): Promise<ReviewResult<ReviewOverview>> {
        return this.run(async () => {
            const indexStatusResult = await this.getIndexingHealth();
            const memoryListResult = await this.listMemories({ limit: 50 });
            const metricsResult = await this.getMemoryMetrics();
            const sessionMetricsResult = await this.getSessionMetrics();
            if (!indexStatusResult.ok) {
                throw indexStatusResult.error;
            }
            if (!memoryListResult.ok) {
                throw memoryListResult.error;
            }
            if (!metricsResult.ok) {
                throw metricsResult.error;
            }
            if (!sessionMetricsResult.ok) {
                throw sessionMetricsResult.error;
            }
            return {
                indexStatus: indexStatusResult.value.snapshot,
                memoryList: memoryListResult.value,
                metrics: metricsResult.value,
                sessionMetrics: sessionMetricsResult.value,
            };
        });
    }

    public async listMemories(filters?: {
        sessionId?: string;
        status?: string[];
        scope?: string[];
        memoryClass?: string[];
        limit?: number;
    }): Promise<ReviewResult<ReviewMemoryList>> {
        return this.run(async () => {
            const args: Record<string, unknown> = {};
            if (filters?.sessionId) {
                args.session_id = filters.sessionId;
            }
            if (filters?.limit) {
                args.limit = filters.limit;
            }
            const payload = await this.callTool('list_observations', args);
            const memories = arrayValue(payload, 'memories').map(normalizeMemory);
            const filtered = memories.filter((memory) => matchesMemoryFilters(memory, filters));
            return {
                count: filtered.length,
                memories: filtered,
            };
        });
    }

    public async listPromotionProposals(
        sessionId?: string
    ): Promise<ReviewResult<ReviewConsolidationReport>> {
        return this.run(async () => {
            const activeSessionId = sessionId ?? 'review-panel';
            const payload = await this.callTool('consolidate_session', {
                session_id: activeSessionId,
                mode: 'manual_review',
                render_mode: 'diagnostic',
            });
            return normalizeConsolidationReport(payload);
        });
    }

    public async applyPromotion(
        proposalId: string,
        reason?: string
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return this.run(async () => {
            const payload = await this.callTool('propose_memory_evolution', {
                action: 'apply',
                proposal_id: proposalId,
                reason,
                decided_by: 'operator',
            });
            return normalizeEvolutionProposal(payload);
        }, true);
    }

    public async rejectPromotion(
        proposalId: string,
        reason: string
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return this.run(async () => {
            const payload = await this.callTool('propose_memory_evolution', {
                action: 'reject',
                proposal_id: proposalId,
                reason,
                decided_by: 'operator',
            });
            return normalizeEvolutionProposal(payload);
        }, true);
    }

    public async listContradictions(anchor?: string): Promise<ReviewResult<ReviewConflictList>> {
        return this.run(async () => {
            if (anchor) {
                const payload = await this.callTool('list_memory_conflicts', {
                    anchor,
                    render_mode: 'full',
                    limit: 50,
                });
                return normalizeConflictList(payload);
            }
            const memories = await this.listMemories({ limit: 50 });
            if (!memories.ok) {
                throw memories.error;
            }
            const firstMemory = memories.value.memories[0];
            if (!firstMemory) {
                return {
                    anchor: 'memory-list',
                    total: 0,
                    renderMode: 'full',
                    summaryLines: [],
                    conflicts: [],
                };
            }
            const payload = await this.callTool('list_memory_conflicts', {
                anchor: firstMemory.id,
                render_mode: 'full',
                limit: 50,
            });
            return normalizeConflictList(payload);
        });
    }

    public async applyContradictionResolution(
        args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return this.run(async () => {
            const proposalId = await this.ensureContradictionProposal(args);
            const payload = await this.callTool('propose_memory_evolution', {
                action: 'apply',
                proposal_id: proposalId,
                reason: args.reason,
                decided_by: 'operator',
            });
            return normalizeEvolutionProposal(payload);
        }, true);
    }

    public async rejectContradictionResolution(
        args: ReviewContradictionDecisionArgs
    ): Promise<ReviewResult<ReviewEvolutionProposal>> {
        return this.run(async () => {
            if (!args.reason) {
                throw createError('parse_error', 'Contradiction rejection requires a reason.');
            }
            const proposalId = await this.ensureContradictionProposal(args);
            const payload = await this.callTool('propose_memory_evolution', {
                action: 'reject',
                proposal_id: proposalId,
                reason: args.reason,
                decided_by: 'operator',
            });
            return normalizeEvolutionProposal(payload);
        }, true);
    }

    public async listStaleMemories(
        query?: string,
        limit = 50
    ): Promise<ReviewResult<ReviewMemoryList>> {
        return this.run(async () => {
            const payload = await this.callTool('list_stale_memories', {
                query,
                limit,
            });
            const memories = arrayValue(payload, 'memories').map(normalizeMemory);
            return {
                count: numberValue(payload, 'count'),
                memories,
            };
        });
    }

    public async getMemoryEvidence(
        memoryId: string
    ): Promise<ReviewResult<ReviewMemoryEvidenceBundle>> {
        return this.run(async () => {
            const listResult = await this.listMemories({ limit: 200 });
            if (!listResult.ok) {
                throw listResult.error;
            }
            const memory = listResult.value.memories.find((entry) => entry.id === memoryId);
            if (!memory) {
                throw createError('not_found', `Memory ${memoryId} was not found in the current review scope.`);
            }

            let provenanceEvents: ReviewEventTracePage['events'] = [];
            if (memory.sessionId) {
                const eventTraceResult = await this.getEventTrace({
                    sessionId: memory.sessionId,
                    limit: 200,
                    renderMode: 'full',
                });
                if (eventTraceResult.ok) {
                    provenanceEvents = eventTraceResult.value.events.filter((event) =>
                        eventReferencesMemory(event, memory.id)
                    );
                }
            }

            const explainResult = memory.lastVerifiedAt
                ? await this.explainMemory(memory.id)
                : { ok: false, error: createError('not_found', 'No verification report is available yet.') } as const;
            return {
                memory,
                provenanceEvents,
                lastVerification: explainResult.ok ? explainResult.value : undefined,
            };
        });
    }

    public async verifyMemory(
        memoryId: string
    ): Promise<ReviewResult<ReviewVerifyExplainResponse>> {
        return this.run(async () => {
            const payload = await this.callTool('verify_explain_memory', {
                memory_id: memoryId,
                mode: 'verify_and_explain',
                render_mode: 'full',
            });
            return normalizeVerifyExplainResponse(payload);
        }, true);
    }

    public async getEventTrace(args: {
        taskId?: string;
        sessionId?: string;
        workspaceId?: string;
        kinds?: string[];
        since?: string;
        until?: string;
        cursor?: string;
        limit?: number;
        renderMode?: 'compact' | 'full' | 'diagnostic';
    }): Promise<ReviewResult<ReviewEventTracePage>> {
        return this.run(async () => {
            const payload = await this.callTool('get_event_trace', {
                task_id: args.taskId,
                session_id: args.sessionId,
                workspace_id: args.workspaceId,
                kinds: args.kinds,
                since: args.since,
                until: args.until,
                cursor: args.cursor,
                limit: args.limit ?? 25,
                render_mode: args.renderMode ?? 'full',
            });
            return normalizeEventTracePage(payload);
        });
    }

    public async getRetrievalExplanation(
        requestId: string
    ): Promise<ReviewResult<ReviewRetrievalExplanation>> {
        return this.run(async () => {
            return {
                requestId,
                supported: false,
                reason: 'No public retrieval explanation tool is currently exposed by the daemon.',
            };
        }, true);
    }

    public async listConsolidationJobs(
        sessionId?: string
    ): Promise<ReviewResult<ReviewConsolidationQueueData>> {
        return this.run(async () => {
            const workspaceId = await this.resolveWorkspaceId();
            const traceResult = await this.getEventTrace({
                sessionId,
                workspaceId: sessionId ? undefined : workspaceId,
                kinds: ['memory_consolidated', 'consolidation_failed'],
                limit: sessionId ? 200 : 500,
                renderMode: 'diagnostic',
            });
            if (!traceResult.ok) {
                throw traceResult.error;
            }
            const jobs = dedupeJobs(traceResult.value.events
                .map((event) => toConsolidationJob(event))
                .filter((job): job is ReviewConsolidationJob => job !== undefined));
            return {
                workspaceId,
                jobs,
                queueDepth: buildQueueDepth(jobs),
            };
        });
    }

    public async getConsolidationQueueDepth(): Promise<ReviewResult<ReviewQueueDepthSnapshot>> {
        return this.run(async () => {
            const jobs = await this.listConsolidationJobs();
            if (!jobs.ok) {
                throw jobs.error;
            }
            return jobs.value.queueDepth;
        });
    }

    public async getIndexingHealth(): Promise<ReviewResult<ReviewIndexingHealth>> {
        return this.run(async () => {
            const payload = await this.callTool('index_status', {});
            const snapshot = normalizeIndexStatus(payload);
            const workspaceEvents = await this.getEventTrace({
                workspaceId: snapshot.workspace,
                limit: 500,
                renderMode: 'diagnostic',
            });
            if (!workspaceEvents.ok) {
                throw workspaceEvents.error;
            }
            return {
                snapshot,
                parserHealth: Object.entries(snapshot.languages)
                    .sort(([left], [right]) => left.localeCompare(right))
                    .map(([language, parsedCount]) => ({
                        language,
                        parsedCount,
                    })),
                watcherStatus: snapshot.status === 'indexing' ? 'indexing' : 'watching',
                lastFullRescanAt: undefined,
                vectorIndex: {
                    notes: ['The current daemon snapshot does not expose vector index counters in the review bridge yet.'],
                },
                ftsIndex: {
                    rows: [],
                    notes: ['The current daemon snapshot does not expose FTS table counters in the review bridge yet.'],
                },
                eventLog: {
                    lastHourCount: countEventsSinceHours(workspaceEvents.value.events, 1),
                    last24HoursCount: countEventsSinceHours(workspaceEvents.value.events, 24),
                    last7DaysCount: countEventsSinceHours(workspaceEvents.value.events, 24 * 7),
                    lastCompactionAt: undefined,
                    spilloverRows: workspaceEvents.value.events.filter((event) => event.spilledPayloadRowId !== undefined).length,
                    notes: ['Compaction timestamps are not exposed by the current daemon snapshot.'],
                },
            };
        });
    }

    public async getWorkspaceGraphHealth(): Promise<ReviewResult<ReviewWorkspaceGraphHealth>> {
        return this.run(async () => {
            const indexing = await this.getIndexingHealth();
            if (!indexing.ok) {
                throw indexing.error;
            }
            const staleMemories = await this.listStaleMemories(undefined, 100);
            if (!staleMemories.ok) {
                throw staleMemories.error;
            }
            const staleEdges: ReviewGraphDiagnosticRow[] = staleMemories.value.memories.map((memory) => ({
                identity: memory.id,
                reason: memory.staleReason || memory.freshnessStatus || memory.verificationStatus,
                detail: [
                    memory.linkedFiles.slice(0, 2).join(', '),
                    memory.linkedSymbols.slice(0, 2).join(', '),
                ].filter(Boolean).join(' | '),
                relatedMemoryId: memory.id,
            }));
            return {
                snapshot: indexing.value.snapshot,
                nodeFamilies: WORKSPACE_GRAPH_NODE_FAMILIES.map((family) => {
                    const count = nodeFamilyCount(family, indexing.value.snapshot);
                    return {
                        family,
                        count,
                        note: count === undefined ? 'Not reported by the current daemon snapshot.' : undefined,
                    };
                }),
                edgeFamilies: WORKSPACE_GRAPH_EDGE_FAMILIES.map((family) => ({
                    family,
                    count: family === 'stale_against' ? staleEdges.length : undefined,
                    note: family === 'stale_against' ? undefined : 'Not reported by the current daemon snapshot.',
                })),
                brokenReferences: [],
                staleEdges,
                orphanSymbols: [],
                notes: [
                    'Broken-reference and orphan-symbol diagnostics require daemon-side graph family enumeration that is not yet exposed through the review bridge.',
                ],
            };
        });
    }

    public async retryConsolidationSession(
        sessionId: string,
        mode: 'background' | 'manual_review' | 'replay' | 'post_task' = 'manual_review'
    ): Promise<ReviewResult<ReviewConsolidationReport>> {
        return this.run(async () => {
            const payload = await this.callTool('consolidate_session', {
                session_id: sessionId,
                mode,
                render_mode: 'diagnostic',
            });
            return normalizeConsolidationReport(payload);
        }, true);
    }

    private async getMemoryMetrics(): Promise<ReviewResult<ReviewMetricSnapshot>> {
        return this.run(async () => {
            const payload = await this.callTool('get_memory_metrics', {
                scope: 'session',
                render_mode: 'compact',
            });
            return normalizeMetricSnapshot(payload);
        });
    }

    private async getSessionMetrics(): Promise<ReviewResult<ReviewSessionMetrics>> {
        return this.run(async () => {
            const payload = await this.callToolRaw('get_session_metrics', {});
            return normalizeSessionMetrics(payload);
        });
    }

    private async ensureContradictionProposal(
        args: ReviewContradictionDecisionArgs
    ): Promise<string> {
        if (args.proposalId) {
            return args.proposalId;
        }
        const reason = contradictionReason(args);
        const proposalArgs: Record<string, unknown> = {
            action: 'propose',
            memory_id: args.targetMemoryId,
            reason,
        };
        if (args.linkType === 'supersedes') {
            proposalArgs.superseded_by_memory_id = args.sourceMemoryId;
        } else {
            proposalArgs.invalidate_reason = reason;
        }
        const payload = await this.callTool('propose_memory_evolution', proposalArgs);
        return normalizeEvolutionProposal(payload).proposalId;
    }

    private async explainMemory(
        memoryId: string
    ): Promise<ReviewResult<ReviewVerifyExplainResponse>> {
        return this.run(async () => {
            const payload = await this.callTool('verify_explain_memory', {
                memory_id: memoryId,
                mode: 'explain',
                render_mode: 'full',
            });
            return normalizeVerifyExplainResponse(payload);
        });
    }

    private async resolveWorkspaceId(): Promise<string | undefined> {
        const payload = await this.callTool('index_status', {});
        return normalizeIndexStatus(payload).workspace || undefined;
    }

    private async run<T>(
        operation: () => Promise<T>,
        postError = false
    ): Promise<ReviewResult<T>> {
        try {
            return { ok: true, value: await operation() };
        } catch (error) {
            const reviewError = normalizeError(error);
            vscode.window.showErrorMessage(reviewError.message);
            if (postError) {
                void this.webview?.postMessage({
                    type: 'error',
                    message: reviewError.message,
                });
            }
            return { ok: false, error: reviewError };
        }
    }

    private async callTool(name: string, args: Record<string, unknown>): Promise<Record<string, unknown>> {
        const response = await this.callToolRaw(name, args);
        return recordValue(response);
    }

    private async callToolRaw(name: string, args: Record<string, unknown>): Promise<unknown> {
        if (this.daemon.getStatus() !== 'running') {
            throw createError('daemon_not_running', 'Lattice daemon is not running.');
        }
        const response = await this.daemon.sendRequest('tools/call', {
            name,
            arguments: stripUndefined(args),
        }, 120_000) as McpToolResponse;
        const text = response.content?.find((item) => item.type === 'text')?.text;
        if (typeof text !== 'string') {
            throw createError('parse_error', `Unexpected response payload from ${name}.`, response);
        }
        try {
            return JSON.parse(text) as unknown;
        } catch (error) {
            throw createError('parse_error', `Failed to parse JSON payload from ${name}.`, error);
        }
    }
}

function support(mode: ReviewRouteSupport['mode'], reason: string): ReviewRouteSupport {
    return { mode, reason };
}

function stripUndefined(args: Record<string, unknown>): Record<string, unknown> {
    return Object.fromEntries(Object.entries(args).filter(([, value]) => value !== undefined));
}

function normalizeError(error: unknown): ReviewRpcError {
    if (isReviewRpcError(error)) {
        return error;
    }
    if (error instanceof Error) {
        return createError('rpc_error', error.message, error);
    }
    return createError('rpc_error', String(error));
}

function isReviewRpcError(error: unknown): error is ReviewRpcError {
    return typeof error === 'object' && error !== null && 'code' in error && 'message' in error;
}

function createError(code: ReviewErrorCode, message: string, cause?: unknown): ReviewRpcError {
    return { code, message, cause };
}

function contradictionReason(args: ReviewContradictionDecisionArgs): string {
    const detector = args.detectedBy?.trim();
    const base = args.reason?.trim();
    if (args.linkType === 'supersedes') {
        return base
            || `Marked ${args.targetMemoryId} as superseded by ${args.sourceMemoryId}${detector ? ` (${detector})` : ''}.`;
    }
    return base
        || `Invalidated ${args.targetMemoryId} because it contradicts ${args.sourceMemoryId}${detector ? ` (${detector})` : ''}.`;
}

function eventReferencesMemory(event: ReviewEventTracePage['events'][number], memoryId: string): boolean {
    if (event.references.some((reference) => reference.includes(memoryId))) {
        return true;
    }
    if (event.summary.includes(memoryId)) {
        return true;
    }
    return containsMemoryId(event.payload, memoryId);
}

function containsMemoryId(value: unknown, memoryId: string): boolean {
    if (typeof value === 'string') {
        return value.includes(memoryId);
    }
    if (Array.isArray(value)) {
        return value.some((entry) => containsMemoryId(entry, memoryId));
    }
    if (typeof value === 'object' && value !== null) {
        return Object.values(value).some((entry) => containsMemoryId(entry, memoryId));
    }
    return false;
}

function toConsolidationJob(event: ReviewEventTracePage['events'][number]): ReviewConsolidationJob | undefined {
    const payload = typedPayload(event.payload);
    if (event.kind === 'consolidation_failed') {
        const failure = payload?.payload ?? {};
        const errorKind = stringProperty(failure, 'error_kind');
        return {
            jobId: stringProperty(failure, 'job_id') || event.eventId,
            status: errorKind === 'queue_full' ? 'dropped' : 'failed',
            kind: stringProperty(failure, 'job_kind') || 'unknown',
            mode: stringProperty(failure, 'mode') || 'background',
            sessionId: event.sessionId,
            taskId: event.taskId,
            createdAt: event.timestamp,
            completedAt: event.timestamp,
            llmModel: stringProperty(failure, 'model_name') || undefined,
            resultSummary: event.summary,
            sourceEventId: event.eventId,
            errorKind: errorKind || undefined,
            errorMessage: stringProperty(failure, 'error_message') || undefined,
        };
    }
    if (event.kind !== 'memory_consolidated') {
        return undefined;
    }
    const memoryPayload = payload?.payload ?? {};
    const proposalId = stringProperty(memoryPayload, 'proposal_id');
    const createdAt = event.timestamp;
    return {
        jobId: proposalId || event.eventId,
        proposalId: proposalId || undefined,
        status: proposalId ? 'proposed' : 'applied',
        kind: inferConsolidationKind(event.summary, proposalId),
        mode: inferConsolidationMode(event.summary),
        sessionId: event.sessionId,
        taskId: event.taskId,
        createdAt,
        completedAt: proposalId ? undefined : createdAt,
        resultSummary: event.summary,
        targetMemoryId: extractMemoryIdFromPayload(memoryPayload),
        sourceEventId: event.eventId,
    };
}

function buildQueueDepth(jobs: ReviewConsolidationJob[]): ReviewQueueDepthSnapshot {
    const currentDepth = jobs.filter((job) => ['queued', 'running', 'proposed'].includes(job.status)).length;
    const droppedCount = jobs.filter((job) => job.status === 'dropped').length;
    const notes = droppedCount > 0
        ? ['Dropped jobs were reconstructed from consolidation failure events with error_kind=queue_full.']
        : [];
    return {
        currentDepth,
        maxDepth: DEFAULT_CONSOLIDATION_QUEUE_DEPTH,
        droppedCount,
        notes,
    };
}

function dedupeJobs(jobs: ReviewConsolidationJob[]): ReviewConsolidationJob[] {
    const seen = new Set<string>();
    return jobs
        .sort((left, right) => (Date.parse(right.createdAt ?? '') || 0) - (Date.parse(left.createdAt ?? '') || 0))
        .filter((job) => {
            const key = `${job.jobId}:${job.status}`;
            if (seen.has(key)) {
                return false;
            }
            seen.add(key);
            return true;
        });
}

function typedPayload(value: unknown): { kind?: string; payload?: Record<string, unknown> } | undefined {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        return undefined;
    }
    const record = value as Record<string, unknown>;
    const payload = record.payload;
    return {
        kind: typeof record.kind === 'string' ? record.kind : undefined,
        payload: typeof payload === 'object' && payload !== null && !Array.isArray(payload)
            ? payload as Record<string, unknown>
            : record,
    };
}

function stringProperty(record: Record<string, unknown>, key: string): string {
    return typeof record[key] === 'string' ? record[key] as string : '';
}

function countEventsSinceHours(events: ReviewEventTracePage['events'], hours: number): number {
    const cutoff = Date.now() - hours * 60 * 60 * 1000;
    return events.filter((event) => {
        const timestamp = Date.parse(event.timestamp);
        return !Number.isNaN(timestamp) && timestamp >= cutoff;
    }).length;
}

function extractMemoryIdFromPayload(payload: Record<string, unknown>): string | undefined {
    const value = payload.consolidated_memory_id;
    if (typeof value === 'string') {
        return value;
    }
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        return undefined;
    }
    const record = value as Record<string, unknown>;
    return typeof record.ulid === 'string' ? record.ulid : undefined;
}

function inferConsolidationKind(summary: string, proposalId: string): string {
    const normalized = `${summary} ${proposalId}`.toLowerCase();
    if (normalized.includes('procedure')) {
        return 'procedure_extraction';
    }
    if (normalized.includes('failure')) {
        return 'failure_pattern_extraction';
    }
    if (normalized.includes('duplicate')) {
        return 'duplicate_detection';
    }
    if (normalized.includes('contradict')) {
        return 'contradiction_detection';
    }
    if (normalized.includes('supersed')) {
        return 'supersession_proposal';
    }
    if (normalized.includes('docs')) {
        return 'docs_update_proposal';
    }
    return 'episode_summary';
}

function inferConsolidationMode(summary: string): string {
    const normalized = summary.toLowerCase();
    if (normalized.includes('replay')) {
        return 'replay';
    }
    if (normalized.includes('background')) {
        return 'background';
    }
    return 'manual_review';
}

function nodeFamilyCount(family: string, snapshot: ReviewIndexStatus): number | undefined {
    switch (family) {
        case 'File':
            return snapshot.files;
        case 'Package':
            return snapshot.repos.length || 1;
        default:
            return undefined;
    }
}
