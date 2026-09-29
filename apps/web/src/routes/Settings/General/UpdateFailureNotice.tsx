// Copyright (C) 2017-2026 Smart code 203358507

// The failed state under Settings > Updates, the same treatment as the update
// window's (apps/web/src/update.html): what happened and what to check in
// plain words, then "Try again" (the one accent) and "Copy error" (tonal; the
// whole technical report goes to the clipboard, nothing technical on screen).
// The words come from the shell (error_chain.rs wording()), never from a table
// here. Flat and tonal: no border, no red; the one status cue is a small
// low-chroma dot.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Check, Copy } from 'lucide-react';
import { Button } from 'rillio/components/ui/button';
import type { UpdateFailure } from 'rillio/common/Platform/shell/updateFailure';

type Props = {
    // Which step failed ("Couldn't update Rillio").
    title: string,
    failure: UpdateFailure,
    // Runs the step that failed again (the check, or the install).
    onRetry: () => void,
};

type CopyState = 'idle' | 'copied' | 'failed';

const UpdateFailureNotice = ({ title, failure, onRetry }: Props) => {
    const [copy, setCopy] = useState<CopyState>('idle');
    const timer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

    // A new failure starts fresh.
    useEffect(() => { setCopy('idle'); }, [failure]);
    useEffect(() => () => clearTimeout(timer.current), []);

    const flash = useCallback((state: CopyState) => {
        setCopy(state);
        clearTimeout(timer.current);
        timer.current = setTimeout(() => setCopy('idle'), 1500);
    }, []);

    // Clipboard API first; where it is refused, the legacy copy from an
    // off-screen textarea. If both fail the button says so: no checkmark
    // claims a copy that did not happen.
    const legacyCopy = useCallback(() => {
        const area = document.createElement('textarea');
        area.value = failure.report;
        area.setAttribute('readonly', '');
        area.style.position = 'fixed';
        area.style.left = '-9999px';
        document.body.appendChild(area);
        area.select();
        let ok = false;
        try { ok = document.execCommand('copy'); } catch { ok = false; }
        document.body.removeChild(area);
        flash(ok ? 'copied' : 'failed');
    }, [failure.report, flash]);

    const onCopy = useCallback(() => {
        if (navigator.clipboard?.writeText) {
            navigator.clipboard.writeText(failure.report).then(() => flash('copied'), legacyCopy);
        } else {
            legacyCopy();
        }
    }, [failure.report, flash, legacyCopy]);

    return (
        <div className="-mt-2 flex w-full flex-col items-start gap-1">
            <div className="flex items-center gap-2 text-sm font-medium text-fg">
                <span className="size-1.5 flex-none rounded-full bg-warning/70" aria-hidden="true" />
                {title}
            </div>
            <div className="text-sm text-fg-muted">{failure.summary}</div>
            {failure.hint ? <div className="text-xs text-fg-subtle">{failure.hint}</div> : null}
            <div className="mt-3 flex items-center gap-2">
                <Button variant="default" size="sm" onClick={onRetry} className="px-4">
                    Try again
                </Button>
                <Button
                    variant="ghost"
                    size="sm"
                    onClick={onCopy}
                    className="min-w-32 bg-surface-hover px-4 font-medium text-fg hover:brightness-110"
                >
                    {copy === 'copied' ? <Check className="size-(--icon-size-sm)" /> : <Copy className="size-(--icon-size-sm)" />}
                    {copy === 'copied' ? 'Copied' : copy === 'failed' ? 'Couldn\'t copy' : 'Copy error'}
                </Button>
            </div>
        </div>
    );
};

export default UpdateFailureNotice;
