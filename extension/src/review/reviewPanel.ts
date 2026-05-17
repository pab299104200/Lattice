import * as crypto from 'crypto';
import * as vscode from 'vscode';
import { DaemonManager } from '../daemon';
import { createReviewI18n, loadReviewCatalog, ReviewI18n } from './i18n';
import { serializeProposalDialog } from './components/ProposalDialog';
import { renderStatusBadge } from './components/StatusBadge';
import {
    ConsolidationQueueViewState,
    mountConsolidationQueueView,
} from './consolidationQueueView';
import { mountEvidenceInspector, EvidenceInspectorState } from './evidenceInspector';
import { EventTraceViewState, mountEventTraceView } from './eventTraceView';
import {
    IndexingHealthViewState,
    mountIndexingHealthView,
} from './indexingHealthView';
import {
    MemoryInboxHost,
    MountedMemoryInbox,
    mountMemoryInbox,
    ReviewPanelMessage,
    ReviewRouteView,
} from './memoryInbox';
import {
    mountRetrievalExplanationView,
    RetrievalExplanationViewState,
} from './retrievalExplanationView';
import { mountStaleView, StaleViewState } from './staleView';
import {
    mountWorkspaceGraphHealthView,
    WorkspaceGraphHealthViewState,
} from './workspaceGraphHealthView';
import {
    ReviewContradictionDecisionArgs,
    ReviewBridgeCapabilities,
    ReviewRpcBridge,
    ReviewRpcBridgeContract,
} from './rpcBridge';
import { serializeContradictionQueue } from './contradictionQueue';
import { serializePromotionQueue } from './promotionQueue';
import { ReviewOverview } from './rpcPayloads';

export type ReviewRouteId =
    | 'memoryInbox'
    | 'promotionQueue'
    | 'contradictionQueue'
    | 'staleView'
    | 'evidenceInspector'
    | 'eventTrace'
    | 'retrievalExplanation'
    | 'usefulnessMetrics'
    | 'workspaceGraphHealth'
    | 'indexingHealth'
    | 'consolidationQueue';

interface ReviewRouteDefinition {
    id: ReviewRouteId;
    label: string;
    description: string;
}

interface WebviewMessage {
    command?: string;
    route?: ReviewRouteId;
    requestId?: string;
    method?: string;
    params?: Record<string, unknown>;
    filterKey?: 'status' | 'scope' | 'memoryClass';
    sortKey?: 'status' | 'memoryClass' | 'scope' | 'lastVerifiedAt' | 'confidence';
    sortBy?: 'status' | 'memoryClass' | 'scope' | 'staleSince' | 'timestamp' | 'kind' | 'actor' | 'score' | 'source' | 'decision' | 'mode' | 'createdAt' | 'duration' | 'family' | 'count' | 'identity' | 'reason';
    page?: number;
    pageSize?: number;
    memoryId?: string;
    value?: string;
    eventId?: string;
    since?: string;
    until?: string;
}

export interface ReviewStateMessage {
    type: 'state';
    activeRoute: ReviewRouteId;
    routes: ReviewRouteDefinition[];
    capabilities: ReviewBridgeCapabilities;
    overview: ReviewOverview;
    routeView?: ReviewRouteView;
}

type BridgeFactory = (daemon: DaemonManager) => ReviewRpcBridgeContract;
interface FilterPickItem extends vscode.QuickPickItem {
    value: string;
}

let bridgeFactory: BridgeFactory = (daemon) => new ReviewRpcBridge(daemon);
let latestProviderForTests: ReviewPanelProvider | undefined;

export function setBridgeForTests(factory?: BridgeFactory): void {
    bridgeFactory = factory ?? ((daemon) => new ReviewRpcBridge(daemon));
}

export function getReviewPanelProviderForTests(): ReviewPanelProvider | undefined {
    return latestProviderForTests;
}

export class ReviewPanelProvider implements vscode.WebviewViewProvider, vscode.Disposable {
    public static readonly viewType = 'lattice.reviewPanel';

    private webviewView: vscode.WebviewView | undefined;
    private bridge: ReviewRpcBridgeContract | undefined;
    private activeRoute: ReviewRouteId = 'memoryInbox';
    private routeView: ReviewRouteView | undefined;
    private latestOverview: ReviewOverview | undefined;
    private memoryInbox: MountedMemoryInbox | undefined;
    private staleViewState: StaleViewState = {
        selectedStatuses: [],
        selectedScopes: [],
        sortBy: 'staleSince',
        sortDirection: 'desc',
        rowErrors: {},
    };
    private evidenceInspectorState: EvidenceInspectorState = {
        memoryId: undefined,
        reverifyInFlight: false,
        inlineError: undefined,
    };
    private eventTraceState: EventTraceViewState = {
        inlineError: undefined,
        kinds: [],
        memoryId: undefined,
        page: 1,
        pageSize: 25,
        selectedActors: [],
        sortBy: 'timestamp',
        sortDirection: 'desc',
    };
    private retrievalExplanationState: RetrievalExplanationViewState = {
        inlineError: undefined,
        page: 1,
        pageSize: 25,
        requestId: undefined,
        sortBy: 'score',
        sortDirection: 'desc',
    };
    private consolidationQueueState: ConsolidationQueueViewState = {
        inlineError: undefined,
        page: 1,
        pageSize: 25,
        selectedKinds: [],
        selectedModes: [],
        selectedStatuses: [],
        sortBy: 'createdAt',
        sortDirection: 'desc',
    };
    private indexingHealthState: IndexingHealthViewState = {
        inlineError: undefined,
    };
    private workspaceGraphHealthState: WorkspaceGraphHealthViewState = {
        inlineError: undefined,
        nodePage: 1,
        edgePage: 1,
        brokenPage: 1,
        stalePage: 1,
        orphanPage: 1,
        pageSize: 10,
        nodeSortBy: 'family',
        edgeSortBy: 'family',
        diagnosticSortBy: 'identity',
        nodeSortDirection: 'asc',
        edgeSortDirection: 'asc',
        diagnosticSortDirection: 'asc',
    };
    private readonly i18n: ReviewI18n;
    private readonly disposables: vscode.Disposable[] = [];

    constructor(
        private readonly context: vscode.ExtensionContext,
        private readonly daemon: DaemonManager
    ) {
        this.i18n = createReviewI18n();
        latestProviderForTests = this;
    }

    public dispose(): void {
        this.memoryInbox?.dispose();
        this.memoryInbox = undefined;
        this.bridge?.dispose();
        this.bridge = undefined;
        this.webviewView = undefined;
        this.routeView = undefined;
        this.latestOverview = undefined;
        if (latestProviderForTests === this) {
            latestProviderForTests = undefined;
        }
        while (this.disposables.length > 0) {
            this.disposables.pop()?.dispose();
        }
    }

    public resolveWebviewView(
        webviewView: vscode.WebviewView,
        _context: vscode.WebviewViewResolveContext,
        _token: vscode.CancellationToken
    ): void {
        this.webviewView = webviewView;
        this.bridge?.dispose();
        this.bridge = bridgeFactory(this.daemon);
        this.bridge.setWebview(webviewView.webview);

        webviewView.webview.options = {
            enableScripts: true,
            localResourceRoots: [
                vscode.Uri.joinPath(this.context.extensionUri, 'media'),
                vscode.Uri.joinPath(this.context.extensionUri, 'out'),
            ],
        };

        webviewView.webview.html = this.renderHtml(webviewView.webview);

        const messageSubscription = webviewView.webview.onDidReceiveMessage((message: WebviewMessage) => {
            void this.handleMessage(message);
        });
        const disposeSubscription = webviewView.onDidDispose(() => {
            this.memoryInbox?.dispose();
            this.memoryInbox = undefined;
            this.bridge?.dispose();
            this.bridge = undefined;
            this.webviewView = undefined;
            this.routeView = undefined;
            this.latestOverview = undefined;
        });
        const visibilitySubscription = webviewView.onDidChangeVisibility(() => {
            if (webviewView.visible) {
                void this.refreshState();
            }
        });

        this.disposables.push(messageSubscription, disposeSubscription, visibilitySubscription);
        void this.refreshState();
    }

    private async handleMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'ready':
            case 'refresh':
                await this.refreshState();
                break;
            case 'navigate':
                if (message.route) {
                    this.activeRoute = message.route;
                    if (message.memoryId) {
                        this.evidenceInspectorState.memoryId = message.memoryId;
                        this.eventTraceState.memoryId = message.memoryId;
                    }
                    if (message.route === 'retrievalExplanation' && message.requestId) {
                        this.retrievalExplanationState.requestId = message.requestId;
                    }
                }
                await this.refreshState();
                break;
            case 'rpcRequest':
                await this.handleRpcRequest(message);
                break;
            case 'reviewViewError':
                if (message.value) {
                    await vscode.window.showErrorMessage(message.value);
                }
                break;
            default:
                if (this.activeRoute === 'memoryInbox') {
                    await this.handleMemoryInboxMessage(message);
                    return;
                }
                if (this.activeRoute === 'staleView') {
                    await this.handleStaleViewMessage(message);
                    return;
                }
                if (this.activeRoute === 'evidenceInspector') {
                    await this.handleEvidenceInspectorMessage(message);
                    return;
                }
                if (this.activeRoute === 'eventTrace') {
                    await this.handleEventTraceMessage(message);
                    return;
                }
                if (this.activeRoute === 'retrievalExplanation') {
                    await this.handleRetrievalExplanationMessage(message);
                    return;
                }
                if (this.activeRoute === 'consolidationQueue') {
                    await this.handleConsolidationQueueMessage(message);
                    return;
                }
                if (this.activeRoute === 'indexingHealth') {
                    await this.handleIndexingHealthMessage(message);
                    return;
                }
                if (this.activeRoute === 'workspaceGraphHealth') {
                    await this.handleWorkspaceGraphHealthMessage(message);
                }
                break;
        }
    }

    public async buildStateForTests(
        options?: { route?: ReviewRouteId; memoryId?: string; requestId?: string }
    ): Promise<ReviewStateMessage> {
        if (!this.bridge) {
            this.bridge = bridgeFactory(this.daemon);
        }
        if (options?.route) {
            this.activeRoute = options.route;
        }
        if (options?.memoryId !== undefined) {
            this.evidenceInspectorState.memoryId = options.memoryId;
            this.eventTraceState.memoryId = options.memoryId;
        }
        if (options?.requestId !== undefined) {
            this.retrievalExplanationState.requestId = options.requestId;
        }
        const overviewResult = await this.bridge.getOverview();
        if (!overviewResult.ok) {
            throw new Error(overviewResult.error.message);
        }
        this.latestOverview = overviewResult.value;
        this.routeView = await this.renderActiveRoute();
        return {
            type: 'state',
            activeRoute: this.activeRoute,
            routes: this.routes(),
            capabilities: this.bridge.getCapabilities(),
            overview: this.latestOverview,
            routeView: this.routeView,
        };
    }

    private async handleRpcRequest(message: WebviewMessage): Promise<void> {
        if (!this.webviewView || !this.bridge || !message.requestId || !message.method) {
            return;
        }
        try {
            const result = await this.dispatchRpcMethod(message.method, message.params ?? {});
            if (!result.ok) {
                await this.webviewView.webview.postMessage({
                    type: 'rpcResponse',
                    requestId: message.requestId,
                    error: result.error.message,
                });
                return;
            }
            const successMessage = this.successMessageForRpc(message.method);
            if (successMessage) {
                await vscode.window.showInformationMessage(successMessage);
            }
            await this.webviewView.webview.postMessage({
                type: 'rpcResponse',
                requestId: message.requestId,
                result: result.value,
            });
        } catch (error) {
            const errorMessage = error instanceof Error ? error.message : String(error);
            await this.webviewView.webview.postMessage({
                type: 'rpcResponse',
                requestId: message.requestId,
                error: errorMessage,
            });
        }
    }

    private async dispatchRpcMethod(
        method: string,
        params: Record<string, unknown>
    ): Promise<{ ok: true; value: unknown } | { ok: false; error: { message: string } }> {
        if (!this.bridge) {
            return { ok: false, error: { message: 'Review bridge is unavailable.' } };
        }
        switch (method) {
            case 'listPromotionProposals':
                return this.bridge.listPromotionProposals();
            case 'applyPromotion':
                return this.bridge.applyPromotion(
                    stringParam(params, 'proposalId'),
                    optionalStringParam(params, 'reason')
                );
            case 'rejectPromotion':
                return this.bridge.rejectPromotion(
                    stringParam(params, 'proposalId'),
                    stringParam(params, 'reason')
                );
            case 'listContradictions':
                return this.bridge.listContradictions(optionalStringParam(params, 'anchor'));
            case 'getMemoryEvidence':
                return this.bridge.getMemoryEvidence(stringParam(params, 'memoryId'));
            case 'applyContradictionResolution':
                return this.bridge.applyContradictionResolution(
                    contradictionParams(params)
                );
            case 'rejectContradictionResolution':
                return this.bridge.rejectContradictionResolution(
                    contradictionParams(params, true) as ReviewContradictionDecisionArgs & { reason: string }
                );
            default:
                return { ok: false, error: { message: `Unsupported review RPC method: ${method}` } };
        }
    }

    private successMessageForRpc(method: string): string | undefined {
        switch (method) {
            case 'applyPromotion':
                return this.i18n.t('promotionQueue.messages.applySuccess');
            case 'rejectPromotion':
                return this.i18n.t('promotionQueue.messages.rejectSuccess');
            case 'applyContradictionResolution':
                return this.i18n.t('contradictionQueue.messages.applySuccess');
            case 'rejectContradictionResolution':
                return this.i18n.t('contradictionQueue.messages.rejectSuccess');
            default:
                return undefined;
        }
    }

    private async refreshState(): Promise<void> {
        if (!this.webviewView || !this.bridge) {
            return;
        }
        const overviewResult = await this.bridge.getOverview();
        if (!overviewResult.ok) {
            await this.webviewView.webview.postMessage({
                type: 'error',
                message: overviewResult.error.message,
            });
            return;
        }
        this.latestOverview = overviewResult.value;
        this.routeView = await this.renderActiveRoute();
        await this.postState();
    }

    private async handleMemoryInboxMessage(message: WebviewMessage): Promise<void> {
        const inbox = this.ensureMemoryInbox();
        if (!inbox) {
            return;
        }
        const routeView = await inbox.handleMessage(toReviewPanelMessage(message));
        if (routeView) {
            this.routeView = routeView;
            await this.postState();
        }
    }

    private async postState(): Promise<void> {
        if (!this.webviewView || !this.bridge || !this.latestOverview) {
            return;
        }
        const state: ReviewStateMessage = {
            type: 'state',
            activeRoute: this.activeRoute,
            routes: this.routes(),
            capabilities: this.bridge.getCapabilities(),
            overview: this.latestOverview,
            routeView: this.routeView,
        };
        await this.webviewView.webview.postMessage(state);
    }

    private async renderActiveRoute(): Promise<ReviewRouteView | undefined> {
        if (this.activeRoute === 'memoryInbox') {
            const inbox = this.ensureMemoryInbox();
            return inbox?.refresh();
        }
        this.memoryInbox?.dispose();
        this.memoryInbox = undefined;
        if (!this.bridge) {
            return undefined;
        }
        if (this.activeRoute === 'staleView') {
            return this.renderStaleView();
        }
        if (this.activeRoute === 'evidenceInspector') {
            return this.renderEvidenceInspector();
        }
        if (this.activeRoute === 'eventTrace') {
            return this.renderEventTrace();
        }
        if (this.activeRoute === 'retrievalExplanation') {
            return this.renderRetrievalExplanation();
        }
        if (this.activeRoute === 'consolidationQueue') {
            return this.renderConsolidationQueue();
        }
        if (this.activeRoute === 'indexingHealth') {
            return this.renderIndexingHealth();
        }
        if (this.activeRoute === 'workspaceGraphHealth') {
            return this.renderWorkspaceGraphHealth();
        }
        return undefined;
    }

    private ensureMemoryInbox(): MountedMemoryInbox | undefined {
        if (!this.bridge) {
            return undefined;
        }
        if (!this.memoryInbox) {
            this.memoryInbox = mountMemoryInbox(this.memoryInboxHost(), this.bridge, this.i18n);
        }
        return this.memoryInbox;
    }

    private memoryInboxHost(): MemoryInboxHost {
        return {
            navigate: async (route, context) => {
                this.activeRoute = route;
                if (context?.memoryId) {
                    this.evidenceInspectorState.memoryId = context.memoryId;
                    this.eventTraceState.memoryId = context.memoryId;
                }
                await this.refreshState();
            },
            pickFilter: async ({ title, placeholder, selectedValues, options }) => {
                const picks = options.map((option) => ({
                    label: option.label,
                    detail: option.detail,
                    picked: selectedValues.includes(option.value),
                    value: option.value,
                }));
                const selection = await vscode.window.showQuickPick(picks, {
                    title,
                    placeHolder: placeholder,
                    canPickMany: true,
                    ignoreFocusOut: true,
                });
                return selection?.map((item) => item.value);
            },
            showError: (message) => {
                void vscode.window.showErrorMessage(message);
            },
        };
    }

    private async renderStaleView(): Promise<ReviewRouteView> {
        let html = '';
        await mountStaleView(
            {
                state: this.staleViewState,
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n
        );
        return {
            html,
            testId: 'review-route-staleView',
        };
    }

    private async renderEvidenceInspector(): Promise<ReviewRouteView> {
        let html = '';
        await mountEvidenceInspector(
            {
                state: this.evidenceInspectorState,
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n,
            this.evidenceInspectorState.memoryId
        );
        return {
            html,
            testId: 'review-route-evidenceInspector',
        };
    }

    private async renderEventTrace(): Promise<ReviewRouteView> {
        let html = '';
        await mountEventTraceView(
            {
                state: this.eventTraceState,
                rememberPage: (page) => {
                    this.eventTraceState.lastPage = page;
                },
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n,
            {
                sessionId: this.eventTraceState.sessionId,
                taskId: this.eventTraceState.taskId,
                memoryId: this.eventTraceState.memoryId,
                workspaceId: this.eventTraceState.workspaceId ?? this.latestOverview?.indexStatus.workspace,
            }
        );
        return {
            html,
            testId: 'review-route-eventTrace',
        };
    }

    private async renderRetrievalExplanation(): Promise<ReviewRouteView> {
        let html = '';
        await mountRetrievalExplanationView(
            {
                state: this.retrievalExplanationState,
                rememberExplanation: (explanation) => {
                    this.retrievalExplanationState.lastExplanation = explanation;
                },
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n,
            this.retrievalExplanationState.requestId
        );
        return {
            html,
            testId: 'review-route-retrievalExplanation',
        };
    }

    private async renderConsolidationQueue(): Promise<ReviewRouteView> {
        let html = '';
        await mountConsolidationQueueView(
            {
                state: this.consolidationQueueState,
                remember: (jobs) => {
                    this.consolidationQueueState.jobs = jobs;
                },
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n
        );
        return {
            html,
            testId: 'review-route-consolidationQueue',
        };
    }

    private async renderIndexingHealth(): Promise<ReviewRouteView> {
        let html = '';
        await mountIndexingHealthView(
            {
                state: this.indexingHealthState,
                remember: (health) => {
                    this.indexingHealthState.health = health;
                },
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n
        );
        return {
            html,
            testId: 'review-route-indexingHealth',
        };
    }

    private async renderWorkspaceGraphHealth(): Promise<ReviewRouteView> {
        let html = '';
        await mountWorkspaceGraphHealthView(
            {
                state: this.workspaceGraphHealthState,
                remember: (health) => {
                    this.workspaceGraphHealthState.health = health;
                },
                reportError(message) {
                    void vscode.window.showErrorMessage(message);
                },
                setContent(markup) {
                    html = markup;
                },
            },
            this.bridge as ReviewRpcBridgeContract,
            this.i18n
        );
        return {
            html,
            testId: 'review-route-workspaceGraphHealth',
        };
    }

    private async pickStaleStatusFilters(): Promise<void> {
        const picks: FilterPickItem[] = STALE_STATUS_VALUES.map((status) => ({
            label: this.i18n.t(`reviewStatus.${status}`),
            picked: this.staleViewState.selectedStatuses.includes(status),
            value: status,
        }));
        const selection = await vscode.window.showQuickPick(picks, {
            title: this.i18n.t('staleView.filterStatus'),
            placeHolder: this.i18n.t('staleView.filterStatusPlaceholder'),
            canPickMany: true,
            ignoreFocusOut: true,
        });
        if (!selection) {
            return;
        }
        this.staleViewState.selectedStatuses = normalizeFilterSelection(
            selection.map((item) => item.value),
            STALE_STATUS_VALUES
        );
        await this.refreshState();
    }

    private async pickStaleScopeFilters(): Promise<void> {
        const scopes: FilterPickItem[] = STALE_SCOPE_VALUES.map((scope) => ({
            label: this.i18n.t(`staleView.scope.${scope}`),
            picked: this.staleViewState.selectedScopes.includes(scope),
            value: scope,
        }));
        const selection = await vscode.window.showQuickPick(scopes, {
            title: this.i18n.t('staleView.filterScope'),
            placeHolder: this.i18n.t('staleView.filterScopePlaceholder'),
            canPickMany: true,
            ignoreFocusOut: true,
        });
        if (!selection) {
            return;
        }
        this.staleViewState.selectedScopes = normalizeFilterSelection(
            selection.map((item) => item.value),
            STALE_SCOPE_VALUES
        );
        await this.refreshState();
    }

    private async reverifyStaleMemory(memoryId: string): Promise<void> {
        if (!this.bridge) {
            return;
        }
        this.staleViewState.reverifyInFlightId = memoryId;
        delete this.staleViewState.rowErrors[memoryId];
        await this.refreshState();
        try {
            const result = await this.bridge.verifyMemory(memoryId);
            if (!result.ok) {
                this.staleViewState.rowErrors[memoryId] = result.error.message;
                await vscode.window.showErrorMessage(result.error.message);
            } else {
                await vscode.window.showInformationMessage(
                    this.i18n.t('staleView.verifySuccess', { status: result.value.status })
                );
            }
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            this.staleViewState.rowErrors[memoryId] = message;
            await vscode.window.showErrorMessage(message);
        } finally {
            this.staleViewState.reverifyInFlightId = undefined;
            await this.refreshState();
        }
    }

    private async reverifyEvidenceMemory(memoryId: string): Promise<void> {
        if (!this.bridge) {
            return;
        }
        this.evidenceInspectorState.reverifyInFlight = true;
        this.evidenceInspectorState.inlineError = undefined;
        await this.refreshState();
        try {
            const result = await this.bridge.verifyMemory(memoryId);
            if (!result.ok) {
                this.evidenceInspectorState.inlineError = result.error.message;
                await vscode.window.showErrorMessage(result.error.message);
            } else {
                await vscode.window.showInformationMessage(
                    this.i18n.t('evidenceInspector.verifySuccess', { status: result.value.status })
                );
            }
        } catch (error) {
            const message = error instanceof Error ? error.message : String(error);
            this.evidenceInspectorState.inlineError = message;
            await vscode.window.showErrorMessage(message);
        } finally {
            this.evidenceInspectorState.reverifyInFlight = false;
            await this.refreshState();
        }
    }

    private async openReference(reference: string): Promise<void> {
        const uri = resolveReferenceUri(reference);
        if (!uri) {
            await vscode.window.showErrorMessage(this.i18n.t('evidenceInspector.referenceResolveError'));
            return;
        }
        await vscode.commands.executeCommand('vscode.open', uri);
    }

    private async handleStaleViewMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'pickStaleStatusFilters':
                await this.pickStaleStatusFilters();
                return;
            case 'pickStaleScopeFilters':
                await this.pickStaleScopeFilters();
                return;
            case 'sortStaleView':
                if (message.sortBy && isStaleSortKey(message.sortBy)) {
                    this.staleViewState.sortDirection = this.staleViewState.sortBy === message.sortBy
                        && this.staleViewState.sortDirection === 'asc'
                        ? 'desc'
                        : 'asc';
                    this.staleViewState.sortBy = message.sortBy;
                }
                await this.refreshState();
                return;
            case 'openEvidenceInspector':
                if (message.memoryId) {
                    this.evidenceInspectorState.memoryId = message.memoryId;
                    this.evidenceInspectorState.inlineError = undefined;
                    this.activeRoute = 'evidenceInspector';
                    await this.refreshState();
                }
                return;
            case 'reverifyStaleMemory':
                if (message.memoryId) {
                    await this.reverifyStaleMemory(message.memoryId);
                }
                return;
            default:
                return;
        }
    }

    private async handleEvidenceInspectorMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'reverifyInspectorMemory':
                if (message.memoryId) {
                    await this.reverifyEvidenceMemory(message.memoryId);
                }
                return;
            case 'openMemoryReference':
                if (message.value) {
                    await this.openReference(message.value);
                }
                return;
            case 'openEventTraceForMemory':
            case 'openEventTraceForEvent':
                this.activeRoute = 'eventTrace';
                if (message.memoryId) {
                    this.eventTraceState.memoryId = message.memoryId;
                }
                await this.refreshState();
                return;
            default:
                return;
        }
    }

    private async handleEventTraceMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'pickEventTraceKinds':
                await this.pickEventTraceKinds();
                return;
            case 'pickEventTraceActors':
                await this.pickEventTraceActors();
                return;
            case 'pickEventTraceSession':
                await this.pickEventTraceScope('session');
                return;
            case 'pickEventTraceTask':
                await this.pickEventTraceScope('task');
                return;
            case 'pickEventTraceWorkspace':
                await this.pickEventTraceScope('workspace');
                return;
            case 'pickEventTraceBranch':
                await this.pickEventTraceBranch();
                return;
            case 'applyEventTraceWindow':
                this.eventTraceState.since = message.since?.trim() || undefined;
                this.eventTraceState.until = message.until?.trim() || undefined;
                this.eventTraceState.inlineError = validateIsoWindow(
                    this.eventTraceState.since,
                    this.eventTraceState.until,
                    this.i18n
                );
                this.eventTraceState.page = 1;
                await this.refreshState();
                return;
            case 'clearEventTraceWindow':
                this.eventTraceState.since = undefined;
                this.eventTraceState.until = undefined;
                this.eventTraceState.inlineError = undefined;
                this.eventTraceState.page = 1;
                await this.refreshState();
                return;
            case 'sortEventTrace':
                if (message.sortBy === 'timestamp' || message.sortBy === 'kind' || message.sortBy === 'actor') {
                    this.eventTraceState.sortDirection = this.eventTraceState.sortBy === message.sortBy
                        && this.eventTraceState.sortDirection === 'asc'
                        ? 'desc'
                        : 'asc';
                    this.eventTraceState.sortBy = message.sortBy;
                }
                await this.refreshState();
                return;
            case 'pageEventTrace':
                if (typeof message.page === 'number') {
                    this.eventTraceState.page = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'setEventTracePageSize':
                if (typeof message.pageSize === 'number') {
                    this.eventTraceState.pageSize = message.pageSize;
                    this.eventTraceState.page = 1;
                }
                await this.refreshState();
                return;
            case 'openEvidenceInspectorFromReference':
                if (message.value) {
                    const memoryId = extractMemoryId(message.value);
                    if (memoryId) {
                        this.evidenceInspectorState.memoryId = memoryId;
                        this.activeRoute = 'evidenceInspector';
                        await this.refreshState();
                    }
                }
                return;
            case 'openEventTraceReference':
                if (message.value) {
                    await this.openReference(message.value);
                }
                return;
            case 'copyEventPayload':
                if (message.eventId) {
                    await this.copyEventPayload(message.eventId);
                }
                return;
            case 'openEventPayloadInEditor':
                if (message.eventId) {
                    await this.openEventPayloadInEditor(message.eventId);
                }
                return;
            case 'openRetrievalExplanationFromEvent':
                if (message.eventId) {
                    this.retrievalExplanationState.requestId = extractRetrievalRequestId(
                        this.findTraceEvent(message.eventId)
                    );
                    this.activeRoute = 'retrievalExplanation';
                    await this.refreshState();
                }
                return;
            default:
                return;
        }
    }

    private async handleRetrievalExplanationMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'sortRetrievalExplanation':
                if (message.sortBy === 'score' || message.sortBy === 'source' || message.sortBy === 'decision') {
                    this.retrievalExplanationState.sortDirection =
                        this.retrievalExplanationState.sortBy === message.sortBy
                            && this.retrievalExplanationState.sortDirection === 'asc'
                            ? 'desc'
                            : 'asc';
                    this.retrievalExplanationState.sortBy = message.sortBy;
                }
                await this.refreshState();
                return;
            case 'pageRetrievalExplanation':
                if (typeof message.page === 'number') {
                    this.retrievalExplanationState.page = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'setRetrievalExplanationPageSize':
                if (typeof message.pageSize === 'number') {
                    this.retrievalExplanationState.pageSize = message.pageSize;
                    this.retrievalExplanationState.page = 1;
                }
                await this.refreshState();
                return;
            default:
                return;
        }
    }

    private async handleConsolidationQueueMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'pickConsolidationStatuses':
                await this.pickConsolidationFilter('status');
                return;
            case 'pickConsolidationKinds':
                await this.pickConsolidationFilter('kind');
                return;
            case 'pickConsolidationModes':
                await this.pickConsolidationFilter('mode');
                return;
            case 'clearConsolidationFilters':
                this.consolidationQueueState.selectedStatuses = [];
                this.consolidationQueueState.selectedKinds = [];
                this.consolidationQueueState.selectedModes = [];
                this.consolidationQueueState.page = 1;
                await this.refreshState();
                return;
            case 'sortConsolidationQueue':
                if (message.sortBy && isConsolidationSortKey(message.sortBy)) {
                    this.consolidationQueueState.sortDirection =
                        this.consolidationQueueState.sortBy === message.sortBy
                            && this.consolidationQueueState.sortDirection === 'asc'
                            ? 'desc'
                            : 'asc';
                    this.consolidationQueueState.sortBy = message.sortBy;
                }
                await this.refreshState();
                return;
            case 'pageConsolidationQueue':
                if (typeof message.page === 'number') {
                    this.consolidationQueueState.page = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'setConsolidationQueuePageSize':
                if (typeof message.pageSize === 'number') {
                    this.consolidationQueueState.pageSize = message.pageSize;
                    this.consolidationQueueState.page = 1;
                }
                await this.refreshState();
                return;
            case 'openConsolidationEventTrace':
                if (message.value) {
                    await this.openConsolidationJobTrace(message.value);
                }
                return;
            case 'retryConsolidationJob':
                if (message.value) {
                    await this.retryConsolidationJob(message.value);
                }
                return;
            case 'openConsolidationFailures':
                await this.openConsolidationFailureTrace();
                return;
            default:
                return;
        }
    }

    private async handleIndexingHealthMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'refreshIndexingHealthSection':
                if (message.value === 'pipeline' || message.value === 'vector' || message.value === 'fts' || message.value === 'eventLog') {
                    this.indexingHealthState.refreshingSection = message.value;
                    await this.refreshState();
                    this.indexingHealthState.refreshingSection = undefined;
                    await this.refreshState();
                }
                return;
            default:
                return;
        }
    }

    private async handleWorkspaceGraphHealthMessage(message: WebviewMessage): Promise<void> {
        switch (message.command) {
            case 'refreshWorkspaceGraphPanel':
                if (message.value && isWorkspaceGraphPanel(message.value)) {
                    this.workspaceGraphHealthState.refreshingPanel = message.value;
                    await this.refreshState();
                    this.workspaceGraphHealthState.refreshingPanel = undefined;
                    await this.refreshState();
                }
                return;
            case 'sortWorkspaceGraphFamilies':
                if (message.value) {
                    const parsed = parseGraphFamilySort(message.value);
                    if (parsed) {
                        if (parsed.panel === 'nodes') {
                            this.workspaceGraphHealthState.nodeSortDirection =
                                this.workspaceGraphHealthState.nodeSortBy === parsed.sortBy
                                    && this.workspaceGraphHealthState.nodeSortDirection === 'asc'
                                    ? 'desc'
                                    : 'asc';
                            this.workspaceGraphHealthState.nodeSortBy = parsed.sortBy;
                        } else {
                            this.workspaceGraphHealthState.edgeSortDirection =
                                this.workspaceGraphHealthState.edgeSortBy === parsed.sortBy
                                    && this.workspaceGraphHealthState.edgeSortDirection === 'asc'
                                    ? 'desc'
                                    : 'asc';
                            this.workspaceGraphHealthState.edgeSortBy = parsed.sortBy;
                        }
                    }
                }
                await this.refreshState();
                return;
            case 'sortWorkspaceGraphDiagnostics':
                if (message.value === 'identity' || message.value === 'reason') {
                    this.workspaceGraphHealthState.diagnosticSortDirection =
                        this.workspaceGraphHealthState.diagnosticSortBy === message.value
                            && this.workspaceGraphHealthState.diagnosticSortDirection === 'asc'
                            ? 'desc'
                            : 'asc';
                    this.workspaceGraphHealthState.diagnosticSortBy = message.value;
                }
                await this.refreshState();
                return;
            case 'pageWorkspaceGraphNodes':
                if (typeof message.page === 'number') {
                    this.workspaceGraphHealthState.nodePage = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'pageWorkspaceGraphEdges':
                if (typeof message.page === 'number') {
                    this.workspaceGraphHealthState.edgePage = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'pageWorkspaceGraphBroken':
                if (typeof message.page === 'number') {
                    this.workspaceGraphHealthState.brokenPage = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'pageWorkspaceGraphStale':
                if (typeof message.page === 'number') {
                    this.workspaceGraphHealthState.stalePage = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'pageWorkspaceGraphOrphan':
                if (typeof message.page === 'number') {
                    this.workspaceGraphHealthState.orphanPage = Math.max(1, message.page);
                }
                await this.refreshState();
                return;
            case 'setWorkspaceGraphPageSize':
                if (typeof message.pageSize === 'number') {
                    this.workspaceGraphHealthState.pageSize = message.pageSize;
                    this.workspaceGraphHealthState.nodePage = 1;
                    this.workspaceGraphHealthState.edgePage = 1;
                    this.workspaceGraphHealthState.brokenPage = 1;
                    this.workspaceGraphHealthState.stalePage = 1;
                    this.workspaceGraphHealthState.orphanPage = 1;
                }
                await this.refreshState();
                return;
            case 'openWorkspaceGraphReference':
                if (message.value) {
                    await this.openReference(message.value);
                }
                return;
            case 'openWorkspaceGraphEvidence':
                if (message.value) {
                    this.evidenceInspectorState.memoryId = message.value;
                    this.activeRoute = 'evidenceInspector';
                    await this.refreshState();
                }
                return;
            default:
                return;
        }
    }

    private async pickConsolidationFilter(kind: 'status' | 'kind' | 'mode'): Promise<void> {
        const jobs = this.consolidationQueueState.jobs?.jobs ?? [];
        const values = [...new Set(jobs.map((job) => {
            if (kind === 'status') {
                return job.status;
            }
            if (kind === 'kind') {
                return job.kind;
            }
            return job.mode;
        }))].sort((left, right) => left.localeCompare(right));
        const selected = kind === 'status'
            ? this.consolidationQueueState.selectedStatuses
            : kind === 'kind'
                ? this.consolidationQueueState.selectedKinds
                : this.consolidationQueueState.selectedModes;
        const picks: FilterPickItem[] = values.map((value) => ({
            label: this.i18n.has(`consolidationQueueView.${kind}.${value}`)
                ? this.i18n.t(`consolidationQueueView.${kind}.${value}`)
                : value,
            picked: selected.includes(value),
            value,
        }));
        const selection = await vscode.window.showQuickPick(picks, {
            title: this.i18n.t(`consolidationQueueView.filters.${kind}Title`),
            placeHolder: this.i18n.t(`consolidationQueueView.filters.${kind}Placeholder`),
            canPickMany: true,
            ignoreFocusOut: true,
        });
        if (!selection) {
            return;
        }
        const normalized = normalizeFilterSelection(selection.map((item) => item.value), values);
        if (kind === 'status') {
            this.consolidationQueueState.selectedStatuses = normalized;
        } else if (kind === 'kind') {
            this.consolidationQueueState.selectedKinds = normalized;
        } else {
            this.consolidationQueueState.selectedModes = normalized;
        }
        this.consolidationQueueState.page = 1;
        await this.refreshState();
    }

    private async openConsolidationJobTrace(jobId: string): Promise<void> {
        const job = this.consolidationQueueState.jobs?.jobs.find((entry) => entry.jobId === jobId);
        if (!job) {
            return;
        }
        this.activeRoute = 'eventTrace';
        this.eventTraceState.kinds = ['memory_consolidated', 'consolidation_failed'];
        this.eventTraceState.memoryId = undefined;
        this.eventTraceState.sessionId = job.sessionId;
        this.eventTraceState.taskId = job.taskId;
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async retryConsolidationJob(jobId: string): Promise<void> {
        if (!this.bridge) {
            return;
        }
        const job = this.consolidationQueueState.jobs?.jobs.find((entry) => entry.jobId === jobId);
        if (!job?.sessionId) {
            await vscode.window.showErrorMessage(this.i18n.t('consolidationQueueView.retryMissingSession'));
            return;
        }
        const confirm = await vscode.window.showInformationMessage(
            this.i18n.t('consolidationQueueView.retryConfirm', { value: job.sessionId }),
            { modal: true },
            this.i18n.t('consolidationQueueView.actions.retry')
        );
        if (!confirm) {
            return;
        }
        this.consolidationQueueState.retryingJobId = jobId;
        await this.refreshState();
        try {
            const result = await this.bridge.retryConsolidationSession(job.sessionId, normalizeConsolidationMode(job.mode));
            if (!result.ok) {
                await vscode.window.showErrorMessage(result.error.message);
            } else {
                await vscode.window.showInformationMessage(this.i18n.t('consolidationQueueView.retrySuccess'));
            }
        } finally {
            this.consolidationQueueState.retryingJobId = undefined;
            await this.refreshState();
        }
    }

    private async openConsolidationFailureTrace(): Promise<void> {
        this.activeRoute = 'eventTrace';
        this.eventTraceState.kinds = ['memory_consolidated', 'consolidation_failed'];
        this.eventTraceState.memoryId = undefined;
        this.eventTraceState.sessionId = undefined;
        this.eventTraceState.taskId = undefined;
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async pickEventTraceKinds(): Promise<void> {
        const picks: FilterPickItem[] = EVENT_TRACE_KIND_VALUES.map((kind) => ({
            label: this.i18n.t(`eventTraceView.kind.${kind}`),
            picked: this.eventTraceState.kinds.includes(kind),
            value: kind,
        }));
        const selection = await vscode.window.showQuickPick(picks, {
            title: this.i18n.t('eventTraceView.filters.kindTitle'),
            placeHolder: this.i18n.t('eventTraceView.filters.kindPlaceholder'),
            canPickMany: true,
            ignoreFocusOut: true,
        });
        if (!selection) {
            return;
        }
        this.eventTraceState.kinds = normalizeFilterSelection(
            selection.map((item) => item.value),
            EVENT_TRACE_KIND_VALUES
        );
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async pickEventTraceActors(): Promise<void> {
        const actors = uniqueEventTraceValues(this.eventTraceState.lastPage?.events.map((event) => event.actor) ?? []);
        const picks: FilterPickItem[] = actors.map((actor) => ({
            label: formatActorLabel(actor, this.i18n),
            picked: this.eventTraceState.selectedActors.includes(actor),
            value: actor,
        }));
        const selection = await vscode.window.showQuickPick(picks, {
            title: this.i18n.t('eventTraceView.filters.actorTitle'),
            placeHolder: this.i18n.t('eventTraceView.filters.actorPlaceholder'),
            canPickMany: true,
            ignoreFocusOut: true,
        });
        if (!selection) {
            return;
        }
        this.eventTraceState.selectedActors = selection.map((item) => item.value);
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async pickEventTraceScope(kind: 'session' | 'task' | 'workspace'): Promise<void> {
        const values = uniqueEventTraceValues((this.eventTraceState.lastPage?.events ?? []).map((event) => {
            if (kind === 'session') {
                return event.sessionId;
            }
            if (kind === 'task') {
                return event.taskId ?? '';
            }
            return event.workspaceId;
        }));
        const selection = await pickSingleValue(
            values,
            this.i18n.t(`eventTraceView.filters.${kind}Title`),
            this.i18n.t(`eventTraceView.filters.${kind}Placeholder`),
            this.i18n.t('eventTraceView.filters.all')
        );
        if (selection === undefined) {
            return;
        }
        this.eventTraceState.sessionId = kind === 'session' ? selection : undefined;
        this.eventTraceState.taskId = kind === 'task' ? selection : undefined;
        this.eventTraceState.workspaceId = kind === 'workspace'
            ? selection
            : this.latestOverview?.indexStatus.workspace;
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async pickEventTraceBranch(): Promise<void> {
        const values = uniqueEventTraceValues((this.eventTraceState.lastPage?.events ?? []).map((event) => event.branch));
        const selection = await pickSingleValue(
            values,
            this.i18n.t('eventTraceView.filters.branchTitle'),
            this.i18n.t('eventTraceView.filters.branchPlaceholder'),
            this.i18n.t('eventTraceView.filters.all')
        );
        if (selection === undefined) {
            return;
        }
        this.eventTraceState.branch = selection || undefined;
        this.eventTraceState.page = 1;
        await this.refreshState();
    }

    private async copyEventPayload(eventId: string): Promise<void> {
        const event = this.findTraceEvent(eventId);
        if (!event?.payload) {
            await vscode.window.showErrorMessage(this.i18n.t('eventTraceView.copyPayloadError'));
            return;
        }
        try {
            await vscode.env.clipboard.writeText(JSON.stringify(event.payload, null, 2));
            await vscode.window.showInformationMessage(this.i18n.t('eventTraceView.copyPayloadSuccess'));
        } catch (error) {
            await vscode.window.showErrorMessage(error instanceof Error ? error.message : String(error));
        }
    }

    private async openEventPayloadInEditor(eventId: string): Promise<void> {
        const event = this.findTraceEvent(eventId);
        if (!event?.payload) {
            await vscode.window.showErrorMessage(this.i18n.t('eventTraceView.openPayloadError'));
            return;
        }
        try {
            const document = await vscode.workspace.openTextDocument({
                language: 'json',
                content: JSON.stringify(event.payload, null, 2),
            });
            await vscode.window.showTextDocument(document, { preview: false });
        } catch (error) {
            await vscode.window.showErrorMessage(error instanceof Error ? error.message : String(error));
        }
    }

    private findTraceEvent(eventId: string) {
        return this.eventTraceState.lastPage?.events.find((event) => event.eventId === eventId);
    }

    private routes(): ReviewRouteDefinition[] {
        return routeIds().map((id) => ({
            id,
            label: this.i18n.t(`reviewPanel.route.${id}`),
            description: this.i18n.t(`reviewPanel.description.${id}`),
        }));
    }

    private renderHtml(webview: vscode.Webview): string {
        const nonce = crypto.randomBytes(16).toString('hex');
        const catalog = loadReviewCatalog();
        const statusBadgeScript = renderStatusBadge.toString();
        const proposalDialogScript = serializeProposalDialog();
        const promotionQueueScript = serializePromotionQueue();
        const contradictionQueueScript = serializeContradictionQueue();
        const bootstrap = JSON.stringify({
            catalog,
            title: this.i18n.t('reviewPanel.title'),
            subtitle: this.i18n.t('reviewPanel.subtitle'),
            loading: this.i18n.t('reviewPanel.loading'),
            empty: this.i18n.t('reviewPanel.empty'),
            error: this.i18n.t('reviewPanel.error'),
            refresh: this.i18n.t('reviewPanel.refresh'),
            retry: this.i18n.t('reviewPanel.retry'),
            overview: this.i18n.t('reviewPanel.overview'),
            overviewDescription: this.i18n.t('reviewPanel.overviewDescription'),
            routePlaceholder: this.i18n.t('reviewPanel.routePlaceholder'),
            routeUnavailable: this.i18n.t('reviewPanel.routeUnavailable'),
            capabilityDirect: this.i18n.t('reviewPanel.capabilityDirect'),
            capabilityComposed: this.i18n.t('reviewPanel.capabilityComposed'),
            capabilityUnavailable: this.i18n.t('reviewPanel.capabilityUnavailable'),
            summaryIndexing: this.i18n.t('reviewPanel.summaryIndexing'),
            summaryMemories: this.i18n.t('reviewPanel.summaryMemories'),
            summaryMetrics: this.i18n.t('reviewPanel.summaryMetrics'),
            summaryGraph: this.i18n.t('reviewPanel.summaryGraph'),
            daemonStatus: this.i18n.t('reviewOverview.daemonStatus'),
            memoryCount: this.i18n.t('reviewOverview.memoryCount'),
            signalCount: this.i18n.t('reviewOverview.signalCount'),
            nodeCount: this.i18n.t('reviewOverview.nodeCount'),
            fileCount: this.i18n.t('reviewOverview.fileCount'),
            edgeCount: this.i18n.t('reviewOverview.edgeCount'),
            languages: this.i18n.t('reviewOverview.languages'),
            none: this.i18n.t('reviewOverview.none'),
            unknown: this.i18n.t('reviewOverview.unknown'),
            routeSupport: this.i18n.t('reviewOverview.routeSupport'),
        });
        const csp = [
            "default-src 'none'",
            `img-src ${webview.cspSource} data:`,
            `style-src ${webview.cspSource} 'unsafe-inline'`,
            `script-src 'nonce-${nonce}'`,
        ].join('; ');

        return `<!DOCTYPE html>
<html lang="en">
<head>
  <meta charset="UTF-8" />
  <meta http-equiv="Content-Security-Policy" content="${csp}" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>${escapeHtml(this.i18n.t('reviewPanel.title'))}</title>
  <style>
    :root {
      color-scheme: light dark;
      --border: color-mix(in srgb, var(--vscode-panel-border) 72%, transparent);
      --muted: var(--vscode-descriptionForeground);
      --surface: color-mix(in srgb, var(--vscode-editor-background) 92%, var(--vscode-sideBar-background));
      --surface-alt: color-mix(in srgb, var(--vscode-editor-background) 84%, var(--vscode-sideBar-background));
      --accent: var(--vscode-button-background);
      --accent-foreground: var(--vscode-button-foreground);
      --warning: var(--vscode-editorWarning-foreground);
      --error: var(--vscode-errorForeground);
      --success: var(--vscode-testing-iconPassed);
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: var(--vscode-font-family);
      color: var(--vscode-foreground);
      background: radial-gradient(circle at top right, color-mix(in srgb, var(--accent) 12%, transparent), transparent 42%),
        linear-gradient(180deg, var(--surface), var(--vscode-editor-background));
    }
    button, input, textarea, select {
      font: inherit;
      color: inherit;
    }
    button {
      border: 1px solid var(--border);
      background: var(--surface-alt);
      color: inherit;
      border-radius: 10px;
      padding: 8px 12px;
      cursor: pointer;
    }
    button:hover { border-color: var(--accent); }
    button[disabled] { opacity: 0.6; cursor: default; }
    .shell { display: grid; grid-template-columns: 220px 1fr; min-height: 100vh; }
    .nav {
      border-right: 1px solid var(--border);
      padding: 16px 12px;
      background: color-mix(in srgb, var(--surface-alt) 84%, transparent);
    }
    .nav h1 {
      font-size: 18px;
      margin: 0;
    }
    .nav p {
      color: var(--muted);
      font-size: 12px;
      line-height: 1.4;
      margin: 8px 0 16px;
    }
    .nav-list {
      display: flex;
      flex-direction: column;
      gap: 6px;
    }
    .nav-list button {
      text-align: left;
      width: 100%;
    }
    .nav-list button.active {
      background: var(--accent);
      color: var(--accent-foreground);
      border-color: transparent;
    }
    .content { padding: 18px; display: flex; flex-direction: column; gap: 16px; }
    .toolbar { display: flex; justify-content: space-between; align-items: center; gap: 12px; }
    .toolbar h2 { margin: 0; font-size: 18px; }
    .toolbar p { margin: 4px 0 0; color: var(--muted); font-size: 12px; }
    .summary-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
      gap: 12px;
    }
    .card, .placeholder, .banner, .table-card {
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 14px;
      background: color-mix(in srgb, var(--surface-alt) 72%, transparent);
    }
    .card small, .placeholder small {
      display: block;
      color: var(--muted);
      margin-bottom: 6px;
      text-transform: uppercase;
      letter-spacing: 0.04em;
      font-size: 11px;
    }
    .card strong {
      display: block;
      font-size: 22px;
      line-height: 1.2;
    }
    .banner.error {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 10%, transparent);
    }
    .placeholder h3 { margin: 0 0 8px; font-size: 16px; }
    .placeholder p { margin: 0 0 12px; color: var(--muted); line-height: 1.5; }
    .support {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 8px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 12px;
    }
    .support.direct { border-color: color-mix(in srgb, var(--success) 50%, var(--border)); }
    .support.composed { border-color: color-mix(in srgb, var(--warning) 50%, var(--border)); }
    .support.unsupported { border-color: color-mix(in srgb, var(--error) 50%, var(--border)); }
    .filter-bar, .filter-summary, .pagination-bar, .pagination-controls, .page-size-group, .row-actions {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
      align-items: center;
    }
    .filter-summary {
      margin-top: 10px;
      color: var(--muted);
      font-size: 12px;
    }
    .table-card { display: flex; flex-direction: column; gap: 14px; }
    .table-wrap { overflow-x: auto; }
    .review-table {
      width: 100%;
      border-collapse: collapse;
    }
    .review-table th, .review-table td {
      text-align: left;
      padding: 10px 12px;
      border-bottom: 1px solid var(--border);
      vertical-align: top;
      font-size: 12px;
    }
    .review-table tbody tr { cursor: pointer; }
    .review-table tbody tr:hover {
      background: color-mix(in srgb, var(--accent) 8%, transparent);
    }
    .sort-button, .link-button {
      border: 0;
      background: transparent;
      padding: 0;
      color: inherit;
      border-radius: 0;
    }
    .sort-button:hover, .link-button:hover {
      color: var(--accent);
      border-color: transparent;
    }
    .content-cell {
      max-width: 420px;
      line-height: 1.5;
    }
    .badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 3px 9px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 11px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: 0.04em;
    }
    .badge-success {
      border-color: color-mix(in srgb, var(--success) 64%, var(--border));
      background: color-mix(in srgb, var(--success) 12%, transparent);
    }
    .badge-warning {
      border-color: color-mix(in srgb, var(--warning) 64%, var(--border));
      background: color-mix(in srgb, var(--warning) 12%, transparent);
    }
    .badge-error {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 12%, transparent);
    }
    .badge-info {
      border-color: color-mix(in srgb, var(--accent) 64%, var(--border));
      background: color-mix(in srgb, var(--accent) 12%, transparent);
    }
    .badge-ghost {
      background: color-mix(in srgb, var(--surface) 86%, transparent);
    }
    .empty-state {
      border: 1px dashed var(--border);
      border-radius: 12px;
      padding: 20px;
      text-align: center;
    }
    .empty-state h3 {
      margin: 0 0 8px;
      font-size: 16px;
    }
    .empty-state p { margin: 0; color: var(--muted); }
    .route-stack, .panel-block, .cell-stack, .list-stack {
      display: flex;
      flex-direction: column;
      gap: 12px;
    }
    .field-stack {
      display: flex;
      flex-direction: column;
      gap: 4px;
      min-width: 220px;
    }
    .field-stack span,
    .muted-text {
      color: var(--muted);
      font-size: 12px;
    }
    .field-stack input {
      border: 1px solid var(--border);
      border-radius: 10px;
      background: color-mix(in srgb, var(--surface) 90%, transparent);
      padding: 8px 10px;
    }
    .route-toolbar {
      display: flex;
      justify-content: space-between;
      align-items: flex-start;
      gap: 12px;
    }
    .route-toolbar h3, .panel-block h4 {
      margin: 0;
    }
    .route-toolbar p, .panel-block p {
      margin: 0;
      color: var(--muted);
      line-height: 1.5;
    }
    .toolbar-actions {
      display: flex;
      flex-wrap: wrap;
      gap: 8px;
    }
    .filter-pill {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 4px 10px;
      border-radius: 999px;
      border: 1px solid var(--border);
      background: color-mix(in srgb, var(--surface) 86%, transparent);
      font-size: 12px;
    }
    .table-shell {
      overflow-x: auto;
      border: 1px solid var(--border);
      border-radius: 12px;
      background: color-mix(in srgb, var(--surface) 88%, transparent);
    }
    .data-table {
      width: 100%;
      border-collapse: collapse;
      min-width: 860px;
    }
    .data-table th, .data-table td {
      text-align: left;
      padding: 10px 12px;
      border-bottom: 1px solid var(--border);
      vertical-align: top;
      font-size: 12px;
    }
    .table-sort {
      border: 0;
      background: transparent;
      padding: 0;
      border-radius: 0;
    }
    .table-sort:hover {
      border-color: transparent;
      color: var(--accent);
    }
    .reference-stack {
      display: flex;
      flex-wrap: wrap;
      gap: 6px;
    }
    .reference-chip {
      border-radius: 999px;
      padding: 4px 8px;
      font-size: 11px;
    }
    .row-link {
      cursor: pointer;
    }
    .row-link:hover {
      background: color-mix(in srgb, var(--accent) 8%, transparent);
    }
    .status-badge {
      display: inline-flex;
      align-items: center;
      gap: 6px;
      padding: 3px 9px;
      border-radius: 999px;
      border: 1px solid var(--border);
      font-size: 11px;
      font-weight: 600;
      text-transform: uppercase;
      letter-spacing: 0.04em;
    }
    .status-badge--success {
      border-color: color-mix(in srgb, var(--success) 64%, var(--border));
      background: color-mix(in srgb, var(--success) 12%, transparent);
    }
    .status-badge--warning {
      border-color: color-mix(in srgb, var(--warning) 64%, var(--border));
      background: color-mix(in srgb, var(--warning) 12%, transparent);
    }
    .status-badge--error {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 12%, transparent);
    }
    .status-badge--danger {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      background: color-mix(in srgb, var(--error) 12%, transparent);
    }
    .status-badge--info {
      border-color: color-mix(in srgb, var(--accent) 64%, var(--border));
      background: color-mix(in srgb, var(--accent) 12%, transparent);
    }
    .status-badge--neutral {
      background: color-mix(in srgb, var(--surface) 86%, transparent);
    }
    .status-badge--muted {
      background: color-mix(in srgb, var(--surface) 86%, transparent);
    }
    .inline-error {
      color: var(--error);
      font-size: 12px;
      line-height: 1.4;
    }
    .panel-block {
      border: 1px solid var(--border);
      border-radius: 14px;
      padding: 14px;
      background: color-mix(in srgb, var(--surface-alt) 72%, transparent);
    }
    .queue-surface, .proposal-dialog__body, .proposal-dialog__footer {
      display: flex;
      flex-direction: column;
      gap: 12px;
    }
    .proposal-dialog {
      width: min(960px, calc(100vw - 32px));
      border: 1px solid var(--border);
      border-radius: 16px;
      padding: 0;
      color: var(--vscode-foreground);
      background: color-mix(in srgb, var(--surface) 96%, var(--vscode-editor-background));
    }
    .proposal-dialog::backdrop {
      background: rgba(0, 0, 0, 0.45);
    }
    .proposal-dialog__surface {
      display: flex;
      flex-direction: column;
      gap: 16px;
      padding: 18px;
    }
    .proposal-dialog__header,
    .proposal-dialog__actions,
    .table-actions {
      display: flex;
      align-items: center;
      justify-content: space-between;
      gap: 8px;
      flex-wrap: wrap;
    }
    .proposal-dialog__subtitle,
    .muted {
      color: var(--muted);
      margin: 4px 0 0;
    }
    .proposal-dialog__section {
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 12px;
      background: color-mix(in srgb, var(--surface-alt) 70%, transparent);
    }
    .proposal-dialog__section h4 {
      margin: 0 0 10px;
    }
    .proposal-dialog__reason,
    .proposal-dialog__confirm {
      display: flex;
      flex-direction: column;
      gap: 6px;
    }
    .proposal-dialog textarea,
    .proposal-dialog input {
      width: 100%;
      border: 1px solid var(--border);
      background: color-mix(in srgb, var(--surface) 86%, transparent);
      border-radius: 10px;
      padding: 10px 12px;
    }
    .detail-grid, .diff-grid {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
      gap: 10px;
    }
    .diff-grid {
      grid-template-columns: repeat(auto-fit, minmax(280px, 1fr));
    }
    .detail-grid div, .diff-panel {
      border: 1px solid var(--border);
      border-radius: 10px;
      padding: 10px 12px;
      background: color-mix(in srgb, var(--surface) 88%, transparent);
    }
    .detail-grid span {
      display: block;
      color: var(--muted);
      font-size: 11px;
      margin-bottom: 6px;
    }
    .detail-list {
      margin: 0;
      padding-left: 18px;
    }
    .diff-panel pre {
      margin: 8px 0 0;
      white-space: pre-wrap;
      word-break: break-word;
      font-size: 11px;
      line-height: 1.45;
    }
    .primary-button {
      background: var(--accent);
      color: var(--accent-foreground);
      border-color: transparent;
    }
    .danger-button {
      border-color: color-mix(in srgb, var(--error) 64%, var(--border));
      color: var(--error);
    }
    .icon-button {
      min-width: 88px;
    }
    .hidden {
      display: none;
    }
    .list-card {
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 12px;
      display: flex;
      flex-direction: column;
      gap: 6px;
      background: color-mix(in srgb, var(--surface) 90%, transparent);
    }
    .list-card small {
      color: var(--muted);
    }
    .dialog-shell {
      min-width: min(960px, 90vw);
      max-width: 90vw;
      max-height: 85vh;
      overflow: auto;
      border: 1px solid var(--border);
      border-radius: 16px;
      padding: 16px;
      background: var(--vscode-editor-background);
    }
    .payload-dialog {
      border: 0;
      padding: 0;
      background: transparent;
    }
    .payload-dialog::backdrop {
      background: color-mix(in srgb, var(--vscode-editor-background) 30%, black);
    }
    .route-toolbar.compact {
      align-items: center;
      margin-bottom: 12px;
    }
    .json-tree, .signal-list {
      display: flex;
      flex-direction: column;
      gap: 8px;
    }
    .json-node {
      padding-left: 14px;
      border-left: 1px solid var(--border);
    }
    .json-key { color: var(--accent); }
    .json-string { color: var(--success); }
    .json-number { color: var(--warning); }
    .json-boolean { color: var(--accent); }
    .json-null { color: var(--muted); }
    .signal-row {
      display: flex;
      justify-content: space-between;
      gap: 12px;
      border: 1px solid var(--border);
      border-radius: 10px;
      padding: 8px 10px;
      background: color-mix(in srgb, var(--surface) 90%, transparent);
    }
    .placeholder.compact {
      padding: 16px;
    }
    .pagination-bar {
      justify-content: space-between;
      color: var(--muted);
      font-size: 12px;
    }
    .page-size-group button.active {
      background: var(--accent);
      color: var(--accent-foreground);
      border-color: transparent;
    }
    .sr-only {
      position: absolute;
      width: 1px;
      height: 1px;
      padding: 0;
      margin: -1px;
      overflow: hidden;
      clip: rect(0, 0, 0, 0);
      border: 0;
      white-space: nowrap;
    }
    .kv {
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(180px, 1fr));
      gap: 10px;
    }
    .kv div {
      border: 1px solid var(--border);
      border-radius: 12px;
      padding: 10px 12px;
      background: color-mix(in srgb, var(--surface) 86%, transparent);
    }
    .kv span {
      display: block;
      color: var(--muted);
      font-size: 11px;
      margin-bottom: 6px;
    }
    .skeleton {
      height: 76px;
      border-radius: 14px;
      border: 1px solid var(--border);
      background: linear-gradient(90deg, transparent, color-mix(in srgb, var(--accent) 14%, transparent), transparent);
      background-size: 240px 100%;
      animation: shimmer 1.2s infinite linear;
    }
    @keyframes shimmer {
      from { background-position: -240px 0; }
      to { background-position: 240px 0; }
    }
    @media (max-width: 900px) {
      .shell { grid-template-columns: 1fr; }
      .nav { border-right: 0; border-bottom: 1px solid var(--border); }
    }
  </style>
</head>
<body>
  <div id="app"></div>
  <script nonce="${nonce}">
    const vscode = acquireVsCodeApi();
    const copy = ${bootstrap};
    const renderStatusBadge = ${statusBadgeScript};
    ${proposalDialogScript}
    ${promotionQueueScript}
    ${contradictionQueueScript}
    const state = {
      loading: true,
      error: '',
      payload: undefined,
    };
    const rpcPending = new Map();
    const i18n = createInlineI18n(copy.catalog);

    function createInlineI18n(catalog) {
      return {
        t(key, params) {
          const template = lookup(catalog, key) ?? key;
          if (!params) {
            return template;
          }
          return template.replace(/\\{([^}]+)\\}/g, (_match, name) => {
            const value = params[name];
            return value === undefined ? '{' + name + '}' : String(value);
          });
        },
      };
    }

    function lookup(tree, key) {
      const parts = key.split('.');
      let current = tree;
      for (const part of parts) {
        if (!current || typeof current === 'string') {
          return undefined;
        }
        current = current[part];
      }
      return typeof current === 'string' ? current : undefined;
    }

    function reviewBridge(method, params) {
      const requestId = String(Date.now()) + '-' + Math.random().toString(16).slice(2);
      vscode.postMessage({ command: 'rpcRequest', requestId, method, params });
      return new Promise((resolve, reject) => {
        rpcPending.set(requestId, { resolve, reject });
      });
    }

    const bridge = {
      listPromotionProposals() {
        return reviewBridge('listPromotionProposals', {});
      },
      applyPromotion(proposalId, reason) {
        return reviewBridge('applyPromotion', { proposalId, reason });
      },
      rejectPromotion(proposalId, reason) {
        return reviewBridge('rejectPromotion', { proposalId, reason });
      },
      listContradictions(anchor) {
        return reviewBridge('listContradictions', { anchor });
      },
      getMemoryEvidence(memoryId) {
        return reviewBridge('getMemoryEvidence', { memoryId });
      },
      applyContradictionResolution(args) {
        return reviewBridge('applyContradictionResolution', args);
      },
      rejectContradictionResolution(args) {
        return reviewBridge('rejectContradictionResolution', args);
      },
    };

    function reportReviewViewError(route, message) {
      vscode.postMessage({ command: 'reviewViewError', route, value: message });
    }

    function root() {
      return document.getElementById('app');
    }

    function render() {
      const host = root();
      if (!host) {
        return;
      }
      if (state.loading) {
        host.innerHTML = '<div class="content"><div class="toolbar"><div><h2>' + escape(copy.loading) + '</h2></div></div><div class="summary-grid"><div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div><div class="skeleton"></div></div></div>';
        return;
      }
      if (state.error) {
        host.innerHTML = '<div class="content"><div class="banner error" role="alert">' + escape(state.error) + '</div><button data-command="refresh">' + escape(copy.retry) + '</button></div>';
        return;
      }
      if (!state.payload) {
        host.innerHTML = '<div class="content"><div class="placeholder"><h3>' + escape(copy.empty) + '</h3></div></div>';
        return;
      }
      host.innerHTML = renderShell(state.payload);
      mountActiveRoute();
    }

    function renderShell(payload) {
      const routeCards = payload.routes.map((route) => {
        const active = payload.activeRoute === route.id ? 'active' : '';
        return '<button class="' + active + '" data-command="navigate" data-route="' + escapeAttr(route.id) + '">' + escape(route.label) + '</button>';
      }).join('');
      return '<div class="shell">' +
        '<aside class="nav">' +
        '<h1>' + escape(copy.title) + '</h1>' +
        '<p>' + escape(copy.subtitle) + '</p>' +
        '<div class="nav-list">' + routeCards + '</div>' +
        '</aside>' +
        '<main class="content">' +
        renderToolbar(payload) +
        renderSummary(payload.overview) +
        renderRoute(payload) +
        '</main>' +
        '</div>';
    }

    function renderToolbar(payload) {
      const route = payload.routes.find((entry) => entry.id === payload.activeRoute);
      const description = route ? route.description : copy.overviewDescription;
      const title = route ? route.label : copy.overview;
      return '<div class="toolbar">' +
        '<div><h2>' + escape(title) + '</h2><p>' + escape(description) + '</p></div>' +
        '<button data-command="refresh">' + escape(copy.refresh) + '</button>' +
        '</div>';
    }

    function renderSummary(overview) {
      const languages = Object.keys(overview.indexStatus.languages || {}).join(', ') || copy.none;
      return '<section class="summary-grid" aria-label="' + escapeAttr(copy.overview) + '">' +
        summaryCard(copy.summaryIndexing, overview.indexStatus.status || copy.unknown) +
        summaryCard(copy.summaryMemories, String(overview.memoryList.count || 0)) +
        summaryCard(copy.summaryMetrics, String((overview.metrics.signals || []).length)) +
        summaryCard(copy.summaryGraph, String(overview.indexStatus.nodes || 0)) +
        '</section>' +
        '<section class="kv">' +
        kvItem(copy.daemonStatus, overview.indexStatus.status || copy.unknown) +
        kvItem(copy.memoryCount, String(overview.memoryList.count || 0)) +
        kvItem(copy.signalCount, String((overview.metrics.signals || []).length)) +
        kvItem(copy.nodeCount, String(overview.indexStatus.nodes || 0)) +
        kvItem(copy.fileCount, String(overview.indexStatus.files || 0)) +
        kvItem(copy.edgeCount, String(overview.indexStatus.edges || 0)) +
        kvItem(copy.languages, languages) +
        '</section>';
    }

    function renderRoute(payload) {
      if (payload.routeView && payload.routeView.html) {
        return payload.routeView.html;
      }
      if (payload.activeRoute === 'promotionQueue' || payload.activeRoute === 'contradictionQueue') {
        const queueTestId = payload.activeRoute === 'promotionQueue'
          ? 'review-promotion-queue'
          : 'review-contradiction-queue';
        return '<section class="panel-block" data-testid="' + escapeAttr(queueTestId) + '"><div id="review-queue-host"></div></section>';
      }
      const route = payload.routes.find((entry) => entry.id === payload.activeRoute);
      const support = payload.capabilities[payload.activeRoute];
      const supportLabel = support.mode === 'direct'
        ? copy.capabilityDirect
        : support.mode === 'composed'
          ? copy.capabilityComposed
          : copy.capabilityUnavailable;
      return '<section class="placeholder" data-testid="review-route-' + escapeAttr(payload.activeRoute) + '">' +
        '<div class="support ' + escapeAttr(support.mode) + '">' + escape(copy.routeSupport) + ': ' + escape(supportLabel) + '</div>' +
        '<h3>' + escape(route ? route.label : copy.overview) + '</h3>' +
        '<p>' + escape(route ? route.description : copy.overviewDescription) + '</p>' +
        '<p>' + escape(support.mode === 'unsupported' ? copy.routeUnavailable : copy.routePlaceholder) + '</p>' +
        '<small>' + escape(support.reason) + '</small>' +
        '</section>';
    }

    function summaryCard(label, value) {
      return '<div class="card"><small>' + escape(label) + '</small><strong>' + escape(value) + '</strong></div>';
    }

    function kvItem(label, value) {
      return '<div><span>' + escape(label) + '</span><strong>' + escape(value) + '</strong></div>';
    }

    function mountActiveRoute() {
      if (!state.payload) {
        return;
      }
      const host = document.getElementById('review-queue-host');
      if (!host) {
        return;
      }
      if (state.payload.activeRoute === 'promotionQueue') {
        mountPromotionQueue(host, bridge, i18n);
        return;
      }
      if (state.payload.activeRoute === 'contradictionQueue') {
        mountContradictionQueue(host, bridge, i18n);
      }
    }

    function escape(value) {
      return String(value)
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
    }

    function escapeAttr(value) {
      return escape(value);
    }

    document.addEventListener('click', (event) => {
      const target = event.target;
      if (!(target instanceof Element)) {
        return;
      }
      const localAction = target.closest('[data-local-command]');
      if (localAction) {
        const localCommand = localAction.getAttribute('data-local-command');
        if (localCommand === 'open-dialog') {
          const dialogId = localAction.getAttribute('data-dialog-id');
          const dialog = dialogId ? document.getElementById(dialogId) : undefined;
          if (dialog instanceof HTMLDialogElement) {
            dialog.showModal();
          }
          return;
        }
        if (localCommand === 'close-dialog') {
          const dialog = localAction.closest('dialog');
          if (dialog instanceof HTMLDialogElement) {
            dialog.close();
          }
          return;
        }
      }
      const action = target.closest('[data-command]');
      if (!action) {
        return;
      }
      const command = action.getAttribute('data-command');
      vscode.postMessage({
        command,
        route: action.getAttribute('data-route'),
        filterKey: action.getAttribute('data-filter-key'),
        sortKey: action.getAttribute('data-sort-key'),
        sortBy: action.getAttribute('data-sort-by'),
        memoryId: action.getAttribute('data-memory-id'),
        value: action.getAttribute('data-value'),
        eventId: action.getAttribute('data-event-id'),
        page: numberOrUndefined(action.getAttribute('data-page')),
        pageSize: numberOrUndefined(action.getAttribute('data-page-size')),
      });
    });

    document.addEventListener('submit', (event) => {
      const target = event.target;
      if (!(target instanceof HTMLFormElement)) {
        return;
      }
      const command = target.getAttribute('data-command');
      if (!command) {
        return;
      }
      event.preventDefault();
      const formData = new FormData(target);
      vscode.postMessage({
        command,
        since: stringOrUndefined(formData.get('since')),
        until: stringOrUndefined(formData.get('until')),
      });
    });

    function numberOrUndefined(value) {
      if (value === null || value === '') {
        return undefined;
      }
      const parsed = Number(value);
      return Number.isFinite(parsed) ? parsed : undefined;
    }

    function stringOrUndefined(value) {
      return typeof value === 'string' && value.trim() ? value : undefined;
    }

    window.addEventListener('message', (event) => {
      const message = event.data;
      if (message.type === 'state') {
        state.loading = false;
        state.error = '';
        state.payload = message;
        render();
        return;
      }
      if (message.type === 'rpcResponse') {
        const pending = rpcPending.get(message.requestId);
        if (!pending) {
          return;
        }
        rpcPending.delete(message.requestId);
        if (message.error) {
          pending.reject(new Error(message.error));
          return;
        }
        pending.resolve(message.result);
        return;
      }
      if (message.type === 'error') {
        state.loading = false;
        state.error = message.message || copy.error;
        render();
      }
    });

    render();
    vscode.postMessage({ command: 'ready' });
  </script>
</body>
</html>`;
    }
}

function routeIds(): ReviewRouteId[] {
    return [
        'memoryInbox',
        'promotionQueue',
        'contradictionQueue',
        'staleView',
        'evidenceInspector',
        'eventTrace',
        'retrievalExplanation',
        'usefulnessMetrics',
        'workspaceGraphHealth',
        'indexingHealth',
        'consolidationQueue',
    ];
}

function toReviewPanelMessage(message: WebviewMessage): ReviewPanelMessage {
    return {
        command: message.command,
        filterKey: message.filterKey,
        sortKey: message.sortKey,
        page: message.page,
        pageSize: message.pageSize,
        memoryId: message.memoryId,
    };
}

const STALE_STATUS_VALUES = ['stale', 'contradicted', 'superseded', 'expired', 'invalidated'] as const;
const STALE_SCOPE_VALUES = ['session', 'branch', 'repo', 'user', 'organization'] as const;
const EVENT_TRACE_KIND_VALUES = [
    'assistant_task_started',
    'tool_called',
    'tool_result',
    'context_bundle_returned',
    'memory_retrieved',
    'memory_expanded',
    'plan_created',
    'file_read',
    'patch_applied',
    'test_run_started',
    'test_run_completed',
    'diagnostic_observed',
    'user_correction',
    'user_preference_observed',
    'workflow_succeeded',
    'workflow_failed',
    'memory_created',
    'memory_updated',
    'memory_invalidated',
    'memory_consolidated',
    'consolidation_failed',
    'memory_scope_filtered',
] as const;

function isStaleSortKey(value: string): value is StaleViewState['sortBy'] {
    return ['status', 'memoryClass', 'scope', 'staleSince'].includes(value);
}

function isConsolidationSortKey(value: string): value is ConsolidationQueueViewState['sortBy'] {
    return ['status', 'kind', 'mode', 'createdAt', 'duration'].includes(value);
}

function isWorkspaceGraphPanel(
    value: string
): value is 'nodes' | 'edges' | 'brokenReferences' | 'staleEdges' | 'orphanSymbols' {
    return ['nodes', 'edges', 'brokenReferences', 'staleEdges', 'orphanSymbols'].includes(value);
}

function parseGraphFamilySort(
    value: string
): { panel: 'nodes' | 'edges'; sortBy: WorkspaceGraphHealthViewState['nodeSortBy'] } | undefined {
    const [panel, sortBy] = value.split(':');
    if ((panel === 'nodes' || panel === 'edges') && (sortBy === 'family' || sortBy === 'count')) {
        return { panel, sortBy };
    }
    return undefined;
}

function normalizeFilterSelection(
    values: string[],
    allValues: readonly string[]
): string[] {
    const normalized = values.filter((value, index) => values.indexOf(value) === index);
    return normalized.length === allValues.length ? [] : normalized;
}

function resolveReferenceUri(reference: string): vscode.Uri | undefined {
    const raw = reference.trim();
    if (!raw) {
        return undefined;
    }
    try {
        const parsed = JSON.parse(raw) as Record<string, unknown>;
        if (typeof parsed.FileRef === 'object' && parsed.FileRef !== null) {
            const value = parsed.FileRef as Record<string, unknown>;
            const path = typeof value.repo_relative_path === 'string' ? value.repo_relative_path : '';
            if (path) {
                return joinWorkspacePath(path);
            }
        }
        if (typeof parsed.SymbolRef === 'object' && parsed.SymbolRef !== null) {
            const value = parsed.SymbolRef as Record<string, unknown>;
            const file = typeof value.file === 'object' && value.file !== null
                ? value.file as Record<string, unknown>
                : undefined;
            const path = typeof file?.repo_relative_path === 'string' ? file.repo_relative_path : '';
            if (path) {
                return joinWorkspacePath(path);
            }
        }
    } catch {
        // Fall through to legacy reference parsing.
    }
    if (raw.startsWith('http://') || raw.startsWith('https://')) {
        return vscode.Uri.parse(raw);
    }
    const fileLike = raw.split('#')[0].split('::')[0];
    if (!fileLike) {
        return undefined;
    }
    if (fileLike.startsWith('/')) {
        return vscode.Uri.file(fileLike);
    }
    return joinWorkspacePath(fileLike);
}

function joinWorkspacePath(fileLike: string): vscode.Uri | undefined {
    const workspaceRoot = vscode.workspace.workspaceFolders?.[0]?.uri;
    return workspaceRoot ? vscode.Uri.joinPath(workspaceRoot, fileLike) : undefined;
}

function validateIsoWindow(
    since: string | undefined,
    until: string | undefined,
    i18n: ReviewI18n
): string | undefined {
    if (since && Number.isNaN(Date.parse(since))) {
        return i18n.t('eventTraceView.invalidSince');
    }
    if (until && Number.isNaN(Date.parse(until))) {
        return i18n.t('eventTraceView.invalidUntil');
    }
    if (since && until && Date.parse(since) > Date.parse(until)) {
        return i18n.t('eventTraceView.invalidWindow');
    }
    return undefined;
}

function uniqueEventTraceValues(values: Array<string | undefined>): string[] {
    return [...new Set(values.filter((value): value is string => typeof value === 'string' && value.length > 0))]
        .sort((left, right) => left.localeCompare(right));
}

function formatActorLabel(actor: string, i18n: ReviewI18n): string {
    const base = actor.split(':')[0];
    const detail = actor.includes(':') ? actor.slice(actor.indexOf(':') + 1) : '';
    const label = i18n.has(`eventTraceView.actor.${base}`) ? i18n.t(`eventTraceView.actor.${base}`) : actor;
    return detail ? `${label} (${detail})` : label;
}

async function pickSingleValue(
    values: string[],
    title: string,
    placeholder: string,
    allLabel: string
): Promise<string | undefined> {
    const picks: FilterPickItem[] = [
        { label: allLabel, value: '' },
        ...values.map((value) => ({ label: value, value })),
    ];
    const selection = await vscode.window.showQuickPick(picks, {
        title,
        placeHolder: placeholder,
        ignoreFocusOut: true,
    });
    return selection?.value;
}

function normalizeConsolidationMode(
    mode: string
): 'background' | 'manual_review' | 'replay' | 'post_task' {
    if (mode === 'background' || mode === 'manual_review' || mode === 'replay' || mode === 'post_task') {
        return mode;
    }
    return 'manual_review';
}

function extractMemoryId(reference: string): string | undefined {
    try {
        const parsed = JSON.parse(reference) as Record<string, unknown>;
        const payload = parsed.MemoryRef;
        if (typeof payload !== 'object' || payload === null || Array.isArray(payload)) {
            return undefined;
        }
        const record = payload as Record<string, unknown>;
        return typeof record.ulid === 'string' ? record.ulid : undefined;
    } catch {
        const memoryMatch = reference.match(/memory[:/](?<id>[A-Za-z0-9_-]+)/);
        if (memoryMatch?.groups?.id) {
            return memoryMatch.groups.id;
        }
        const segment = reference.split('/').pop();
        return segment && /^mem[-_]/.test(segment) ? segment : undefined;
    }
}

function extractRetrievalRequestId(
    event: { payload?: unknown; references?: string[]; eventId?: string } | undefined
): string | undefined {
    if (!event) {
        return undefined;
    }
    if (typeof event.payload === 'object' && event.payload !== null) {
        const record = event.payload as Record<string, unknown>;
        for (const key of ['request_id', 'requestId', 'call_id', 'callId']) {
            const value = record[key];
            if (typeof value === 'string' && value.length > 0) {
                return value;
            }
        }
    }
    return event.references?.find((reference) => reference.includes('request'))
        ?? event.eventId;
}

function stringParam(params: Record<string, unknown>, key: string): string {
    const value = params[key];
    return typeof value === 'string' ? value : '';
}

function optionalStringParam(params: Record<string, unknown>, key: string): string | undefined {
    const value = params[key];
    return typeof value === 'string' && value.trim().length > 0 ? value : undefined;
}

function contradictionParams(
    params: Record<string, unknown>,
    requireReason = false
): ReviewContradictionDecisionArgs & { reason?: string } {
    const args: ReviewContradictionDecisionArgs & { reason?: string } = {
        proposalId: optionalStringParam(params, 'proposalId'),
        sourceMemoryId: stringParam(params, 'sourceMemoryId'),
        targetMemoryId: stringParam(params, 'targetMemoryId'),
        linkType: stringParam(params, 'linkType'),
        detectedBy: optionalStringParam(params, 'detectedBy'),
        reason: optionalStringParam(params, 'reason'),
    };
    if (requireReason && !args.reason) {
        args.reason = '';
    }
    return args;
}

function escapeHtml(value: string): string {
    return value
        .replaceAll('&', '&amp;')
        .replaceAll('<', '&lt;')
        .replaceAll('>', '&gt;')
        .replaceAll('"', '&quot;')
        .replaceAll("'", '&#39;');
}
