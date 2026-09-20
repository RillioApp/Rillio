var EventEmitter = require('eventemitter3');
var cloneDeep = require('lodash.clonedeep');
var deepFreeze = require('deep-freeze');
var ERROR = require('../error');

// Maps the profile's subtitlesSize percentage to mpv's sub-scale. Stremio shipped
// 0.0066 (100% -> 0.66, far too small); 0.01 makes the default 100% land exactly on
// mpv's own default sub-scale of 1.0. The observe path divides by the same factor,
// so the round-trip stays consistent.
var SUBS_SCALE_FACTOR = 0.01;
var EOF_END_TOLERANCE = 60000;

// How long to wait for the shell's answer to the `rillio://` capability query
// before assuming "no". A shell that predates the query never answers at all,
// and an unanswered promise here would park `loadfile` forever (the exact way
// the mpv-version handshake once broke playback), so this ALWAYS settles.
var STREAM_CB_QUERY_TIMEOUT = 3000;

// The url that makes the shell player read this torrent's bytes in-process,
// or null when this stream has no such form.
//
// Only a stream carrying an (infoHash, fileIdx) resolved against OUR streaming
// server gets one (withStreamingServer attaches those two, and only for its own
// stream route). The shell parses this grammar strictly - 40 lowercase hex and
// a non-negative integer, nothing else - so anything that does not fit exactly
// stays on the http url rather than being loaded as something else.
function bytePlaneUrl(stream) {
    if (!stream || typeof stream.infoHash !== 'string') {
        return null;
    }

    var infoHash = stream.infoHash.toLowerCase();
    if (!/^[0-9a-f]{40}$/.test(infoHash)) {
        return null;
    }

    var fileIdx = stream.fileIdx;
    if (typeof fileIdx !== 'number' || !isFinite(fileIdx) || fileIdx < 0 || Math.floor(fileIdx) !== fileIdx) {
        return null;
    }

    return 'rillio://' + infoHash + '/' + fileIdx;
}

var stremioToMPVProps = {
    'loaded': 'loaded',
    'stream': null,
    'paused': 'pause',
    'time': 'time-pos',
    'duration': 'duration',
    'buffering': 'buffering',
    'buffered': 'demuxer-cache-time',
    'volume': 'volume',
    'muted': 'mute',
    'playbackSpeed': 'speed',
    'audioTracks': 'audioTracks',
    'selectedAudioTrackId': 'aid',
    'subtitlesTracks': 'subtitlesTracks',
    'selectedSubtitlesTrackId': 'sid',
    'chapters': 'chapter-list',
    'subtitlesSize': 'sub-scale',
    'subtitlesOffset': 'sub-pos',
    'subtitlesDelay': 'sub-delay',
    // Milliseconds on the web side (like subtitlesDelay), seconds in mpv.
    'audioDelay': 'audio-delay',
    'subtitlesTextColor': 'sub-color',
    'subtitlesBackgroundColor': 'sub-back-color',
    'subtitlesOutlineColor': 'sub-border-color',
    'hdrInfo': null,
    'videoScale': null,
};

function parseVersion(version) {
    return version.split('.').slice(0, 2).map(function (v) { return parseInt(v); });
}

function versionGTE(a, b) {
    var versionA = parseVersion(a);
    var versionB = parseVersion(b);
    if (versionA[0] > versionB[0]) return true;
    if (versionA[0] < versionB[0]) return false;
    return versionA[1] >= versionB[1];
}

function ShellVideo(options) {
    options = options || {};

    var ipc = options.shellTransport;
    var observedProps = {};
    var props = {};
    // `buffering` is the OR of two independent mpv signals; track them
    // separately so one going false cannot clobber the other being true
    // (mpv interleaves `seeking=true` with `paused-for-cache=false` while
    // seeking into a not-yet-downloaded torrent region, which used to hide
    // the loading overlay and leave a black screen for the whole stall).
    var stall = { 'seeking': false, 'paused-for-cache': false };
    var stremioProps = {};
    Object.keys(stremioToMPVProps).forEach(function(key) {
        if(stremioToMPVProps[key]) {
            stremioProps[stremioToMPVProps[key]] = key;
        }
    });
    var resolveMPVVersion;
    var waitForMPVVersion = new Promise(function (resolve) {
        resolveMPVVersion = resolve;
    });
    // Whether this shell can serve `rillio://` (libmpv with stream_cb + the
    // streaming engine in-process). Asked once per instance, alongside the
    // property observers below; the answer arrives as a `rillio-stream-cb`
    // signal. Falls back to false on any shell that does not answer.
    var resolveStreamCb;
    var waitForStreamCb = new Promise(function (resolve) {
        resolveStreamCb = resolve;
    });
    setTimeout(function () {
        resolveStreamCb(false);
    }, STREAM_CB_QUERY_TIMEOUT);
    command('unload');

    ipc.send('mpv-command', ['stop']);
    ipc.send('mpv-observe-prop', 'path');

    ipc.send('mpv-observe-prop', 'time-pos');
    ipc.send('mpv-observe-prop', 'volume');
    ipc.send('mpv-observe-prop', 'pause');
    ipc.send('mpv-observe-prop', 'seeking');
    ipc.send('mpv-observe-prop', 'eof-reached');

    ipc.send('mpv-observe-prop', 'duration');
    ipc.send('mpv-observe-prop', 'metadata');
    ipc.send('mpv-observe-prop', 'video-params'); // video width/height
    ipc.send('mpv-observe-prop', 'track-list');
    ipc.send('mpv-observe-prop', 'chapter-list');

    ipc.send('mpv-observe-prop', 'paused-for-cache');
    ipc.send('mpv-observe-prop', 'cache-buffering-state');
    ipc.send('mpv-observe-prop', 'demuxer-cache-time');

    ipc.send('mpv-observe-prop', 'aid');
    ipc.send('mpv-observe-prop', 'vid');
    ipc.send('mpv-observe-prop', 'sid');
    ipc.send('mpv-observe-prop', 'sub-scale');
    ipc.send('mpv-observe-prop', 'sub-pos');
    ipc.send('mpv-observe-prop', 'sub-delay');
    ipc.send('mpv-observe-prop', 'audio-delay');
    ipc.send('mpv-observe-prop', 'speed');

    ipc.send('mpv-observe-prop', 'mpv-version');
    ipc.send('mpv-observe-prop', 'ffmpeg-version');

    ipc.send('rillio-stream-cb-query');

    var events = new EventEmitter();
    var destroyed = false;
    var stream = null;
    // Armed while the current loadfile went out as rillio://: the same argv
    // with the stream's http url instead, so a byte-plane refusal (end-file
    // with error; the shell answers fast for a torrent its engine does not
    // know) retries ONCE over the server route instead of surfacing a dead
    // player. Cleared on use, so a failing http load errors out normally.
    var bytePlaneFallback = null;

    var avgDuration = 0;
    var minClipDuration = 30;

    function setBackground(visible) {
        // This is a bit of a hack but there is no better way so far
        var bg = visible ? '' : 'transparent';
        for(var container = options.containerElement; container; container = container.parentElement) {
            container.style.background = bg;
        }
        if (((window || {}).document || {}).getElementsByTagName) {
            var body = window.document.getElementsByTagName('body');
            if ((body || [])[0]) {
                body[0].style.background = bg;
            }
        }
    }
    function logProp(args) {
        // eslint-disable-next-line no-console
        console.log(args.name+': '+args.data);
    }
    function embeddedProp(args) {
        return args.data && args.data !== 'no' ? 'EMBEDDED_' + args.data.toString() : null;
    }

    ipc.on('rillio-stream-cb', function(args) {
        resolveStreamCb(!!(args && args.available));
    });

    var last_time = 0;
    ipc.on('mpv-prop-change', function(args) {
        switch (args.name) {
            case 'mpv-version':
                resolveMPVVersion(args.data);
                props[args.name] = logProp(args);
                break;
            case 'ffmpeg-version': {
                props[args.name] = logProp(args);
                break;
            }
            case 'duration': {
                var intDuration = args.data | 0;
                // Accumulate average duration over time. if it is greater than minClipDuration
                // and equal to the currently reported duration, it is returned as video length.
                // If the reported duration changes over time the average duration is always
                // smaller than the currently reported one so we set the video length to 0 as
                // this is a live stream.
                props[args.name] = args.data >= minClipDuration && (!avgDuration || intDuration === avgDuration) ? Math.round(args.data * 1000) : null;
                // The average duration is calculated using right bit shifting by one of the sum of
                // the previous average and the currently reported value. This method is not very precise
                // as we get integer value but we avoid floating point errors. JS uses 32 bit values
                // for bitwise maths so the maximum supported video duration is 1073741823 (2 ^ 30 - 1)
                // which is around 34 years of playback time.
                avgDuration = avgDuration ? (avgDuration + intDuration) >> 1 : intDuration;
                props.loaded = intDuration > 0;
                if(props.loaded) {
                    setBackground(false);
                    onPropChanged('loaded');
                }
                break;
            }
            case 'time-pos': {
                props[args.name] = Math.round(args.data*1000);
                break;
            }
            case 'sub-scale': {
                props[args.name] = Math.round(args.data / SUBS_SCALE_FACTOR);
                break;
            }
            case 'sub-pos': {
                props[args.name] = 100 - args.data;
                break;
            }
            case 'sub-delay':
            case 'audio-delay': {
                props[args.name] = Math.round(args.data*1000);
                break;
            }
            case 'volume': {
                if (typeof args.data === 'number' && isFinite(args.data)) {
                    props[args.name] = args.data;
                    onPropChanged('volume');
                }
                break;
            }
            case 'paused-for-cache':
            case 'seeking':
            {
                stall[args.name] = args.data === true;
                var buffering = stall['seeking'] || stall['paused-for-cache'];
                if(props.buffering !== buffering) {
                    props.buffering = buffering;
                    onPropChanged('buffering');
                }
                break;
            }
            case 'demuxer-cache-time': {
                var cacheTime = args.data || 0;
                props[args.name] = cacheTime > 0 ? Math.floor(cacheTime * 1000) : null;
                onPropChanged('buffered');
                break;
            }
            case 'aid':
            case 'sid':
            case 'vid': {
                props[args.name] = embeddedProp(args);
                break;
            }
            case 'video-params': {
                props[args.name] = args.data;
                var params = args.data || {};
                var gamma = typeof params.gamma === 'string' ? params.gamma : null;
                if (gamma === 'pq' || gamma === 'hlg') {
                    props.hdrInfo = {
                        gamma: gamma,
                        primaries: typeof params.primaries === 'string' ? params.primaries : null,
                        maxCll: typeof params['max-cll'] === 'number' ? params['max-cll'] : null,
                        maxLuma: typeof params['max-luma'] === 'number' ? params['max-luma'] : null,
                    };
                } else {
                    props.hdrInfo = null;
                }
                onPropChanged('hdrInfo');
                break;
            }
            // In that case onPropChanged() is manually invoked as track-list contains all
            // the tracks but we have different event for each track type
            case 'track-list': {
                // `forced` / `default` ride along for the smart track selector:
                // forced tracks are partial translations (signs, alien dialogue)
                // that must not win normal selection but SHOULD win when the
                // audio language matches the viewer's subtitle language.
                props.audioTracks = args.data.filter(function(x) { return x.type === 'audio'; })
                    .map(function(x, index) {
                        return {
                            id: 'EMBEDDED_' + x.id,
                            lang: x.lang === undefined ? 'Track' + (index + 1) : x.lang,
                            label: x.title === undefined || x.lang === undefined ? '' : x.title || x.lang,
                            origin: 'EMBEDDED',
                            embedded: true,
                            forced: x.forced === true,
                            default: x['default'] === true,
                            // The only external audio track is the one the shell
                            // adds itself (the AI dub, `audio-add` is shell-issued).
                            generated: x.external === true,
                            mode: x.id === props.aid ? 'showing' : 'disabled',
                        };
                    });
                onPropChanged('audioTracks');

                props.subtitlesTracks = args.data
                    .filter(function(x) { return x.type === 'sub'; })
                    .map(function(x, index) {
                        return {
                            id: 'EMBEDDED_' + x.id,
                            lang: x.lang === undefined ? 'Track ' + (index + 1) : x.lang,
                            label: x.title === undefined || x.lang === undefined ? '' : x.title || x.lang,
                            origin: 'EMBEDDED',
                            embedded: true,
                            forced: x.forced === true,
                            default: x['default'] === true,
                            mode: x.id === props.sid ? 'showing' : 'disabled',
                        };
                    });
                onPropChanged('subtitlesTracks');
                break;
            }
            // Chapters power the skip-intro pill (a chapter named "Opening"/
            // "Ending" is the most reliable segment source when present) and any
            // future chapter markers on the seek bar. mpv reports seconds;
            // everything time-shaped crossing this bridge is milliseconds.
            case 'chapter-list': {
                props[args.name] = (Array.isArray(args.data) ? args.data : [])
                    .map(function(x, index) {
                        return {
                            title: typeof x.title === 'string' ? x.title : 'Chapter ' + (index + 1),
                            time: typeof x.time === 'number' && isFinite(x.time) ? Math.round(x.time * 1000) : 0,
                        };
                    });
                break;
            }
            default: {
                props[args.name] = args.data;
                break;
            }
        }

        // Cap time update to update only when a second passes
        var current_time = args.name === 'time-pos' ? Math.floor(props['time-pos'] / 1000) : null;
        if((!current_time || last_time !== current_time)&& stremioProps[args.name]) {
            if(current_time) {
                last_time = current_time;
            }
            onPropChanged(stremioProps[args.name]);
        }
    });
    ipc.on('mpv-event-ended', function(args) {
        // older shells report 'other' for every non-error reason, including eof
        if (args.error) {
            if (bytePlaneFallback !== null) {
                var fallbackArgs = bytePlaneFallback;
                bytePlaneFallback = null;
                // eslint-disable-next-line no-console
                console.warn('ShellVideo: rillio:// load failed (' + String(args.error) + '), retrying over http');
                ipc.send('mpv-command', fallbackArgs);
                ipc.send('mpv-set-prop', ['pause', false]);
                return;
            }
            // mpv reports a bare string ("loading failed"). Playback cannot
            // continue, so mark it critical: the player then shows its full
            // error screen (and can ask the streaming server for the real
            // cause) instead of a vague toast over a black screen.
            onError({
                critical: true,
                message: String(args.error),
                error: args.error,
            });
        } else if (!args.reason || args.reason === 'eof' || args.reason === 'other') {
            if (!isKnownEarlyEof()) onEnded();
        }
    });

    function getProp(propName) {
        if (propName === 'hdrInfo') return props.hdrInfo || null;
        if (propName === 'videoScale') return props.videoScale || 'contain';
        if(stremioToMPVProps[propName]) return props[stremioToMPVProps[propName]];
        // eslint-disable-next-line no-console
        console.log('Unsupported prop requested', propName);
        return null;
    }
    function onError(error) {
        events.emit('error', error);
        if (error.critical) {
            command('unload');
        }
    }
    function onEnded() {
        events.emit('ended');
    }
    function isKnownEarlyEof() {
        var time = props['time-pos'];
        var duration = props.duration;
        return typeof time === 'number' &&
            isFinite(time) &&
            typeof duration === 'number' &&
            isFinite(duration) &&
            time + EOF_END_TOLERANCE < duration;
    }
    function onPropChanged(propName) {
        if (observedProps[propName]) {
            events.emit('propChanged', propName, getProp(propName));
        }
    }
    function observeProp(propName) {
        events.emit('propValue', propName, getProp(propName));
        observedProps[propName] = true;
    }
    function setProp(propName, propValue) {
        switch (propName) {
            case 'paused': {
                if (stream !== null) {
                    ipc.send('mpv-set-prop', ['pause', propValue]);
                    // Reflect the new state immediately instead of waiting for the
                    // mpv -> IPC round-trip (like volume/muted/videoScale below do).
                    // Otherwise the play/pause icon and the controls auto-hide
                    // (which keys off `paused`) lag ~200-300ms behind the click.
                    // mpv re-emits the observed `pause` prop, so this self-corrects.
                    props.pause = propValue;
                    onPropChanged('paused');
                }

                break;
            }
            case 'time': {
                if (stream !== null && propValue !== null && isFinite(propValue)) {
                    ipc.send('mpv-set-prop', ['time-pos', propValue/1000]);
                }

                break;
            }
            case 'playbackSpeed': {
                if (stream !== null && propValue !== null && isFinite(propValue)) {
                    ipc.send('mpv-set-prop', ['speed', propValue]);
                }
                break;
            }
            case 'videoScale': {
                if (stream !== null) {
                    switch (propValue) {
                        case 'cover':
                            ipc.send('mpv-set-prop', ['keepaspect', true]);
                            ipc.send('mpv-set-prop', ['panscan', 1.0]);
                            break;
                        case 'fill':
                            ipc.send('mpv-set-prop', ['keepaspect', false]);
                            ipc.send('mpv-set-prop', ['panscan', 0.0]);
                            break;
                        default:
                            ipc.send('mpv-set-prop', ['keepaspect', true]);
                            ipc.send('mpv-set-prop', ['panscan', 0.0]);
                            break;
                    }
                    props.videoScale = propValue;
                    onPropChanged('videoScale');
                }
                break;
            }
            case 'volume': {
                if (stream !== null && propValue !== null && isFinite(propValue)) {
                    props.mute = false;
                    ipc.send('mpv-set-prop', ['mute', 'no']);
                    ipc.send('mpv-set-prop', ['volume', propValue]);
                    onPropChanged('muted');
                    onPropChanged('volume');
                }
                break;
            }
            case 'muted': {
                if (stream !== null) {
                    ipc.send('mpv-set-prop', ['mute', propValue ? 'yes' : 'no']);
                    props.mute = propValue;
                    onPropChanged('muted');
                }
                break;
            }
            case 'selectedAudioTrackId': {
                if (stream !== null) {
                    var actualId = propValue.slice('EMBEDDED_'.length);
                    ipc.send('mpv-set-prop', ['aid', actualId]);
                }
                break;
            }
            case 'selectedSubtitlesTrackId': {
                if (stream !== null) {
                    if(propValue) {
                        var actualId = propValue.slice('EMBEDDED_'.length);
                        ipc.send('mpv-set-prop', ['sid', actualId]);
                        events.emit('subtitlesTrackLoaded', propValue);
                    } else {
                        // turn off subs
                        ipc.send('mpv-set-prop', ['sid', 'no']);
                        props.sid = null;
                    }
                }
                onPropChanged('selectedSubtitlesTrackId');
                break;
            }
            case 'subtitlesSize': {
                ipc.send('mpv-set-prop', [stremioToMPVProps[propName], propValue * SUBS_SCALE_FACTOR]);
                break;
            }
            case 'subtitlesDelay':
            case 'audioDelay': {
                // The web keeps delays in MILLISECONDS (the observe handler above
                // converts mpv's seconds the same way); mpv wants seconds. Sending
                // the raw value here would have set a 250ms nudge to 250 SECONDS,
                // which is why embedded-track delay was never wired before.
                if (propValue !== null && isFinite(propValue)) {
                    ipc.send('mpv-set-prop', [stremioToMPVProps[propName], propValue / 1000]);
                }
                break;
            }
            case 'subtitlesOffset': {
                ipc.send('mpv-set-prop', [stremioToMPVProps[propName], 100 - propValue]);
                break;
            }
            case 'subtitlesTextColor':
            case 'subtitlesBackgroundColor':
            case 'subtitlesOutlineColor':
            {
                // MPV accepts color in #AARRGGBB
                var argb = propValue.replace(/^#(\w{6})(\w{2})$/, '#$2$1');
                ipc.send('mpv-set-prop', [stremioToMPVProps[propName], argb]);
                break;
            }
            default: {
                // eslint-disable-next-line no-console
                console.log('Unhandled setProp for', propName);
            }
        }
    }
    function command(commandName, commandArgs) {
        switch (commandName) {
            case 'load': {
                command('unload');
                if (commandArgs && commandArgs.stream && typeof commandArgs.stream.url === 'string') {
                    Promise.all([waitForMPVVersion, waitForStreamCb]).then(function (handshake) {
                        var mpvVersion = handshake[0];
                        var streamCbAvailable = handshake[1];
                        stream = commandArgs.stream;
                        onPropChanged('stream');

                        // What mpv opens. `stream.url` (the http form) stays the
                        // stream's public identity - the thumbnail shadow, the
                        // subtitle routes and the cast gate all read it - and only
                        // the loadfile target moves to the in-process byte plane.
                        var loadURL = stream.url;
                        if (streamCbAvailable) {
                            var bytePlane = bytePlaneUrl(stream);
                            if (bytePlane !== null) {
                                loadURL = bytePlane;
                            }
                        }

                        var subAssOverride = commandArgs.assSubtitlesStyling ? 'strip' : 'no';
                        ipc.send('mpv-set-prop', ['sub-ass-override', subAssOverride]);

                        var gpuProcessing = !!commandArgs.gpuVideoProcessing &&
                            !!commandArgs.hardwareDecoding;

                        // Hardware decoding
                        var hwdecValue = commandArgs.hardwareDecoding ? (gpuProcessing ? 'd3d11va' : 'auto-copy') : 'no';
                        ipc.send('mpv-set-prop', ['hwdec', hwdecValue]);

                        // GPU video processing
                        if (typeof commandArgs.gpuVideoProcessing === 'boolean') {
                            ipc.send('mpv-set-gpu-video-processing', gpuProcessing);
                        }

                        // Video output
                        var videoOutput = commandArgs.platform === 'windows' ? (commandArgs.videoMode === null ? 'gpu-next' : 'gpu') : 'libmpv';
                        ipc.send('mpv-set-prop', ['vo', videoOutput]);

                        var separateWindow = options.mpvSeparateWindow ? 'yes' : 'no';
                        ipc.send('mpv-set-prop', ['osc', separateWindow]);
                        ipc.send('mpv-set-prop', ['input-default-bindings', separateWindow]);
                        ipc.send('mpv-set-prop', ['input-vo-keyboard', separateWindow]);

                        var startAt = Math.floor(parseInt(commandArgs.time, 10) / 1000) || 0;
                        var loadfileArgs;
                        if (startAt !== 0) {
                            if (versionGTE(mpvVersion, '0.39')) {
                                loadfileArgs = ['loadfile', loadURL, 'replace', '-1', 'start=+' + startAt];
                            } else {
                                loadfileArgs = ['loadfile', loadURL, 'replace', 'start=+' + startAt];
                            }
                        } else {
                            loadfileArgs = ['loadfile', loadURL];
                        }
                        bytePlaneFallback = loadURL !== stream.url ?
                            loadfileArgs.map(function(arg) { return arg === loadURL ? stream.url : arg; })
                            :
                            null;
                        ipc.send('mpv-command', loadfileArgs);
                        ipc.send('mpv-set-prop', ['sid', 'no']);
                        ipc.send('mpv-set-prop', ['pause', false]);
                        ipc.send('mpv-set-prop', ['speed', props.speed]);
                        if (props.aid) {
                            if (typeof props.aid === 'string' && props.aid.startsWith('EMBEDDED_')) {
                                ipc.send('mpv-set-prop', ['aid', props.aid.slice('EMBEDDED_'.length)]);
                            } else {
                                ipc.send('mpv-set-prop', ['aid', props.aid]);
                            }
                        }
                        ipc.send('mpv-set-prop', ['mute', 'no']);

                        onPropChanged('paused');
                        onPropChanged('time');
                        onPropChanged('duration');
                        onPropChanged('buffering');
                        onPropChanged('buffered');
                        onPropChanged('muted');
                        onPropChanged('subtitlesTracks');
                        onPropChanged('selectedSubtitlesTrackId');
                    });
                } else {
                    onError(Object.assign({}, ERROR.UNSUPPORTED_STREAM, {
                        critical: true,
                        stream: commandArgs ? commandArgs.stream : null
                    }));
                }
                break;
            }
            case 'unload': {
                props = {
                    loaded: false,
                    pause: false,
                    mute: false,
                    speed: 1,
                    subtitlesTracks: [],
                    audioTracks: [],
                    'chapter-list': [],
                    buffering: false,
                    buffered: null,
                    aid: null,
                    sid: null,
                };
                stall = { 'seeking': false, 'paused-for-cache': false };
                avgDuration = 0;
                bytePlaneFallback = null;
                ipc.send('mpv-command', ['stop']);
                onPropChanged('loaded');
                onPropChanged('stream');
                onPropChanged('paused');
                onPropChanged('time');
                onPropChanged('duration');
                onPropChanged('buffering');
                onPropChanged('buffered');
                onPropChanged('muted');
                onPropChanged('subtitlesTracks');
                onPropChanged('selectedSubtitlesTrackId');
                onPropChanged('chapters');
                setBackground(true);
                break;
            }
            case 'destroy': {
                command('unload');
                destroyed = true;
                events.removeAllListeners();
                break;
            }
        }
    }

    this.on = function (eventName, listener) {
        if (destroyed) {
            throw new Error('Video is destroyed');
        }

        events.on(eventName, listener);
    };
    this.dispatch = function (action) {
        if (destroyed) {
            throw new Error('Video is destroyed');
        }

        if (action) {
            action = deepFreeze(cloneDeep(action));
            switch (action.type) {
                case 'observeProp': {
                    observeProp(action.propName);
                    break;
                }
                case 'setProp': {
                    setProp(action.propName, action.propValue);
                    return;
                }
                case 'command': {
                    command(
                        action.commandName,
                        action.commandArgs
                    );
                    return;
                }
            }
        }
    };
}
ShellVideo.canPlayStream = function() {
    return Promise.resolve(true);
};

ShellVideo.canPlayWithoutStreamingServer = true;

ShellVideo.manifest = {
    name: 'ShellVideo',
    external: false,
    props: Object.keys(stremioToMPVProps),
    commands: ['load', 'unload', 'destroy'],
    events: [
        'propValue',
        'propChanged',
        'ended',
        'error',
        'subtitlesTrackLoaded',
    ],
};

module.exports = ShellVideo;
