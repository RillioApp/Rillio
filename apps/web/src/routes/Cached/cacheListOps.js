// The Cache page's sort / filter / search decisions, pure and dependency-free
// so the jest suite can require them directly (this repo's jest has no TS
// transform; same pattern as common/storageGuardAssess). Quality and display
// names are injected as accessors: parsing lives in streamQuality (TS) and the
// naming rule in Cached.tsx, and duplicating either here would drift.

// Sort orders the toolbar offers. 'date' is the default: the cache is a
// timeline of what you grabbed, and "what did I just add" is the question the
// page usually answers.
const SORTS = ['date', 'size', 'quality'];

// entries: CacheEntry[]; sort: one of SORTS; getQuality(entry) ->
// { resolution: number, hdr: boolean }. Returns a NEW array (the poll swaps
// entry arrays every 3s; sorting in place would mutate React state).
//
// Ties (and entries missing the sort key) fall back to name order so the list
// is stable across polls instead of flickering by server whim.
//
// 'date' is "Recent": newest activity first. getRecency(entry) -> epoch ms
// (the Cache window passes the later of added and last played, see
// cacheRowModel.recencyOf); without it, the added time.
const sortEntries = (entries, sort, getQuality, getRecency) => {
    const recency = typeof getRecency === 'function' ? getRecency : (entry) => entry.addedAt;
    const byName = (a, b) => (a.name || '').localeCompare(b.name || '');
    const sorted = [...entries];
    switch (sort) {
        case 'size':
            sorted.sort((a, b) => (b.downloaded - a.downloaded) || byName(a, b));
            break;
        case 'quality':
            sorted.sort((a, b) => {
                const qa = getQuality(a);
                const qb = getQuality(b);
                // Resolution first; HDR breaks ties (a 4K HDR file outranks a
                // plain 4K one); size last so equal-quality entries still order
                // meaningfully.
                return (qb.resolution - qa.resolution) ||
                    (Number(qb.hdr) - Number(qa.hdr)) ||
                    (b.downloaded - a.downloaded) ||
                    byName(a, b);
            });
            break;
        case 'date':
        default:
            sorted.sort((a, b) => ((recency(b) || 0) - (recency(a) || 0)) || byName(a, b));
            break;
    }
    return sorted;
};

// Filter facets the toolbar offers as toggle chips. Active facets combine with
// AND: "4K + Kept" means kept 4K downloads, which is how people narrow a list.
const FILTERS = ['4k', '1080p', 'hdr', 'downloading', 'complete', 'kept'];

const isComplete = (entry) => entry.total > 0 && entry.downloaded >= entry.total;

const matchesFilter = (entry, filter, getQuality) => {
    switch (filter) {
        case '4k': return getQuality(entry).resolution >= 2160;
        case '1080p': return getQuality(entry).resolution === 1080;
        case 'hdr': return getQuality(entry).hdr === true;
        // "Downloading" means an active transfer of any flavor - live, paused
        // mid-way, or still initializing - i.e. NOT done and NOT failed.
        case 'downloading': return !isComplete(entry) && entry.state !== 'error';
        case 'complete': return isComplete(entry);
        case 'kept': return entry.pinned === true;
        default: return true;
    }
};

// entries -> the ones matching EVERY active filter and the search text.
// query matches case-insensitively against the display name (the matched
// title) AND the raw entry name (the scene filename), because users remember
// either ("silo" or "2160p").
const filterEntries = (entries, activeFilters, query, getQuality, getDisplayName) => {
    const needle = (query || '').trim().toLowerCase();
    return entries.filter((entry) => {
        if (!activeFilters.every((filter) => matchesFilter(entry, filter, getQuality))) {
            return false;
        }
        if (needle.length === 0) return true;
        return getDisplayName(entry).toLowerCase().includes(needle) ||
            (entry.name || '').toLowerCase().includes(needle);
    });
};

module.exports = { SORTS, FILTERS, sortEntries, filterEntries };
