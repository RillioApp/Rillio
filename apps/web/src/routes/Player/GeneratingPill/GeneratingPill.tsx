// Copyright (C) 2017-2026 Smart code 203358507

/**
 * "Generating dub" / "Generating subtitles": the AI worker is still ahead of
 * what can be heard or read. Same material and clearance as the skip pill
 * (player-glass, above the control bar), centered, not a button: it tells,
 * the Audio and Subtitles menus act. Like the skip pill it stays visible
 * while the chrome fades, since the wait is exactly when the viewer looks.
 */

import React from 'react';
import { AudioLines, Captions } from 'lucide-react';
import { useTranslation } from 'react-i18next';

type Props = {
    dub: boolean;
    subtitles: boolean;
};

const GeneratingPill = ({ dub, subtitles }: Props) => {
    const { t } = useTranslation();
    if (!dub && !subtitles) return null;
    const Icon = dub ? AudioLines : Captions;
    const label = dub && subtitles ? t('PLAYER_GENERATING_DUB_AND_SUBTITLES') : dub ? t('PLAYER_GENERATING_DUB') : t('PLAYER_GENERATING_SUBTITLES');
    return (
        <div
            role={'status'}
            className={'pointer-events-none absolute bottom-(--player-chrome-clearance) left-1/2 z-0 flex h-9 -translate-x-1/2 items-center gap-2.5 rounded-full border border-line bg-glass-panel px-4 text-sm font-semibold text-ice backdrop-blur-(--glass-blur) duration-200 animate-in fade-in slide-in-from-bottom-2'}
        >
            <Icon className={'size-4 animate-pulse'} />
            {label}
        </div>
    );
};

export default GeneratingPill;
