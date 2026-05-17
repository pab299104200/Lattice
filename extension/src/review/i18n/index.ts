import * as fs from 'fs';
import * as path from 'path';

type CatalogValue = string | CatalogTree;

export interface CatalogTree {
    [key: string]: CatalogValue;
}

export interface ReviewI18n {
    t: (key: string, params?: Record<string, string | number>) => string;
    has: (key: string) => boolean;
}

const FALLBACK_CATALOG: CatalogTree = {
    reviewPanel: {
        title: 'Lattice Review',
        loading: 'Loading review state...',
        error: 'Error loading review data.',
        retry: 'Retry',
    },
};

function catalogCandidates(): string[] {
    return [
        path.join(__dirname, 'en.json'),
        path.join(__dirname, '../../../src/review/i18n/en.json'),
        path.join(process.cwd(), 'src/review/i18n/en.json'),
    ];
}

function readCatalog(): CatalogTree {
    for (const candidate of catalogCandidates()) {
        try {
            if (!fs.existsSync(candidate)) {
                continue;
            }
            const content = fs.readFileSync(candidate, 'utf8');
            return JSON.parse(content) as CatalogTree;
        } catch {
            continue;
        }
    }
    return FALLBACK_CATALOG;
}

export function loadReviewCatalog(): CatalogTree {
    return readCatalog();
}

function lookup(tree: CatalogTree, key: string): string | undefined {
    const parts = key.split('.');
    let current: CatalogValue | undefined = tree;
    for (const part of parts) {
        if (!current || typeof current === 'string') {
            return undefined;
        }
        current = current[part];
    }
    return typeof current === 'string' ? current : undefined;
}

function interpolate(template: string, params?: Record<string, string | number>): string {
    if (!params) {
        return template;
    }
    return template.replace(/\{([^}]+)\}/g, (_match, name: string) => {
        const value = params[name];
        return value === undefined ? `{${name}}` : String(value);
    });
}

export function createReviewI18n(): ReviewI18n {
    const catalog = readCatalog();
    return {
        t(key, params) {
            const template = lookup(catalog, key) ?? lookup(FALLBACK_CATALOG, key) ?? key;
            return interpolate(template, params);
        },
        has(key) {
            return lookup(catalog, key) !== undefined || lookup(FALLBACK_CATALOG, key) !== undefined;
        },
    };
}
