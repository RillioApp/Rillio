// Copyright (C) 2017-2026 Smart code 203358507

// The failed state under Settings > Updates: the shell's plain sentence first,
// the full cause chain behind a quiet "Details" (the same treatment as the
// update window's failed state, apps/web/src/update.html). Flat and tonal: the
// chain sits on one surface step, no border, no red. The sentence comes from
// the shell (error_chain.rs summary()), never from a table here.

import React, { useCallback, useEffect, useRef, useState } from 'react';
import { Check, ChevronDown, Copy } from 'lucide-react';
import { Button, IconButton } from 'rillio/components/ui/button';
import { cn } from 'rillio/components/ui/cn';
import type { UpdateFailure } from 'rillio/common/Platform/shell/updateFailure';

type Props = {
    // Which step failed ("Couldn't install the update").
    title: string,
    failure: UpdateFailure,
};

const UpdateFailureDetails = ({ title, failure }: Props) => {
    const [open, setOpen] = useState(false);
    const [copied, setCopied] = useState(false);
    const textRef = useRef<HTMLPreElement>(null);
    const copiedTimer = useRef<ReturnType<typeof setTimeout> | undefined>(undefined);

    // A new failure starts collapsed.
    useEffect(() => { setOpen(false); setCopied(false); }, [failure]);
    useEffect(() => () => clearTimeout(copiedTimer.current), []);

    const showCopied = useCallback(() => {
        setCopied(true);
        clearTimeout(copiedTimer.current);
        copiedTimer.current = setTimeout(() => setCopied(false), 1500);
    }, []);

    // Clipboard API first; where it is refused, select the text and try the
    // legacy copy. If that fails too the text stays selected for Ctrl+C, and
    // no checkmark claims a copy that did not happen.
    const selectAll = useCallback(() => {
        const node = textRef.current;
        const selection = window.getSelection();
        if (!node || !selection) return;
        const range = document.createRange();
        range.selectNodeContents(node);
        selection.removeAllRanges();
        selection.addRange(range);
        let ok = false;
        try { ok = document.execCommand('copy'); } catch { ok = false; }
        if (ok) { selection.removeAllRanges(); showCopied(); }
    }, [showCopied]);

    const copy = useCallback(() => {
        if (navigator.clipboard?.writeText) {
            navigator.clipboard.writeText(failure.message).then(showCopied, selectAll);
        } else {
            selectAll();
        }
    }, [failure.message, showCopied, selectAll]);

    return (
        <div className="-mt-2 flex w-full flex-col items-start gap-1">
            {/* The one status cue: a small low-chroma dot, no red. */}
            <div className="flex items-center gap-2 text-sm font-medium text-fg">
                <span className="size-1.5 flex-none rounded-full bg-warning/70" aria-hidden="true" />
                {title}
            </div>
            <div className="text-sm text-fg-muted">{failure.summary}</div>
            {
                failure.message.length > 0 ?
                    <>
                        <Button
                            variant="ghost"
                            size="sm"
                            aria-expanded={open}
                            onClick={() => setOpen((value) => !value)}
                            className="-ml-3 gap-1 font-medium text-fg-subtle hover:brightness-110"
                        >
                            Details
                            <ChevronDown className={cn('size-(--icon-size-sm) transition-transform duration-200', open && 'rotate-180')} />
                        </Button>
                        {
                            open ?
                                <div className="flex w-full items-start rounded-[10px] bg-surface">
                                    <pre
                                        ref={textRef}
                                        className="m-0 max-h-[calc(7*1.5em+1.25rem)] min-w-0 flex-1 cursor-text select-text overflow-y-auto whitespace-pre-wrap py-2.5 pl-3 pr-1 font-mono text-[11px] leading-[1.5] text-fg-muted [overflow-wrap:anywhere] [scrollbar-width:thin]"
                                    >
                                        {failure.message}
                                    </pre>
                                    <IconButton
                                        size="sm"
                                        aria-label={copied ? 'Copied' : 'Copy details'}
                                        title="Copy details"
                                        onClick={copy}
                                        className="m-1 size-7 flex-none"
                                    >
                                        {copied ? <Check className="size-(--icon-size-sm)" /> : <Copy className="size-(--icon-size-sm)" />}
                                    </IconButton>
                                </div>
                                :
                                null
                        }
                    </>
                    :
                    null
            }
        </div>
    );
};

export default UpdateFailureDetails;
