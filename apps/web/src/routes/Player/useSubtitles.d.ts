// Copyright (C) 2017-2026 Smart code 203358507

type SubtitleTrack = {
    id: string,
    lang: string,
    label?: string | null,
    origin?: string,
    url?: string | null,
    fallbackUrl?: string | null,
    embedded?: boolean,
    local?: boolean,
    exclusive?: boolean,
    generated?: boolean,
    buffer?: ArrayBuffer,
};

type SelectedSubtitleTrack = {
    id: string,
    embedded: boolean,
};

type VideoSubtitleState = {
    stream: { url?: unknown } | null,
    time: number | null,
    subtitlesDelay: number | null,
    subtitlesTracks: SubtitleTrack[],
    selectedSubtitlesTrackId: string | null,
    subtitlesOffset: number | null,
    subtitlesSize: number | null,
    extraSubtitlesTracks: SubtitleTrack[],
    selectedExtraSubtitlesTrackId: string | null,
    extraSubtitlesOffset: number | null,
    extraSubtitlesDelay: number | null,
    extraSubtitlesSize: number | null,
};

type VideoEvents = {
    on: (event: string, listener: (...args: any[]) => void) => void,
    off: (event: string, listener: (...args: any[]) => void) => void,
};

type VideoController = {
    events: VideoEvents,
    state: VideoSubtitleState,
    addExtraSubtitlesTracks: (tracks: SubtitleTrack[]) => void,
    addLocalSubtitles: (filename: string, buffer: ArrayBuffer) => void,
    setGeneratedSubtitles: (text: string, lang: string | null, label: string) => void,
    setSubtitlesTrack: (id: string | null) => void,
    setExtraSubtitlesTrack: (id: string | null) => void,
    setSubtitlesDelay: (delay: number) => void,
    setSubtitlesSize: (size: number) => void,
    setSubtitlesOffset: (offset: number) => void,
    setSubtitlesTextColor: (color: string) => void,
    setSubtitlesBackgroundColor: (color: string) => void,
    setSubtitlesOutlineColor: (color: string) => void,
};

type SubtitlesGenerateState = {
    supported: boolean,
    state: 'idle' | 'downloading' | 'loading' | 'running' | 'done' | 'error',
    // 0..1 while downloading the model.
    progress: number | null,
    detail: string | null,
};

type UseSubtitlesArgs = {
    player: Player,
    video: VideoController,
    settings: Settings,
    streamStateChanged: (state: Partial<StreamState>) => void,
    menusOpen: boolean,
    closeMenus: () => void,
    closeSubtitlesMenu: () => void,
    toggleSubtitlesMenu: () => void,
    // While true (player chrome visible, no menu open), the applied offset is
    // lifted above the control bar; the user's intended offset is restored
    // when it turns false.
    liftOffset?: boolean,
};

type SubtitlesMenuProps = {
    subtitlesLanguage: string | null,
    interfaceLanguage: string,
    subtitlesTracks: SubtitleTrack[],
    selectedSubtitlesTrackId: string | null,
    subtitlesOffset: number | null,
    subtitlesSize: number | null,
    extraSubtitlesTracks: SubtitleTrack[],
    selectedExtraSubtitlesTrackId: string | null,
    extraSubtitlesOffset: number | null,
    extraSubtitlesDelay: number | null,
    subtitlesDelay: number | null,
    extraSubtitlesSize: number | null,
    // Auto-sync availability for the menu button: outside the shell there is
    // no decoder (hidden); embedded tracks expose no cues (disabled).
    subtitlesAutoSync: 'unsupported' | 'unavailable' | 'ready' | 'running',
    onSubtitlesAutoSync: () => void,
    // Generated (whisper) subtitles: shell-only; the row shows the phase.
    subtitlesGenerate: SubtitlesGenerateState,
    onSubtitlesGenerateSelect: () => void,
    onSubtitlesTrackSelected: (track: SubtitleTrack | null) => void,
    onExtraSubtitlesTrackSelected: (track: SubtitleTrack | null) => void,
    onSubtitlesOffsetChanged: (offset: number) => void,
    onSubtitlesSizeChanged: (size: number) => void,
    onExtraSubtitlesOffsetChanged: (offset: number) => void,
    onExtraSubtitlesDelayChanged: (delay: number) => void,
    onExtraSubtitlesSizeChanged: (size: number) => void,
};

type UseSubtitlesResult = {
    streamSubtitles: SubtitleTrack[],
    allSubtitleTracks: SubtitleTrack[],
    extraSubtitleTracks: SubtitleTrack[],
    selectedExtraSubtitleTrackId: string | null,
    subtitlesMenuProps: SubtitlesMenuProps,
};
