// Copyright (C) 2017-2023 Smart code 203358507

import React from 'react';
import Video from '@rillio/video';
import EventEmitter from 'eventemitter3';

// Boundary type for the off-limits packages/video instance. The @rillio/video
// implementation stays untyped JS; this is the minimal surface useVideo drives.
type VideoInstance = {
    dispatch: (action: any, options?: any) => void;
    on: (event: string, listener: (...args: any[]) => void) => void;
    destroy: () => void;
};

// TODO: the mpv/video prop bag is dynamic (propChanged/propValue emit arbitrary
// prop names from packages/video); typed as any pending a real VideoState model.
type VideoInstanceState = any;

const events = new EventEmitter();

const useVideo = () => {
    const video = React.useRef<VideoInstance | null>(null);
    const containerRef = React.useRef<HTMLDivElement | null>(null);

    const [state, setState] = React.useState<VideoInstanceState>({
        manifest: null,
        stream: null,
        paused: null,
        time: null,
        duration: null,
        buffering: null,
        buffered: null,
        volume: null,
        muted: null,
        playbackSpeed: null,
        videoParams: null,
        hdrInfo: null,
        audioTracks: [],
        selectedAudioTrackId: null,
        // A/V sync offsets in ms (mpv seconds converted at the ShellVideo edge).
        audioDelay: null,
        subtitlesDelay: null,
        subtitlesTracks: [],
        selectedSubtitlesTrackId: null,
        chapters: [],
        subtitlesOffset: null,
        subtitlesSize: null,
        subtitlesTextColor: null,
        subtitlesBackgroundColor: null,
        subtitlesOutlineColor: null,
        extraSubtitlesTracks: [],
        selectedExtraSubtitlesTrackId: null,
        extraSubtitlesSize: null,
        extraSubtitlesDelay: null,
        extraSubtitlesOffset: null,
        extraSubtitlesTextColor: null,
        extraSubtitlesBackgroundColor: null,
        extraSubtitlesOutlineColor: null,
        fullscreen: null,
    });

    const dispatch = (action: any, options?: any) => {
        if (video.current && containerRef.current) {
            try {
                video.current.dispatch(action, {
                    ...options,
                    containerElement: containerRef.current,
                });
            } catch (error) {
                console.error('Video:', error);
            }
        }
    };

    const load = (args: any, options?: any) => {
        dispatch({
            type: 'command',
            commandName: 'load',
            commandArgs: args
        }, options);
    };

    const unload = () => {
        dispatch({
            type: 'command',
            commandName: 'unload',
        });
    };

    const addExtraSubtitlesTracks = (tracks: any) => {
        dispatch({
            type: 'command',
            commandName: 'addExtraSubtitlesTracks',
            commandArgs: {
                tracks,
            },
        });
    };

    const addLocalSubtitles = (filename: string, buffer: ArrayBuffer) => {
        dispatch({
            type: 'command',
            commandName: 'addLocalSubtitles',
            commandArgs: {
                filename,
                buffer,
            },
        });
    };

    // The live "Generated" track: the whole VTT transcribed so far (the shell's
    // whisper worker sends batches; useSubtitles accumulates and re-renders).
    const setGeneratedSubtitles = (text: string, lang: string | null, label: string) => {
        dispatch({
            type: 'command',
            commandName: 'setGeneratedSubtitles',
            commandArgs: {
                text,
                lang: lang ?? undefined,
                label,
            },
        });
    };

    const setProp = (name: string, value: any) => {
        dispatch({ type: 'setProp', propName: name, propValue: value });
    };

    const setPaused = (state: boolean) => {
        setProp('paused', state);
    };

    const setVolume = (volume: number) => {
        setProp('volume', volume);
    };

    const setMuted = (state: boolean) => {
        setProp('muted', state);
    };

    const setTime = (time: number) => {
        setProp('time', time);
    };

    const setPlaybackSpeed = (rate: number) => {
        setProp('playbackSpeed', rate);
    };

    const setAudioTrack = (id: string) => {
        setProp('selectedAudioTrackId', id);
    };

    const setSubtitlesTrack = (id: string | null) => {
        setProp('selectedSubtitlesTrackId', id);
        setProp('selectedExtraSubtitlesTrackId', null);
    };

    const setExtraSubtitlesTrack = (id: string | null) => {
        setProp('selectedSubtitlesTrackId', null);
        setProp('selectedExtraSubtitlesTrackId', id);
    };

    // One delay for whichever subtitle track is active: addon tracks are drawn
    // by the web (extraSubtitlesDelay), embedded tracks by mpv (subtitlesDelay
    // -> sub-delay). Setting both keeps the menu's single control honest across
    // the two renderers, same as size/offset/colors below.
    const setSubtitlesDelay = (delay: number) => {
        setProp('extraSubtitlesDelay', delay);
        setProp('subtitlesDelay', delay);
    };

    const setAudioDelay = (delay: number) => {
        setProp('audioDelay', delay);
    };

    const setSubtitlesSize = (size: number) => {
        setProp('subtitlesSize', size);
        setProp('extraSubtitlesSize', size);
    };

    const setSubtitlesOffset = (offset: number) => {
        setProp('subtitlesOffset', offset);
        setProp('extraSubtitlesOffset', offset);
    };

    const setVideoScale = (scale: string) => {
        setProp('videoScale', scale);
    };

    const setFullscreen = (state: boolean) => {
        setProp('fullscreen', state);
    };

    const setSubtitlesTextColor = (color: string) => {
        setProp('subtitlesTextColor', color);
        setProp('extraSubtitlesTextColor', color);
    };

    const setSubtitlesBackgroundColor = (color: string) => {
        setProp('subtitlesBackgroundColor', color);
        setProp('extraSubtitlesBackgroundColor', color);
    };

    const setSubtitlesOutlineColor = (color: string) => {
        setProp('subtitlesOutlineColor', color);
        setProp('extraSubtitlesOutlineColor', color);
    };

    const onError = (error: any) => {
        events.emit('error', error);
    };

    const onEnded = () => {
        events.emit('ended');
    };

    const onSubtitlesTrackLoaded = (track: any) => {
        events.emit('subtitlesTrackLoaded', track);
    };

    const onExtraSubtitlesTrackLoaded = (track: any, cues: [number, number][], lines: { startMs: number, endMs: number, text: string }[]) => {
        events.emit('extraSubtitlesTrackLoaded', track, cues, lines);
    };

    const onExtraSubtitlesTrackAdded = (track: any) => {
        events.emit('extraSubtitlesTrackAdded', track);
    };

    const onPropChanged = (name: string, value: any) => {
        setState((state: VideoInstanceState) => ({
            ...state,
            [name]: value
        }));
    };

    const onImplementationChanged = (manifest: any) => {
        manifest.props.forEach((propName: string) => dispatch(({ type: 'observeProp', propName })));
        setState((state: VideoInstanceState) => ({
            ...state,
            manifest
        }));

        events.emit('implementationChanged', manifest);
    };

    React.useEffect(() => {
        video.current = new Video();
        video.current.on('error', onError);
        video.current.on('ended', onEnded);
        video.current.on('propChanged', onPropChanged);
        video.current.on('propValue', onPropChanged);
        video.current.on('implementationChanged', onImplementationChanged);
        video.current.on('subtitlesTrackLoaded', onSubtitlesTrackLoaded);
        video.current.on('extraSubtitlesTrackLoaded', onExtraSubtitlesTrackLoaded);
        video.current.on('extraSubtitlesTrackAdded', onExtraSubtitlesTrackAdded);

        return () => {
            if (video.current) {
                try {
                    video.current.destroy();
                } catch (err) {
                    console.error('Error destroying video:', err);
                }
            }
        };
    }, []);

    return {
        events,
        containerRef,
        state,
        load,
        unload,
        addExtraSubtitlesTracks,
        addLocalSubtitles,
        setGeneratedSubtitles,
        setPaused,
        setVolume,
        setMuted,
        setTime,
        setPlaybackSpeed,
        setAudioTrack,
        setAudioDelay,
        setSubtitlesTrack,
        setSubtitlesDelay,
        setSubtitlesSize,
        setSubtitlesOffset,
        setSubtitlesTextColor,
        setSubtitlesBackgroundColor,
        setSubtitlesOutlineColor,
        setExtraSubtitlesTrack,
        setVideoScale,
        setFullscreen,
    };
};

export default useVideo;
