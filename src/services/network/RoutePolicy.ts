import { StorageAPI } from '@/storage';
import {
    MINTCAT_API_ORIGINS,
    getMintcatApiOrigin,
    mintcatProxyUrl,
    normalizeMintcatApiOrigin,
} from '@/apis/mintcat/urls';
import type {
    MintcatProxyMode,
    NetworkProxyPolicy,
    NetworkResolvedRoute,
    NetworkRoutePlan,
} from './RequestTypes';

let resolvedMintcatProxyMode: MintcatProxyMode = 'disabled';
let hasResolvedMintcatProxyMode = false;

/**
 * Upstream defaults to 'auto': when a mod.io request or download fails or takes over 5 s, it is
 * retried through the MintCat relay (api.mintcat.work/proxy/<url>, mod.io token included), which
 * helps users in mainland China. This fork defaults to 'disabled' so requests always go straight to
 * mod.io; 'auto' / 'enabled' can still be chosen in Network Settings.
 */
export function normalizeMintcatProxyMode(value?: string | null): MintcatProxyMode {
    if (value === 'enabled' || value === 'auto') {
        return value;
    }
    return 'disabled';
}

export function setMintcatProxyModeResolved(mode?: MintcatProxyMode | string | null): void {
    resolvedMintcatProxyMode = normalizeMintcatProxyMode(mode);
    hasResolvedMintcatProxyMode = true;
}

export function getMintcatProxyModeResolved(): MintcatProxyMode {
    return resolvedMintcatProxyMode;
}

function safeParseUrl(url: string): URL | null {
    try {
        return new URL(url);
    } catch {
        return null;
    }
}

export class RoutePolicy {
    private buildDirectRoute(url: string): NetworkResolvedRoute {
        return {
            originalUrl: url,
            resolvedUrl: url,
            kind: 'direct',
            proxyMode: null,
        };
    }

    private buildProxyRoute(url: string, proxyMode: MintcatProxyMode): NetworkResolvedRoute {
        return {
            originalUrl: url,
            resolvedUrl: mintcatProxyUrl(url),
            kind: 'mintcat_proxy',
            proxyMode,
        };
    }

    private isHttpUrl(url: string): boolean {
        const parsed = safeParseUrl(url);
        return parsed?.protocol === 'http:' || parsed?.protocol === 'https:';
    }

    private isMintcatApiUrl(url: string): boolean {
        const parsed = safeParseUrl(url);
        if (!parsed) {
            return false;
        }

        const currentOrigin = normalizeMintcatApiOrigin(getMintcatApiOrigin());
        const knownOrigins = new Set<string>([
            currentOrigin,
            ...MINTCAT_API_ORIGINS.map((row) => normalizeMintcatApiOrigin(row.origin)),
        ]);
        return knownOrigins.has(normalizeMintcatApiOrigin(parsed.origin));
    }

    private isAlreadyMintcatProxyUrl(url: string): boolean {
        const parsed = safeParseUrl(url);
        if (!parsed) {
            return false;
        }

        const origin = normalizeMintcatApiOrigin(parsed.origin);
        const knownOrigins = new Set<string>([
            normalizeMintcatApiOrigin(getMintcatApiOrigin()),
            ...MINTCAT_API_ORIGINS.map((row) => normalizeMintcatApiOrigin(row.origin)),
        ]);

        return knownOrigins.has(origin) && parsed.pathname.startsWith('/proxy/');
    }

    private canUseMintcatProxy(url: string): boolean {
        if (!this.isHttpUrl(url)) {
            return false;
        }
        if (this.isAlreadyMintcatProxyUrl(url)) {
            return false;
        }
        if (this.isMintcatApiUrl(url)) {
            return false;
        }
        return true;
    }

    public async getMintcatProxyMode(): Promise<MintcatProxyMode> {
        if (hasResolvedMintcatProxyMode) {
            return resolvedMintcatProxyMode;
        }

        try {
            const settings = await StorageAPI.getSettings();
            const mode = normalizeMintcatProxyMode(await settings.getMintcatProxyMode());
            setMintcatProxyModeResolved(mode);
            return mode;
        } catch {
            return resolvedMintcatProxyMode;
        }
    }

    public resolveUrlWithMode(
        url: string,
        proxyPolicy: NetworkProxyPolicy = 'direct',
        proxyMode: MintcatProxyMode = getMintcatProxyModeResolved(),
    ): NetworkRoutePlan {
        // 'disabled' also overrides forceMintcatProxy, so nothing goes through the relay.
        if (proxyPolicy === 'direct' || proxyMode === 'disabled') {
            return { primary: this.buildDirectRoute(url) };
        }

        if (!this.canUseMintcatProxy(url)) {
            return { primary: this.buildDirectRoute(url) };
        }

        if (proxyPolicy === 'forceMintcatProxy') {
            return { primary: this.buildProxyRoute(url, proxyMode) };
        }

        if (proxyMode === 'enabled') {
            return {
                primary: this.buildProxyRoute(url, proxyMode),
                fallback: this.buildDirectRoute(url),
            };
        }

        return {
            primary: this.buildDirectRoute(url),
            fallback: this.buildProxyRoute(url, proxyMode),
        };
    }

    public async resolveUrl(
        url: string,
        options?: { proxyPolicy?: NetworkProxyPolicy },
    ): Promise<NetworkRoutePlan> {
        const proxyMode = await this.getMintcatProxyMode();
        return this.resolveUrlWithMode(url, options?.proxyPolicy ?? 'direct', proxyMode);
    }
}

