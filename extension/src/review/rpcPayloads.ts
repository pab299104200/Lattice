export interface ReviewMemory {
    id: string;
    sessionId?: string;
    content: string;
    memoryClass: string;
    assertionType: string;
    scope: string;
    confidence: number;
    confidenceReason?: string;
    verificationStatus: string;
    freshnessStatus: string;
    contradictionState: string;
    supersessionState: string;
    inclusionReason: string;
    evidenceStrength: number;
    linkedFiles: string[];
    linkedSymbols: string[];
    linkedDocs: string[];
    linkedTests: string[];
    linkedMemories: string[];
    validityConditions: string[];
    invalidationTriggers: string[];
    sourceQuery?: string;
    branch?: string;
    refreshKey?: string;
    workspaceId?: string;
    createdAt?: number;
    staleReason?: string;
    lastVerifiedAt?: number;
    lastVerifiedGraphSnapshotId?: number;
    evidence: unknown[];
    links: unknown[];
    provenance: unknown[];
    accessHistory: unknown[];
    usefulnessScores: unknown[];
}

export interface ReviewMemoryList {
    count: number;
    memories: ReviewMemory[];
}

export interface ReviewMemoryEvidenceBundle {
    memory: ReviewMemory;
    provenanceEvents: ReviewEventTraceEntry[];
    lastVerification?: ReviewVerifyExplainResponse;
}

export interface ReviewIndexStatus {
    status: string;
    version: string;
    workspace: string;
    nodes: number;
    edges: number;
    files: number;
    languages: Record<string, number>;
    multiRepo: boolean;
    workspaces: string[];
    repos: Array<{ name: string; files: number; nodes: number; edges: number }>;
}

export interface ReviewParserHealth {
    language: string;
    parsedCount: number;
    failedCount?: number;
    lastError?: string;
}

export interface ReviewCountMetric {
    label: string;
    count?: number;
    note?: string;
}

export interface ReviewQueueDepthSnapshot {
    currentDepth: number;
    maxDepth: number;
    droppedCount: number;
    notes: string[];
}

export interface ReviewConsolidationJob {
    jobId: string;
    proposalId?: string;
    status: string;
    kind: string;
    mode: string;
    sessionId?: string;
    taskId?: string;
    createdAt?: string;
    startedAt?: string;
    completedAt?: string;
    durationMs?: number;
    llmModel?: string;
    resultSummary: string;
    targetMemoryId?: string;
    sourceEventId?: string;
    errorKind?: string;
    errorMessage?: string;
}

export interface ReviewConsolidationQueueData {
    workspaceId?: string;
    jobs: ReviewConsolidationJob[];
    queueDepth: ReviewQueueDepthSnapshot;
}

export interface ReviewIndexingHealth {
    snapshot: ReviewIndexStatus;
    parserHealth: ReviewParserHealth[];
    watcherStatus: string;
    lastFullRescanAt?: string;
    vectorIndex: {
        totalEmbeddings?: number;
        lastRebuildAt?: string;
        averageFreshness?: string;
        staleEmbeddings?: number;
        notes: string[];
    };
    ftsIndex: {
        rows: ReviewCountMetric[];
        lastRebuildAt?: string;
        orphanRows?: number;
        notes: string[];
    };
    eventLog: {
        lastHourCount: number;
        last24HoursCount: number;
        last7DaysCount: number;
        lastCompactionAt?: string;
        spilloverRows: number;
        notes: string[];
    };
}

export interface ReviewFamilyCount {
    family: string;
    count?: number;
    note?: string;
}

export interface ReviewGraphDiagnosticRow {
    identity: string;
    reason: string;
    detail?: string;
    reference?: string;
    relatedMemoryId?: string;
}

export interface ReviewWorkspaceGraphHealth {
    snapshot: ReviewIndexStatus;
    nodeFamilies: ReviewFamilyCount[];
    edgeFamilies: ReviewFamilyCount[];
    brokenReferences: ReviewGraphDiagnosticRow[];
    staleEdges: ReviewGraphDiagnosticRow[];
    orphanSymbols: ReviewGraphDiagnosticRow[];
    notes: string[];
}

export interface ReviewMetricSignal {
    signal: string;
    value: number | null;
    denominator?: number;
    sampleCount?: number;
    incomplete?: boolean;
    reasonIfNull?: string;
}

export interface ReviewMetricSnapshot {
    scope: string;
    renderMode: string;
    incomplete: boolean;
    notes: string[];
    signals: ReviewMetricSignal[];
}

export interface ReviewSessionMetrics {
    totalToolCalls: number;
    workflowToolCalls: number;
    totalPayloadTokens: number;
    averagePayloadTokensPerTool: number;
    totalPayloadBytes: number;
    averagePayloadBytesPerTool: number;
    contextHandleReuses: number;
    contextHandleReuseRate: number;
}

export interface ReviewEventTraceEntry {
    eventId: string;
    expansionHandle: string;
    kind: string;
    timestamp: string;
    actor: string;
    branch: string;
    sessionId: string;
    taskId?: string;
    workspaceId: string;
    summary: string;
    references: string[];
    payload?: unknown;
    payloadHash?: string;
    spilledPayloadRowId?: number;
}

export interface ReviewEventTracePage {
    scope: { kind: string; value: string };
    renderMode: string;
    cursor?: string;
    nextCursor?: string;
    events: ReviewEventTraceEntry[];
}

export interface ReviewConflict {
    source: string;
    target: string;
    linkType: string;
    linkStrength: number;
    createdBy: string;
    createdAt: number;
    linkVerificationStatus: string;
    reason: string;
}

export interface ReviewConflictList {
    anchor: string;
    total: number;
    nextCursor?: number;
    renderMode: string;
    summaryLines: string[];
    conflicts: ReviewConflict[];
}

export interface ReviewEvolutionProposal {
    proposalId: string;
    action: string;
    sourceMemoryId?: string;
    proposalKind: string;
    decision: string;
    priorState: unknown;
    proposedState: unknown;
    deprecationWarning?: string;
}

export interface ReviewVerifyExplainResponse {
    status: string;
    confidenceDelta: number;
    expansionHandle: string;
    summaryLines: string[];
    renderMode: string;
    checks: Array<{
        kind: string;
        target: string;
        outcome: string;
        evidenceRef: string;
        detail: string;
    }>;
    diagnosticTrace?: string[];
    deprecationWarning?: string;
}

export interface ReviewConsolidationReport {
    sessionId: string;
    mode: string;
    renderMode: string;
    budgetMs?: number;
    incomplete: boolean;
    notes: string[];
    proposals: ReviewConsolidationProposalItem[];
    categories: Array<{
        category: string;
        proposalIds: string[];
        note?: string;
    }>;
}

export interface ReviewConsolidationProposalItem {
    proposalId: string;
    jobId: string;
    proposalKind: string;
    taskId: string;
    category: string;
    summary: string;
    targetMemoryId?: string;
    enqueuedAt?: number;
    decision: string;
    proposedClass?: string;
    currentScope?: string;
    targetScope?: string;
    confidence?: number;
    evidenceCount: number;
    priorState?: unknown;
    proposedState?: unknown;
    evidence?: unknown;
    provenance?: unknown;
}

export interface ReviewRetrievalExplanationRequest {
    toolName: string;
    taskStatement: string;
    intentClassification: string;
    timestamp?: string;
}

export interface ReviewRetrievalAnchor {
    kind: string;
    label: string;
    provenance: string;
    reference?: string;
}

export interface ReviewRetrievalCandidateSignal {
    signal: string;
    score: number;
}

export interface ReviewRetrievalCandidate {
    source: string;
    identity: string;
    decision: 'included' | 'excluded' | 'expanded';
    score: number;
    topSignals: ReviewRetrievalCandidateSignal[];
    reason: string;
    allSignals: ReviewRetrievalCandidateSignal[];
}

export interface ReviewRetrievalExcludedCandidate {
    identity: string;
    source: string;
    score: number;
    reason: string;
    allSignals: ReviewRetrievalCandidateSignal[];
}

export interface ReviewRetrievalExplanation {
    requestId: string;
    supported: boolean;
    reason: string;
    request?: ReviewRetrievalExplanationRequest;
    anchors?: ReviewRetrievalAnchor[];
    candidates?: ReviewRetrievalCandidate[];
    excludedCandidates?: ReviewRetrievalExcludedCandidate[];
}

export interface ReviewOverview {
    indexStatus: ReviewIndexStatus;
    memoryList: ReviewMemoryList;
    metrics: ReviewMetricSnapshot;
    sessionMetrics: ReviewSessionMetrics;
}

export function recordValue(value: unknown): Record<string, unknown> {
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        throw new Error('Expected an object payload.');
    }
    return value as Record<string, unknown>;
}

export function arrayValue(object: Record<string, unknown>, key: string): Record<string, unknown>[] {
    const value = object[key];
    if (!Array.isArray(value)) {
        return [];
    }
    return value
        .filter((entry): entry is Record<string, unknown> => typeof entry === 'object' && entry !== null)
        .map((entry) => entry as Record<string, unknown>);
}

export function stringValue(object: Record<string, unknown>, key: string): string {
    return typeof object[key] === 'string' ? object[key] as string : '';
}

export function numberValue(object: Record<string, unknown>, key: string): number {
    return typeof object[key] === 'number' ? object[key] as number : 0;
}

export function optionalString(object: Record<string, unknown>, key: string): string | undefined {
    return typeof object[key] === 'string' ? object[key] as string : undefined;
}

export function stringArrayValue(object: Record<string, unknown>, key: string): string[] {
    const value = object[key];
    if (!Array.isArray(value)) {
        return [];
    }
    return value.filter((entry): entry is string => typeof entry === 'string');
}

export function normalizeMemory(record: Record<string, unknown>): ReviewMemory {
    return {
        id: stringValue(record, 'id'),
        sessionId: optionalString(record, 'session_id'),
        content: stringValue(record, 'content'),
        memoryClass: stringValue(record, 'memory_class'),
        assertionType: stringValue(record, 'assertion_type'),
        scope: stringValue(record, 'scope'),
        confidence: numberValue(record, 'confidence'),
        confidenceReason: optionalString(record, 'confidence_reason'),
        verificationStatus: stringValue(record, 'verification_status'),
        freshnessStatus: stringValue(record, 'freshness_status'),
        contradictionState: stringValue(record, 'contradiction_state'),
        supersessionState: stringValue(record, 'supersession_state'),
        inclusionReason: stringValue(record, 'inclusion_reason'),
        evidenceStrength: numberValue(record, 'evidence_strength'),
        linkedFiles: stringArrayValue(record, 'linked_files'),
        linkedSymbols: stringArrayValue(record, 'linked_symbols'),
        linkedDocs: stringArrayValue(record, 'linked_docs'),
        linkedTests: stringArrayValue(record, 'linked_tests'),
        linkedMemories: stringArrayValue(record, 'linked_memories'),
        validityConditions: stringArrayValue(record, 'validity_conditions'),
        invalidationTriggers: stringArrayValue(record, 'invalidation_triggers'),
        sourceQuery: optionalString(record, 'source_query'),
        branch: optionalString(record, 'branch'),
        refreshKey: optionalString(record, 'refresh_key'),
        workspaceId: optionalString(record, 'workspace_id'),
        createdAt: typeof record.created_at === 'number' ? record.created_at : undefined,
        staleReason: optionalString(record, 'stale_reason'),
        lastVerifiedAt: typeof record.last_verified_at === 'number' ? record.last_verified_at : undefined,
        lastVerifiedGraphSnapshotId: typeof record.last_verified_graph_snapshot_id === 'number'
            ? record.last_verified_graph_snapshot_id
            : undefined,
        evidence: Array.isArray(record.evidence) ? record.evidence : [],
        links: Array.isArray(record.links) ? record.links : [],
        provenance: Array.isArray(record.provenance) ? record.provenance : [],
        accessHistory: Array.isArray(record.access_history) ? record.access_history : [],
        usefulnessScores: Array.isArray(record.usefulness_scores) ? record.usefulness_scores : [],
    };
}

export function matchesMemoryFilters(
    memory: ReviewMemory,
    filters?: {
        status?: string[];
        scope?: string[];
        memoryClass?: string[];
    }
): boolean {
    if (!filters) {
        return true;
    }
    if (filters.status?.length && !filters.status.includes(memory.verificationStatus)) {
        return false;
    }
    if (filters.scope?.length && !filters.scope.includes(memory.scope)) {
        return false;
    }
    if (filters.memoryClass?.length && !filters.memoryClass.includes(memory.memoryClass)) {
        return false;
    }
    return true;
}

export function normalizeIndexStatus(payload: Record<string, unknown>): ReviewIndexStatus {
    const repos = arrayValue(payload, 'repos').map((entry) => ({
        name: stringValue(entry, 'name'),
        files: numberValue(entry, 'files'),
        nodes: numberValue(entry, 'nodes'),
        edges: numberValue(entry, 'edges'),
    }));
    const rawLanguages = payload.languages;
    const languages: Record<string, number> = {};
    if (typeof rawLanguages === 'object' && rawLanguages !== null && !Array.isArray(rawLanguages)) {
        for (const [key, value] of Object.entries(rawLanguages)) {
            if (typeof value === 'number') {
                languages[key] = value;
            }
        }
    }
    return {
        status: stringValue(payload, 'status'),
        version: stringValue(payload, 'version'),
        workspace: stringValue(payload, 'workspace'),
        nodes: numberValue(payload, 'nodes'),
        edges: numberValue(payload, 'edges'),
        files: numberValue(payload, 'files'),
        languages,
        multiRepo: payload.multi_repo === true,
        workspaces: stringArrayValue(payload, 'workspaces'),
        repos,
    };
}

export function normalizeMetricSnapshot(payload: Record<string, unknown>): ReviewMetricSnapshot {
    const signals = arrayValue(payload, 'signals').map((signal) => ({
        signal: stringValue(signal, 'signal'),
        value: typeof signal.value === 'number' ? signal.value : null,
        denominator: typeof signal.denominator === 'number' ? signal.denominator : undefined,
        sampleCount: typeof signal.sample_count === 'number' ? signal.sample_count : undefined,
        incomplete: signal.incomplete === true,
        reasonIfNull: optionalString(signal, 'reason_if_null'),
    }));
    return {
        scope: stringValue(payload, 'scope'),
        renderMode: stringValue(payload, 'render_mode'),
        incomplete: payload.incomplete === true,
        notes: stringArrayValue(payload, 'notes'),
        signals,
    };
}

export function normalizeSessionMetrics(payload: unknown): ReviewSessionMetrics {
    const record = recordValue(payload);
    return {
        totalToolCalls: numberValue(record, 'total_tool_calls'),
        workflowToolCalls: numberValue(record, 'workflow_tool_calls'),
        totalPayloadTokens: numberValue(record, 'total_payload_tokens'),
        averagePayloadTokensPerTool: numberValue(record, 'average_payload_tokens_per_tool'),
        totalPayloadBytes: numberValue(record, 'total_payload_bytes'),
        averagePayloadBytesPerTool: numberValue(record, 'average_payload_bytes_per_tool'),
        contextHandleReuses: numberValue(record, 'context_handle_reuses'),
        contextHandleReuseRate: numberValue(record, 'context_handle_reuse_rate'),
    };
}

export function normalizeEventTracePage(payload: Record<string, unknown>): ReviewEventTracePage {
    const scopeRecord = typeof payload.scope === 'object' && payload.scope !== null
        ? payload.scope as Record<string, unknown>
        : {};
    const events = arrayValue(payload, 'events').map((event) => ({
        eventId: stringValue(event, 'event_id'),
        expansionHandle: stringValue(event, 'expansion_handle'),
        kind: stringValue(event, 'kind'),
        timestamp: stringValue(event, 'timestamp'),
        actor: normalizeActor(event.actor),
        branch: stringValue(event, 'branch'),
        sessionId: stringValue(event, 'session_id'),
        taskId: optionalString(event, 'task_id'),
        workspaceId: stringValue(event, 'workspace_id'),
        summary: stringValue(event, 'summary'),
        references: stringArrayValue(event, 'references'),
        payload: event.payload,
        payloadHash: optionalString(event, 'payload_hash'),
        spilledPayloadRowId: typeof event.spilled_payload_row_id === 'number'
            ? event.spilled_payload_row_id
            : undefined,
    }));
    return {
        scope: {
            kind: stringValue(scopeRecord, 'kind'),
            value: stringValue(scopeRecord, 'value'),
        },
        renderMode: stringValue(payload, 'render_mode'),
        cursor: optionalString(payload, 'cursor'),
        nextCursor: optionalString(payload, 'next_cursor'),
        events,
    };
}

export function normalizeConflictList(payload: Record<string, unknown>): ReviewConflictList {
    const conflicts = arrayValue(payload, 'conflicts').map((conflict) => ({
        source: stringValue(conflict, 'source'),
        target: stringValue(conflict, 'target'),
        linkType: stringValue(conflict, 'link_type'),
        linkStrength: numberValue(conflict, 'link_strength'),
        createdBy: stringValue(conflict, 'created_by'),
        createdAt: numberValue(conflict, 'created_at'),
        linkVerificationStatus: stringValue(conflict, 'link_verification_status'),
        reason: stringValue(conflict, 'reason'),
    }));
    return {
        anchor: stringValue(payload, 'anchor'),
        total: numberValue(payload, 'total'),
        nextCursor: typeof payload.next_cursor === 'number' ? payload.next_cursor : undefined,
        renderMode: stringValue(payload, 'render_mode'),
        summaryLines: stringArrayValue(payload, 'summary_lines'),
        conflicts,
    };
}

export function normalizeEvolutionProposal(payload: Record<string, unknown>): ReviewEvolutionProposal {
    return {
        proposalId: stringValue(payload, 'proposal_id'),
        action: stringValue(payload, 'action'),
        sourceMemoryId: optionalString(payload, 'source_memory_id'),
        proposalKind: stringValue(payload, 'proposal_kind'),
        decision: stringValue(payload, 'decision'),
        priorState: payload.prior_state,
        proposedState: payload.proposed_state,
        deprecationWarning: optionalString(payload, 'deprecation_warning'),
    };
}

export function normalizeVerifyExplainResponse(
    payload: Record<string, unknown>
): ReviewVerifyExplainResponse {
    const checks = arrayValue(payload, 'checks').map((check) => ({
        kind: stringValue(check, 'kind'),
        target: stringValue(check, 'target'),
        outcome: stringValue(check, 'outcome'),
        evidenceRef: stringValue(check, 'evidence_ref'),
        detail: stringValue(check, 'detail'),
    }));
    return {
        status: stringValue(payload, 'status'),
        confidenceDelta: numberValue(payload, 'confidence_delta'),
        expansionHandle: stringValue(payload, 'expansion_handle'),
        summaryLines: stringArrayValue(payload, 'summary_lines'),
        renderMode: stringValue(payload, 'render_mode'),
        checks,
        diagnosticTrace: Array.isArray(payload.diagnostic_trace)
            ? payload.diagnostic_trace.filter((entry): entry is string => typeof entry === 'string')
            : undefined,
        deprecationWarning: optionalString(payload, 'deprecation_warning'),
    };
}

export function normalizeConsolidationReport(
    payload: Record<string, unknown>
): ReviewConsolidationReport {
    const proposals = arrayValue(payload, 'proposals').map((proposal) => ({
        proposalId: stringValue(proposal, 'proposal_id'),
        jobId: stringValue(proposal, 'job_id'),
        proposalKind: stringValue(proposal, 'proposal_kind'),
        taskId: stringValue(proposal, 'task_id'),
        category: stringValue(proposal, 'category'),
        summary: stringValue(proposal, 'summary'),
        targetMemoryId: optionalString(proposal, 'target_memory_id'),
        enqueuedAt: typeof proposal.enqueued_at === 'number' ? proposal.enqueued_at : undefined,
        decision: stringValue(proposal, 'decision'),
        proposedClass: optionalString(proposal, 'proposed_class'),
        currentScope: optionalString(proposal, 'current_scope'),
        targetScope: optionalString(proposal, 'target_scope'),
        confidence: typeof proposal.confidence === 'number' ? proposal.confidence : undefined,
        evidenceCount: numberValue(proposal, 'evidence_count'),
        priorState: proposal.prior_state,
        proposedState: proposal.proposed_state,
        evidence: proposal.evidence,
        provenance: proposal.provenance,
    }));
    const categories = arrayValue(payload, 'categories').map((category) => ({
        category: stringValue(category, 'category'),
        proposalIds: stringArrayValue(category, 'proposal_ids'),
        note: optionalString(category, 'note'),
    }));
    return {
        sessionId: stringValue(payload, 'session_id'),
        mode: stringValue(payload, 'mode'),
        renderMode: stringValue(payload, 'render_mode'),
        budgetMs: typeof payload.budget_ms === 'number' ? payload.budget_ms : undefined,
        incomplete: payload.incomplete === true,
        notes: stringArrayValue(payload, 'notes'),
        proposals,
        categories,
    };
}

function normalizeActor(value: unknown): string {
    if (typeof value === 'string') {
        return value;
    }
    if (typeof value !== 'object' || value === null || Array.isArray(value)) {
        return '';
    }
    const record = value as Record<string, unknown>;
    const type = typeof record.type === 'string' ? record.type : '';
    const label = typeof record.label === 'string' ? record.label : '';
    if (label) {
        return label;
    }
    if (type) {
        return type;
    }
    const keys = Object.keys(record);
    if (keys.length === 1) {
        const [kind] = keys;
        const detail = record[kind];
        if (typeof detail === 'object' && detail !== null && !Array.isArray(detail)) {
            const named = detail as Record<string, unknown>;
            const suffix = typeof named.model === 'string'
                ? named.model
                : typeof named.name === 'string'
                    ? named.name
                    : '';
            return suffix ? `${kind}:${suffix}` : kind;
        }
        return kind;
    }
    return '';
}
