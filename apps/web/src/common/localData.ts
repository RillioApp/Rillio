// Export / import the whole local account as one compact code. Everything the
// core persists lives in localStorage under the ACTIVE profile's namespace
// (crates/core/src/constants.rs keys via common/profileStorage), so a backup is
// just those keys (plus our local display name) bundled, gzip'd, and base64'd.
// Import writes them back and the caller reloads so the core re-reads.
import { gzipSync, gunzipSync, strToU8, strFromU8 } from 'fflate';
import { getItem, setItem, removeItem } from 'rillio/common/profileStorage';

// Binary <-> base64 without Buffer (chunked so large arrays don't blow the stack).
const bytesToBase64 = (bytes: Uint8Array): string => {
    let bin = '';
    const chunk = 0x8000;
    for (let i = 0; i < bytes.length; i += chunk) {
        bin += String.fromCharCode.apply(null, Array.from(bytes.subarray(i, i + chunk)));
    }
    return btoa(bin);
};
const base64ToBytes = (b64: string): Uint8Array => {
    const bin = atob(b64);
    const out = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
    return out;
};

// Buckets the Rust core reads on boot. `schema_version` is included so an import
// stays internally consistent with the buckets it shipped with. (Codes exported
// by older versions also carry `dismissed_events`, the retired Stremio-promo
// dismissals bucket; the core no longer reads it, so import skips it.)
const CORE_KEYS = [
    'profile',
    'library',
    'library_recent',
    'streams',
    'search_history',
    'streaming_server_urls',
    'notifications',
    'calendar',
    'schema_version',
];
const NAME_KEY = 'rillio.displayName';
const ALL_KEYS = [...CORE_KEYS, NAME_KEY];

// Encode the local account into a paste/scan-friendly base64 string. The profile
// bucket is embedded with `auth` nulled (same shape the post-Stremio-import
// anonymize step rewrites): a code exported right after a Stremio import must
// never carry a live Stremio token. Local accounts have null auth anyway, so the
// restored account works the same.
export const exportLocalData = (): string => {
    const data: Record<string, string> = {};
    for (const k of ALL_KEYS) {
        const v = getItem(k);
        if (v !== null) data[k] = v;
    }
    if (typeof data['profile'] === 'string') {
        let profile: unknown;
        try {
            profile = JSON.parse(data['profile']);
        } catch {
            throw new Error('The profile data on this device is not valid JSON, cannot export.');
        }
        if (profile !== null && typeof profile === 'object' && 'auth' in profile) {
            (profile as { auth: unknown }).auth = null;
            data['profile'] = JSON.stringify(profile);
        }
    }
    const json = JSON.stringify({ v: 1, data });
    const gz = gzipSync(strToU8(json), { level: 9 });
    return bytesToBase64(gz);
};

// Decode a code and write the buckets back. Only known keys are written; anything
// else in the payload is ignored. The import is all-or-nothing: every bucket is
// validated up front, the current buckets are backed up before the first write,
// and any write failure rolls everything back. Caller should reload the page on
// success.
export const importLocalData = (code: string): { ok: boolean; error?: string; keys?: number } => {
    const trimmed = (code || '').trim();
    if (!trimmed) return { ok: false, error: 'Paste a sync code first.' };
    let parsed: any;
    try {
        parsed = JSON.parse(strFromU8(gunzipSync(base64ToBytes(trimmed))));
    } catch {
        return { ok: false, error: 'Could not read that code, it may be truncated or corrupted.' };
    }
    if (!parsed || parsed.v !== 1 || typeof parsed.data !== 'object' || parsed.data === null) {
        return { ok: false, error: 'This does not look like a Rillio sync code.' };
    }
    // Validate every incoming bucket before touching storage. Core buckets must be
    // parseable JSON (the Rust core JSON-parses them on boot); the display name is
    // a plain string.
    const incoming: Record<string, string> = {};
    for (const k of CORE_KEYS) {
        const v = parsed.data[k];
        if (typeof v !== 'string') continue;
        try {
            JSON.parse(v);
        } catch {
            return { ok: false, error: `The "${k}" data in this code is not valid JSON, refusing to import.` };
        }
        incoming[k] = v;
    }
    if (typeof parsed.data[NAME_KEY] === 'string') incoming[NAME_KEY] = parsed.data[NAME_KEY];
    const keys = Object.keys(incoming);
    if (keys.length === 0) return { ok: false, error: 'The code contained no account data.' };
    // Reject codes exported by a newer core schema. The local `schema_version`
    // bucket is written by this app's core from SCHEMA_VERSION in
    // crates/core/src/constants.rs; the core migrates older buckets forward on
    // boot but refuses newer ones, which would brick the app after the reload.
    if (typeof incoming['schema_version'] === 'string') {
        const incomingSchema = Number(incoming['schema_version']);
        if (!Number.isFinite(incomingSchema)) {
            return { ok: false, error: 'The schema version in this code is unreadable, refusing to import.' };
        }
        const localRaw = getItem('schema_version');
        const localSchema = localRaw === null ? NaN : Number(localRaw);
        if (!Number.isFinite(localSchema)) {
            return { ok: false, error: 'Could not read this device\'s schema version to check compatibility, refusing to import.' };
        }
        if (incomingSchema > localSchema) {
            return { ok: false, error: 'This code was exported by a newer version of Rillio. Update this app, then restore.' };
        }
    }
    // All-or-nothing write: back up every bucket about to be overwritten, and on
    // any failure restore all of them. A failed write is a failed import, never a
    // partial success.
    const backup: Record<string, string | null> = {};
    for (const k of keys) backup[k] = getItem(k);
    try {
        for (const k of keys) setItem(k, incoming[k]);
    } catch (e) {
        const rollbackFailures: string[] = [];
        for (const k of keys) {
            try {
                if (backup[k] === null) removeItem(k);
                else setItem(k, backup[k] as string);
            } catch {
                rollbackFailures.push(k);
            }
        }
        const reason = e instanceof Error ? e.message : String(e);
        const restored = rollbackFailures.length === 0 ?
            'Your previous data was restored.' :
            `Restoring your previous data also failed for: ${rollbackFailures.join(', ')}.`;
        return { ok: false, error: `Import failed while writing (${reason}). ${restored}` };
    }
    return { ok: true, keys: keys.length };
};

// (anonymizeBucket is gone: the one-time-import flow that needed it was replaced
// by the persistent connection - the core's Ctx.Disconnect now does the
// keep-data owner retag natively, in every bucket.)
