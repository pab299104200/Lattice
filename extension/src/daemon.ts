import * as vscode from 'vscode';
import * as cp from 'child_process';
import * as crypto from 'crypto';
import * as path from 'path';
import * as fs from 'fs';

export type DaemonStatus = 'starting' | 'running' | 'stopped' | 'error';

interface JsonRpcRequest {
    jsonrpc: '2.0';
    id: number;
    method: string;
    params?: unknown;
}

interface JsonRpcResponse {
    jsonrpc: '2.0';
    id: number;
    result?: unknown;
    error?: { code: number; message: string; data?: unknown };
}

interface PendingRequest {
    resolve: (value: unknown) => void;
    reject: (reason: Error) => void;
    timer: ReturnType<typeof setTimeout>;
}

export class DaemonManager implements vscode.Disposable {
    private process: cp.ChildProcess | null = null;
    private nextId = 1;
    private pendingRequests = new Map<number, PendingRequest>();
    private buffer = '';
    private status: DaemonStatus = 'stopped';
    private restartCount = 0;
    private maxRestarts = 5;
    private restartTimer: ReturnType<typeof setTimeout> | null = null;
    private disposed = false;
    private requestTimeoutMs = 30_000;

    private readonly _onStatusChange = new vscode.EventEmitter<DaemonStatus>();
    public readonly onStatusChange = this._onStatusChange.event;

    constructor(private readonly extensionPath: string) {}

    private setStatus(status: DaemonStatus): void {
        if (this.status !== status) {
            this.status = status;
            this._onStatusChange.fire(status);
        }
    }

    public getStatus(): DaemonStatus {
        return this.status;
    }

    /**
     * Resolve the daemon binary path.
     * Search order: extension/bin/, PATH, ../daemon/target/debug/
     */
    private resolveBinaryPath(): string | null {
        const binaryName = process.platform === 'win32' ? 'lattice.exe' : 'lattice';

        // 1. Check extension/bin/
        const extensionBin = path.join(this.extensionPath, 'bin', binaryName);
        if (fs.existsSync(extensionBin)) {
            return extensionBin;
        }

        // 2. Check PATH via which/where
        try {
            const cmd = process.platform === 'win32' ? 'where' : 'which';
            const result = cp.execSync(`${cmd} ${binaryName}`, { encoding: 'utf-8', timeout: 5000 });
            const found = result.trim().split(/\r?\n/)[0];
            if (found && fs.existsSync(found)) {
                return found;
            }
        } catch {
            // not found on PATH
        }

        // 3. Check ../daemon/target/debug/ and target_new/debug/
        for (const targetDir of ['target', 'target_new']) {
            const debugBin = path.join(this.extensionPath, '..', 'daemon', targetDir, 'debug', binaryName);
            if (fs.existsSync(debugBin)) {
                return debugBin;
            }
        }

        // 4. Check D:\lattice\ (deployed location)
        const deployedBin = path.join('D:', 'lattice', binaryName);
        if (fs.existsSync(deployedBin)) {
            return deployedBin;
        }

        return null;
    }

    /**
     * Verify the daemon binary's SHA-256 hash before spawning.
     * Looks for a .sha256 file alongside the binary.
     * Returns true if verification passes or is skipped (dev mode).
     */
    private verifyBinary(binaryPath: string): boolean {
        const hashFile = binaryPath + '.sha256';
        if (!fs.existsSync(hashFile)) {
            // No hash file = development mode, skip verification
            console.log('Lattice: No .sha256 file found, skipping binary verification (dev mode)');
            return true;
        }

        try {
            const expectedHash = fs.readFileSync(hashFile, 'utf8').trim().split(/\s+/)[0];
            const binaryContent = fs.readFileSync(binaryPath);
            const actualHash = crypto.createHash('sha256').update(binaryContent).digest('hex');

            if (actualHash !== expectedHash) {
                vscode.window.showErrorMessage(
                    `Lattice: Binary verification failed!\nExpected: ${expectedHash}\nActual: ${actualHash}`
                );
                return false;
            }

            console.log('Lattice: Binary SHA-256 verified');
            return true;
        } catch (e) {
            console.error('Lattice: Binary verification error:', e);
            return true; // Allow startup on verification errors (file read issues etc.)
        }
    }

    /**
     * Start the daemon process.
     */
    public async start(): Promise<void> {
        if (this.process) {
            return;
        }

        this.setStatus('starting');

        const binaryPath = this.resolveBinaryPath();
        if (!binaryPath) {
            this.setStatus('error');
            throw new Error(
                'Lattice daemon binary not found. Searched: extension/bin/, PATH, ../daemon/target/debug/'
            );
        }

        if (!this.verifyBinary(binaryPath)) {
            this.setStatus('error');
            return;
        }

        return this.spawn(binaryPath);
    }

    private spawn(binaryPath: string): Promise<void> {
        return new Promise<void>((resolve, reject) => {
            if (this.disposed) {
                reject(new Error('DaemonManager disposed'));
                return;
            }

            const workspaceFolders = vscode.workspace.workspaceFolders;
            const cwd = workspaceFolders?.[0]?.uri.fsPath ?? this.extensionPath;

            // Build args: --stdio plus --workspace for each workspace folder
            const args = ['--stdio'];
            if (workspaceFolders) {
                for (const folder of workspaceFolders) {
                    args.push('--workspace', folder.uri.fsPath);
                }
            }

            // Ensure MinGW DLLs are findable if built with GNU toolchain
            const env = { ...process.env };
            const mingwPaths = ['D:\\mingw64\\bin', 'C:\\mingw64\\bin'];
            for (const p of mingwPaths) {
                if (fs.existsSync(p) && !env.PATH?.includes(p)) {
                    env.PATH = p + ';' + (env.PATH ?? '');
                }
            }
            // Also ensure cargo bin is on PATH
            const cargoBin = path.join(process.env.USERPROFILE ?? '', '.cargo', 'bin');
            if (fs.existsSync(cargoBin) && !env.PATH?.includes(cargoBin)) {
                env.PATH = cargoBin + ';' + (env.PATH ?? '');
            }

            this.process = cp.spawn(binaryPath, args, {
                cwd,
                stdio: ['pipe', 'pipe', 'pipe'],
                env,
            });

            let started = false;

            this.process.stdout?.on('data', (data: Buffer) => {
                this.onData(data.toString('utf-8'));
                if (!started) {
                    started = true;
                    this.setStatus('running');
                    this.restartCount = 0;
                    resolve();
                }
            });

            this.process.stderr?.on('data', (data: Buffer) => {
                console.error(`[lattice-daemon stderr] ${data.toString('utf-8')}`);
            });

            this.process.on('error', (err: Error) => {
                console.error(`[lattice-daemon] process error: ${err.message}`);
                this.setStatus('error');
                if (!started) {
                    started = true;
                    reject(err);
                }
                this.handleExit();
            });

            this.process.on('exit', (code: number | null, signal: string | null) => {
                console.log(`[lattice-daemon] exited code=${code} signal=${signal}`);
                if (!started) {
                    started = true;
                    this.setStatus('error');
                    reject(new Error(`Daemon exited before starting (code=${code})`));
                }
                this.handleExit();
            });

            // If we don't get stdout within 10s, consider it running anyway
            setTimeout(() => {
                if (!started) {
                    started = true;
                    this.setStatus('running');
                    this.restartCount = 0;
                    resolve();
                }
            }, 10_000);
        });
    }

    private handleExit(): void {
        this.process = null;

        // Reject all pending requests
        for (const [id, pending] of this.pendingRequests) {
            clearTimeout(pending.timer);
            pending.reject(new Error('Daemon process exited'));
            this.pendingRequests.delete(id);
        }

        this.buffer = '';

        if (this.disposed) {
            this.setStatus('stopped');
            return;
        }

        // Auto-restart with exponential backoff
        if (this.restartCount < this.maxRestarts) {
            const delay = Math.min(1000 * Math.pow(2, this.restartCount), 30_000);
            this.restartCount++;
            console.log(`[lattice-daemon] auto-restart attempt ${this.restartCount}/${this.maxRestarts} in ${delay}ms`);
            this.setStatus('starting');
            this.restartTimer = setTimeout(() => {
                this.restartTimer = null;
                const binaryPath = this.resolveBinaryPath();
                if (binaryPath) {
                    this.spawn(binaryPath).catch((err) => {
                        console.error(`[lattice-daemon] restart failed: ${err.message}`);
                        this.setStatus('error');
                    });
                } else {
                    this.setStatus('error');
                }
            }, delay);
        } else {
            console.error(`[lattice-daemon] max restart attempts (${this.maxRestarts}) reached`);
            this.setStatus('error');
        }
    }

    /**
     * Handle incoming data from stdout with Content-Length framing.
     */
    private onData(chunk: string): void {
        this.buffer += chunk;
        this.parseBuffer();
    }

    private parseBuffer(): void {
        while (true) {
            // Try newline-delimited JSON first (each line is a complete JSON message)
            const newlineIdx = this.buffer.indexOf('\n');
            if (newlineIdx !== -1) {
                const line = this.buffer.substring(0, newlineIdx).trim();
                this.buffer = this.buffer.substring(newlineIdx + 1);

                if (!line) { continue; } // skip empty lines

                // Skip Content-Length headers (daemon may still send them)
                if (line.match(/^Content-Length:/i)) { continue; }

                if (line.startsWith('{')) {
                    try {
                        const message = JSON.parse(line) as JsonRpcResponse;
                        this.handleMessage(message);
                    } catch (err) {
                        console.error(`[lattice-daemon] failed to parse JSON line: ${err}`);
                    }
                }
                continue;
            }

            // No newline yet — check if we have a partial JSON object without newline
            // (shouldn't happen with our daemon, but handle gracefully)
            return;
        }
    }

    private handleMessage(message: JsonRpcResponse): void {
        if (message.id === undefined || message.id === null) {
            // Notification from daemon — ignore for now
            return;
        }

        const pending = this.pendingRequests.get(message.id);
        if (!pending) {
            console.warn(`[lattice-daemon] received response for unknown request id=${message.id}`);
            return;
        }

        this.pendingRequests.delete(message.id);
        clearTimeout(pending.timer);

        if (message.error) {
            pending.reject(new Error(`JSON-RPC error ${message.error.code}: ${message.error.message}`));
        } else {
            pending.resolve(message.result);
        }
    }

    /**
     * Send a JSON-RPC request to the daemon.
     * Returns a Promise that resolves with the result.
     */
    public sendRequest(method: string, params?: unknown, timeoutMs?: number): Promise<unknown> {
        return new Promise((resolve, reject) => {
            if (!this.process || !this.process.stdin) {
                reject(new Error('Daemon is not running'));
                return;
            }

            const id = this.nextId++;
            const request: JsonRpcRequest = {
                jsonrpc: '2.0',
                id,
                method,
                ...(params !== undefined ? { params } : {}),
            };

            const body = JSON.stringify(request);
            const message = body + '\n';

            const effectiveTimeout = timeoutMs ?? this.requestTimeoutMs;
            const timer = setTimeout(() => {
                this.pendingRequests.delete(id);
                reject(new Error(`Request ${method} (id=${id}) timed out after ${effectiveTimeout}ms`));
            }, effectiveTimeout);

            this.pendingRequests.set(id, { resolve, reject, timer });

            try {
                this.process.stdin.write(message, 'utf-8', (err) => {
                    if (err) {
                        this.pendingRequests.delete(id);
                        clearTimeout(timer);
                        reject(new Error(`Failed to write to daemon stdin: ${err.message}`));
                    }
                });
            } catch (err) {
                this.pendingRequests.delete(id);
                clearTimeout(timer);
                reject(err instanceof Error ? err : new Error(String(err)));
            }
        });
    }

    /**
     * Stop the daemon process.
     */
    public stop(): void {
        if (this.restartTimer) {
            clearTimeout(this.restartTimer);
            this.restartTimer = null;
        }

        // Prevent auto-restart
        this.restartCount = this.maxRestarts;

        if (this.process) {
            try {
                this.process.kill('SIGTERM');
            } catch {
                // Process may already be dead
            }
            this.process = null;
        }

        // Reject all pending requests
        for (const [id, pending] of this.pendingRequests) {
            clearTimeout(pending.timer);
            pending.reject(new Error('Daemon stopped'));
            this.pendingRequests.delete(id);
        }

        this.buffer = '';
        this.setStatus('stopped');
    }

    /**
     * Dispose and clean up all resources.
     */
    public dispose(): void {
        this.disposed = true;
        this.stop();
        this._onStatusChange.dispose();
    }
}
