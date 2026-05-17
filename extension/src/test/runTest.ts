import * as cp from 'child_process';
import * as path from 'path';
import { runTests } from '@vscode/test-electron';

async function main(): Promise<void> {
    await ensureDisplayServer();

    const extensionPath = path.resolve(__dirname, '..', '..');
    const workspacePath = path.resolve(extensionPath, '..');
    const testsPath = path.resolve(__dirname, 'suite', 'index.js');

    await runTests({
        vscodeExecutablePath: process.env.VSCODE_EXECUTABLE_PATH ?? '/usr/bin/code',
        extensionDevelopmentPath: extensionPath,
        extensionTestsPath: testsPath,
        extensionTestsEnv: {
            LATTICE_EXTENSION_TEST: '1',
        },
        launchArgs: [
            workspacePath,
            '--disable-workspace-trust',
            '--skip-welcome',
            '--disable-updates',
            '--disable-telemetry',
            '--new-window',
        ],
    });
}

async function ensureDisplayServer(): Promise<void> {
    if (process.env.DISPLAY || process.env.LATTICE_XVFB_RUN === '1') {
        return;
    }

    await new Promise<void>((resolve, reject) => {
        const child = cp.spawn('xvfb-run', ['-a', process.execPath, __filename], {
            env: {
                ...process.env,
                LATTICE_XVFB_RUN: '1',
            },
            stdio: 'inherit',
        });
        child.once('error', reject);
        child.once('exit', (code) => {
            if (code === 0) {
                resolve();
                return;
            }
            reject(new Error(`xvfb-run exited with code ${code ?? 1}`));
        });
    });

    process.exit(0);
}

void main().catch((error) => {
    console.error(error instanceof Error ? error.stack ?? error.message : String(error));
    process.exit(1);
});
