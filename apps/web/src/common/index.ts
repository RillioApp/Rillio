// Copyright (C) 2017-2023 Smart code 203358507

import { FileDropProvider, useFileDrop, onFileDrop } from './FileDrop';
import { FullscreenProvider, useFullscreen } from './Fullscreen';
import { PlatformProvider, usePlatform } from './Platform';
import { ToastProvider, useToast } from './Toast';
import { ShortcutsProvider, useShortcuts, onShortcut, useExclusiveKeyboard } from './Shortcuts';
import { DiscordProvider, useDiscord, EMPTY_DISCORD_TIMESTAMPS, getPlaybackDiscordActivity } from './Discord';
import * as CONSTANTS from './CONSTANTS';
import { withCoreSuspender, useCoreSuspender } from './CoreSuspender';
import getVisibleChildrenRange from './getVisibleChildrenRange';
import interfaceLanguages from './interfaceLanguages.json';
import languageNames from './languageNames.json';
import * as languages from './languages';
import routesRegexp from './routesRegexp';
import useAnimationFrame from './useAnimationFrame';
import useBinaryState from './useBinaryState';
import useInterval from './useInterval';
import useLibraryItemState from './useLibraryItemState';
import useLiveRef from './useLiveRef';
import useModelState from './useModelState';
import useNotifications from './useNotifications';
import useOnScrollToBottom from './useOnScrollToBottom';
import useProfile from './useProfile';
import useRouteFocused from './useRouteFocused';
import useSettings from './useSettings';
import useStreamingServer from './useStreamingServer';
import useTimeout from './useTimeout';
import usePlayUrl from './usePlayUrl';
import useTorrent from './useTorrent';
import useTranslate from './useTranslate';
import useOrientation from './useOrientation';
import useLanguageSorting from './useLanguageSorting';

export {
    FileDropProvider,
    useFileDrop,
    onFileDrop,
    FullscreenProvider,
    PlatformProvider,
    usePlatform,
    ShortcutsProvider,
    useShortcuts,
    onShortcut,
    useExclusiveKeyboard,
    ToastProvider,
    useToast,
    DiscordProvider,
    useDiscord,
    EMPTY_DISCORD_TIMESTAMPS,
    getPlaybackDiscordActivity,
    CONSTANTS,
    withCoreSuspender,
    useCoreSuspender,
    getVisibleChildrenRange,
    interfaceLanguages,
    languageNames,
    languages,
    routesRegexp,
    useAnimationFrame,
    useBinaryState,
    useFullscreen,
    useInterval,
    useLibraryItemState,
    useLiveRef,
    useModelState,
    useNotifications,
    useOnScrollToBottom,
    useProfile,
    useRouteFocused,
    useSettings,
    useStreamingServer,
    useTimeout,
    usePlayUrl,
    useTorrent,
    useTranslate,
    useOrientation,
    useLanguageSorting,
};
