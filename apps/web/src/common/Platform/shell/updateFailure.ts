// A failed update, as the desktop shell reports it (error_chain::UpdateFailure
// in apps/desktop/src-tauri/src/error_chain.rs): the `check_for_update` and
// `install_update` commands reject with this object. The shell classifies the
// failure and writes the words (one table, shared with the update window), so
// the web layer only lays them out: `summary` says what happened, `hint` what
// to check, and "Copy error" copies `report` (version, time, step, kind and the
// full cause chain). Nothing technical is shown on screen.

export type UpdateFailure = {
    stage: string,
    kind: string,
    summary: string,
    hint: string,
    message: string,
    report: string,
};

// A rejection that is not the shell's shape (an IPC-level failure, or an older
// shell) has no words to lead with: show its own text as-is rather than invent
// a cause, and copy that same text.
export const toUpdateFailure = (error: unknown): UpdateFailure => {
    const e = error as Partial<UpdateFailure> | null;
    if (e && typeof e === 'object' && typeof e.summary === 'string' && typeof e.message === 'string') {
        return {
            stage: typeof e.stage === 'string' ? e.stage : 'unknown',
            kind: typeof e.kind === 'string' ? e.kind : 'other',
            summary: e.summary,
            hint: typeof e.hint === 'string' ? e.hint : '',
            message: e.message,
            report: typeof e.report === 'string' ? e.report : e.message,
        };
    }
    const text = error instanceof Error ? error.message : String(error);
    return { stage: 'unknown', kind: 'other', summary: text, hint: '', message: text, report: text };
};
