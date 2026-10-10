// Copyright (C) 2017-2026 Smart code 203358507

/**
 * EpisodeArrows - previous/next episode navigation for the streams view.
 *
 * Wraps the streams region. From `sm` up the two arrows sit out at the
 * content area's side edges, vertically centred on the streams, with a
 * reserved gutter so they never cover a card and read as page navigation
 * rather than a carousel control of the tiles. At phone width there is no
 * room for that gutter, so they become two small labelled pills ("S1E1",
 * "S1E3") in a row above the streams header instead.
 *
 * The neighbours come from adjacentEpisodes (season-then-episode order,
 * specials only from a special); the click follows the target video's own
 * metaDetailsStreams deep link, exactly as picking it from the episode list
 * does. An end of the list shows no arrow at all; a movie or anything
 * without an episode list renders the children untouched.
 *
 * Keyboard: Left/Right step too, but only while nothing on the page holds
 * focus (or one of these arrows does). Everything on this page that already
 * binds the arrow keys (the preset toggle's roving focus, the trailer
 * carousel, the language search, the addon select, the tile row's scroll,
 * the all-streams modal) only does so while focused, so a body-focused key
 * press cannot be meant for any of them.
 */

import React from 'react';
import { useNavigate } from 'react-router';
import { ChevronLeft, ChevronRight } from 'lucide-react';
import { Button } from 'rillio/components/ui/button';
import { cn } from 'rillio/components/ui/cn';

const { adjacentEpisodes } = require('./adjacentEpisodes');
const { default: toPath } = require('rillio-router/toPath');

type Target = {
    id: string;
    season: number;
    episode: number;
    title: string;
    label: string;
    link: string | null;
};

type Props = {
    className?: string;
    // The title's episode list (meta.videos) and the video on screen.
    videos: any[];
    videoId: string;
    children: React.ReactNode;
};

const navigable = (target: Target | null): target is Target & { link: string } =>
    target !== null && typeof target.link === 'string';

// Sizes are in rem and the app's root font is 14px: h-11 is ~38px, size-13
// ~45px.
const ARROW_CLASS = cn(
    'bg-surface text-fg-muted hover:bg-surface-hover hover:text-fg',
    // Phone: a small labelled pill in the row above the streams.
    'h-11 gap-1 px-3.5 text-xs font-medium',
    // sm and up: a round icon button out at the side edge.
    'sm:pointer-events-auto sm:absolute sm:top-1/2 sm:size-13 sm:-translate-y-1/2 sm:p-0',
);

const EpisodeArrows = ({ className, videos, videoId, children }: Props) => {
    const navigate = useNavigate();
    const navRef = React.useRef<HTMLElement>(null);
    const { previous, next } = React.useMemo(() => adjacentEpisodes(videos, videoId), [videos, videoId]);
    const previousTarget = navigable(previous) ? previous : null;
    const nextTarget = navigable(next) ? next : null;
    const hasArrows = previousTarget !== null || nextTarget !== null;

    // replace, like the streams view's own back button: stepping through
    // episodes should not stack one history entry per episode.
    const go = React.useCallback((target: { link: string }) => {
        navigate(toPath(target.link), { replace: true });
    }, [navigate]);

    React.useEffect(() => {
        if (!hasArrows) {
            return;
        }
        const onKeyDown = (event: KeyboardEvent) => {
            if (event.key !== 'ArrowLeft' && event.key !== 'ArrowRight') return;
            if (event.defaultPrevented || event.repeat) return;
            if (event.altKey || event.ctrlKey || event.metaKey || event.shiftKey) return;
            const active = document.activeElement;
            const onArrow = active instanceof HTMLElement && navRef.current !== null && navRef.current.contains(active);
            if (active !== null && active !== document.body && !onArrow) return;
            if (document.querySelector('[role="dialog"]') !== null) return;
            const target = event.key === 'ArrowLeft' ? previousTarget : nextTarget;
            if (target === null) return;
            event.preventDefault();
            go(target);
        };
        document.addEventListener('keydown', onKeyDown);
        return () => document.removeEventListener('keydown', onKeyDown);
    }, [hasArrows, previousTarget, nextTarget, go]);

    return (
        <div className={cn('relative flex flex-col', hasArrows && 'sm:px-16', className)}>
            {
                hasArrows ?
                    <nav
                        ref={navRef}
                        aria-label="Episodes"
                        className="mx-2 flex items-center sm:pointer-events-none sm:absolute sm:inset-0 sm:mx-0"
                    >
                        {
                            previousTarget !== null ?
                                <Button
                                    variant="ghost"
                                    aria-label="Previous episode"
                                    title={previousTarget.label}
                                    onClick={() => go(previousTarget)}
                                    className={cn(ARROW_CLASS, 'sm:-left-4')}
                                >
                                    <ChevronLeft className="size-4 sm:size-6" />
                                    <span className="sm:hidden">S{previousTarget.season}E{previousTarget.episode}</span>
                                </Button>
                                :
                                null
                        }
                        {
                            nextTarget !== null ?
                                <Button
                                    variant="ghost"
                                    aria-label="Next episode"
                                    title={nextTarget.label}
                                    onClick={() => go(nextTarget)}
                                    className={cn(ARROW_CLASS, 'ml-auto sm:-right-4')}
                                >
                                    <span className="sm:hidden">S{nextTarget.season}E{nextTarget.episode}</span>
                                    <ChevronRight className="size-4 sm:size-6" />
                                </Button>
                                :
                                null
                        }
                    </nav>
                    :
                    null
            }
            {children}
        </div>
    );
};

export default EpisodeArrows;
