import React, { forwardRef, useLayoutEffect, useMemo, useState } from 'react';
import { Image } from 'rillio/components';
import { cn } from 'rillio/components/ui/cn';

// The loading mark, everywhere the player waits (before playback, a stall
// mid-play, the dub's buffering): the title's logo, or its name as text when
// there is no logo, filling left-to-right by clip-path with progress
// (foreground copy) over a dim static copy, both gently breathing. Never the
// brand symbol: it is not part of the player's design. The breathe keyframe
// lives in tailwind.css.
const FILL = 'absolute animate-[buffering-pulse_2s_infinite] [transition:clip-path_0.1s_ease-in-out]';
const LOGO = FILL + ' block h-auto max-h-60 w-auto max-w-60';
const TITLE = FILL + ' pointer-events-none max-w-[min(44rem,calc(100vw-3rem))] px-6 text-center text-2xl font-semibold leading-snug text-fg [overflow-wrap:anywhere]';

type Props = {
    className: string,
    logo: string,
    // Text fallback for streams with no meta (e.g. played from the Cached
    // page): when there is no usable logo, the stream/file name is rendered
    // as a title instead of the bare symbol logo.
    title?: string | null,
    progress: number,
    infoHash?: string | null,
    loaded?: boolean,
    hasStatistics?: boolean,
    peers?: number,
    speed?: number,
    completed?: number,
    // Sustained-slow escalation (from useSlowDownload). When escalated, the panel
    // offers actions instead of a bare "downloading slowly" note.
    escalated?: boolean,
    connectionSlow?: boolean | null,
    fastModeAvailable?: boolean,
    onTryDifferentSource?: () => void,
    onSwitchToFastMode?: () => void,
    // Why the player waits when the stream itself is not the reason (the AI
    // dub is not made yet): a heading and a progress line under the mark.
    note?: { heading: string, detail: string } | null,
};

// Pre-playback status panel for torrent streams. When a torrent has been added
// but playback has not started yet, the bare pulsing logo does not explain why,
// so we surface the honest reason (buffering) plus the numbers that decide it:
// how much has downloaded, the current speed, and how many peers we are pulling
// from. All stats are optional-chained with defaults, so missing stats fall
// back to a plain "Buffering" without numbers rather than crashing.
const Buffering = forwardRef<HTMLDivElement, Props>(({ className, logo, title, progress, infoHash, loaded, hasStatistics, peers, speed, completed, escalated, connectionSlow, fastModeAvailable, onTryDifferentSource, onSwitchToFastMode, note }, ref) => {
    const style = useMemo(() => {
        return {
            clipPath: `inset(0 ${100 - progress}% 0 0)`,
        };
    }, [progress]);

    // A logo that is missing, or one that failed to load, gives way to the
    // title text (the stream/file name for meta-less playback), same fill.
    const [logoBroken, setLogoBroken] = useState(false);
    useLayoutEffect(() => {
        setLogoBroken(false);
    }, [logo]);
    const hasLogo = typeof logo === 'string' && logo.length > 0 && !logoBroken;
    const titleText = typeof title === 'string' && title.length > 0 ? title : null;

    // Only a torrent stream that has not started playing yet gets the panel;
    // direct (non-torrent) streams have no infoHash, and once loaded is true the
    // "before playback can begin" wording no longer applies.
    const showStatus = typeof infoHash === 'string' && infoHash.length > 0 && !loaded;

    const peerCount = peers ?? 0;
    const speedValue = speed ?? 0;
    const completedValue = Math.min(Math.max(completed ?? 0, 0), 100);
    // A source with peers but a near-zero rate is stalled or slow; with no peers
    // it is simply still finding sources, which is a different (honest) message.
    const slow = hasStatistics === true && peerCount > 0 && speedValue < 0.5;

    // Fast mode is only worth suggesting when it can actually help: it is not
    // already active, and the bottleneck is the source rather than the
    // connection (a slow link will not benefit from more peers).
    const showFastMode = escalated === true && fastModeAvailable === true && connectionSlow !== true;
    // Word the escalation from the speed test: a slow CONNECTION will not improve
    // with another source; otherwise the SOURCE is the slow part.
    const escalatedMessage = connectionSlow === true ?
        'Your internet connection looks slow, so this may not improve with another source. Improving your connection will help.'
        :
        showFastMode ?
            'This source is downloading slowly. Try a different source, or turn on Fast mode.'
            :
            'This source is downloading slowly. Try a different source.';

    return (
        <div ref={ref} className={cn('flex items-center justify-center', className)}>
            {
                hasLogo ?
                    <>
                        <Image
                            className={LOGO}
                            style={style}
                            src={logo}
                            alt={' '}
                            fallbackSrc={''}
                            renderFallback={() => null}
                            onError={() => setLogoBroken(true)}
                        />
                        <Image
                            className={cn(LOGO, 'opacity-20!')}
                            src={logo}
                            alt={' '}
                            fallbackSrc={''}
                            renderFallback={() => null}
                            onError={() => undefined}
                        />
                    </>
                    :
                    titleText !== null ?
                        <>
                            <div className={TITLE} style={style} title={titleText}>
                                {titleText}
                            </div>
                            <div className={cn(TITLE, 'opacity-20!')}>
                                {titleText}
                            </div>
                        </>
                        :
                        null
            }
            {
                showStatus ?
                    <div className="pointer-events-none absolute bottom-[16%] left-1/2 flex w-[min(28rem,calc(100vw-3rem))] -translate-x-1/2 flex-col items-center gap-2 px-6 text-center">
                        <div className="text-sm font-semibold uppercase tracking-[0.08em] text-fg">
                            Initializing
                        </div>
                        {
                            hasStatistics === true ?
                                <div className="flex flex-wrap items-center justify-center gap-x-2.5 gap-y-1 text-sm text-fg-muted">
                                    <span>{completedValue}% downloaded</span>
                                    <span className="text-fg-subtle">·</span>
                                    <span>{speedValue} MB/s</span>
                                    <span className="text-fg-subtle">·</span>
                                    <span>{peerCount === 1 ? '1 peer' : `${peerCount} peers`}</span>
                                </div>
                                :
                                null
                        }
                        <div className="text-xs leading-relaxed text-fg-subtle">
                            Waiting for enough of the video to download before playback can begin.
                        </div>
                        {
                            escalated === true ?
                                <div className="pointer-events-auto mt-1 flex flex-col items-center gap-3">
                                    <div className="text-xs leading-relaxed text-warning">
                                        {escalatedMessage}
                                    </div>
                                    <div className="flex flex-wrap items-center justify-center gap-2">
                                        <button
                                            type="button"
                                            onClick={onTryDifferentSource}
                                            className="rounded-full bg-accent px-4 py-2 text-sm font-semibold text-bg transition hover:brightness-110"
                                        >
                                            Try a different source
                                        </button>
                                        {
                                            showFastMode ?
                                                <button
                                                    type="button"
                                                    onClick={onSwitchToFastMode}
                                                    className="rounded-full bg-surface px-4 py-2 text-sm text-fg transition hover:bg-surface-hover"
                                                >
                                                    Switch to Fast mode
                                                </button>
                                                :
                                                null
                                        }
                                    </div>
                                    {
                                        showFastMode ?
                                            <div className="text-[0.6875rem] leading-relaxed text-fg-subtle">
                                                Fast mode connects to more peers for higher speed, which exposes your IP to more of them. Best used with a VPN.
                                            </div>
                                            :
                                            null
                                    }
                                </div>
                                :
                                slow ?
                                    <div className="text-xs leading-relaxed text-warning">
                                        This source is downloading slowly, playback will start once enough has buffered.
                                    </div>
                                    :
                                    null
                        }
                    </div>
                    :
                    note ?
                        <div role="status" className="pointer-events-none absolute bottom-[16%] left-1/2 flex w-[min(28rem,calc(100vw-3rem))] -translate-x-1/2 flex-col items-center gap-2 px-6 text-center">
                            <div className="text-sm font-semibold uppercase tracking-[0.08em] text-fg">
                                {note.heading}
                            </div>
                            <div className="text-sm text-fg-muted">
                                {note.detail}
                            </div>
                        </div>
                        :
                        null
            }
        </div>
    );
});

export default Buffering;
