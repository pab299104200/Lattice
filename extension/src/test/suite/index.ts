import * as fs from 'fs';
import * as path from 'path';
import Mocha from 'mocha';

export async function run(): Promise<void> {
    const mocha = new Mocha({
        ui: 'tdd',
        color: true,
        timeout: 30_000,
    });
    const testsRoot = path.resolve(__dirname, '..');

    for (const file of collectTestFiles(testsRoot)) {
        mocha.addFile(file);
    }

    await new Promise<void>((resolve, reject) => {
        mocha.run((failures) => {
            if (failures > 0) {
                reject(new Error(`${failures} extension smoke tests failed.`));
                return;
            }
            resolve();
        });
    });
}

function collectTestFiles(root: string): string[] {
    const files: string[] = [];
    for (const entry of fs.readdirSync(root, { withFileTypes: true })) {
        const absolute = path.join(root, entry.name);
        if (entry.isDirectory()) {
            if (entry.name !== 'suite') {
                files.push(...collectTestFiles(absolute));
            }
            continue;
        }
        if (entry.name.endsWith('.test.js')) {
            files.push(absolute);
        }
    }
    return files.sort();
}
