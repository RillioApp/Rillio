var EventEmitter = require('eventemitter3');
var url = require('url');
var hat = require('hat');
var cloneDeep = require('lodash.clonedeep');
var deepFreeze = require('deep-freeze');
var mediaCapabilities = require('../mediaCapabilities');
var serverContext = require('../serverContext');
var convertStream = require('./convertStream');
var fetchVideoParams = require('./fetchVideoParams');
var isPlayerLoaded = require('./isPlayerLoaded');
var supportsTranscoding = require('../supportsTranscoding');
var ERROR = require('../error');

// Does `mediaURL` point at THIS streaming server's own stream route for
// `(infoHash, fileIdx)`? Only then may the shell player swap it for the
// in-process `rillio://` byte plane, which always reads the LOCAL engine.
//
// Three gates, all load-bearing:
//   1. The configured server must BE the in-process one (serverContext).
//      convertStream also reports an infoHash for a `/proxy` rewrite and for
//      an addon that ships a pre-computed streaming-server url, and the user
//      can select a remote server in Settings; reading those torrents from
//      the local engine would download them all over again.
//   2. The url must be the stream route for exactly this (infoHash, fileIdx).
//   3. Only `?tr=` params (or an empty query - the core's url builder leaves a
//      dangling '?') may ride on the route: the server gives `?f=<name>`
//      priority over the numeric idx, so any other query can name a DIFFERENT
//      file than the byte plane would open.
function torrentIdentity(streamingServerURL, mediaURL, infoHash, fileIdx) {
    if (typeof streamingServerURL !== 'string' || typeof mediaURL !== 'string' || typeof infoHash !== 'string') {
        return {};
    }

    if (!serverContext.isLocalServerUrl(streamingServerURL)) {
        return {};
    }

    // The core hands fileIdx over as a number, but a decoded deep link can
    // carry it as a digit string; both name the same file.
    var fileIdxNumber = typeof fileIdx === 'number' ?
        fileIdx
        :
        typeof fileIdx === 'string' && /^-?\d+$/.test(fileIdx) ? parseInt(fileIdx, 10) : NaN;
    if (!isFinite(fileIdxNumber)) {
        return {};
    }

    var route = url.resolve(streamingServerURL, '/' + encodeURIComponent(infoHash) + '/' + encodeURIComponent(fileIdxNumber));
    if (mediaURL !== route) {
        if (mediaURL.indexOf(route + '?') !== 0) {
            return {};
        }

        var query = mediaURL.slice(route.length + 1);
        var onlyTrackers = Array.from(new URLSearchParams(query).keys())
            .every(function(key) { return key === 'tr'; });
        if (!onlyTrackers) {
            return {};
        }
    }

    return { infoHash: infoHash, fileIdx: fileIdxNumber };
}

// Subtitle tracks routed through the server's `/subtitles.vtt?from=` converter
// (SRT and friends become VTT for the HTML renderer), keeping the original url
// as the fallback for when the conversion fails. One rule for every path a
// track can take into the player: the stream's own tracks on load (direct
// play AND transcode) and the addon tracks added later. The direct-play load
// used to rebuild the stream as `{ url }` alone, so stream-attached subtitles
// silently vanished in the shell, where direct play is the normal case.
function serverSubtitles(streamingServerURL, subtitles) {
    if (!Array.isArray(subtitles)) {
        return [];
    }

    return subtitles.map(function(track) {
        return Object.assign({}, track, {
            fallbackUrl: track.url,
            url: typeof track.url === 'string' && typeof streamingServerURL === 'string' ?
                url.resolve(streamingServerURL, '/subtitles.vtt?' + new URLSearchParams([['from', track.url]]).toString())
                :
                track.url
        });
    });
}

function withStreamingServer(Video) {
    function VideoWithStreamingServer(options) {
        options = options || {};

        var video = new Video(options);
        video.on('error', onVideoError);
        video.on('propValue', onVideoPropEvent.bind(null, 'propValue'));
        video.on('propChanged', onVideoPropEvent.bind(null, 'propChanged'));
        Video.manifest.events
            .filter(function(eventName) {
                return !['error', 'propValue', 'propChanged'].includes(eventName);
            })
            .forEach(function(eventName) {
                video.on(eventName, onOtherVideoEvent(eventName));
            });

        var self = this;
        var loadArgs = null;
        var loaded = false;
        var actionsQueue = [];
        var videoParams = null;
        var events = new EventEmitter();
        var destroyed = false;
        var observedProps = {
            stream: false,
            videoParams: false
        };

        function canLoadDirectlyWithoutServer(commandArgs) {
            if (Video.canPlayWithoutStreamingServer !== true || !commandArgs || !commandArgs.stream) {
                return false;
            }

            var stream = commandArgs.stream;
            var proxyHeaders = stream.behaviorHints && stream.behaviorHints.proxyHeaders;
            var proxyStreamsEnabled = commandArgs.streamingServerSettings && commandArgs.streamingServerSettings.proxyStreamsEnabled;

            return typeof stream.url === 'string' &&
                stream.url.indexOf('magnet:') !== 0 &&
                !commandArgs.forceTranscoding &&
                !proxyHeaders &&
                !proxyStreamsEnabled;
        }

        function flushActionsQueue() {
            while (actionsQueue.length > 0) {
                var action = actionsQueue.shift();
                self.dispatch.call(self, action);
            }
        }
        function onVideoError(error) {
            events.emit('error', error);
            if (error.critical) {
                command('unload');
            }
        }
        function onVideoPropEvent(eventName, propName, propValue) {
            events.emit(eventName, propName, getProp(propName, propValue));
        }
        function onOtherVideoEvent(eventName) {
            return function() {
                events.emit.apply(events, [eventName].concat(Array.from(arguments)));
            };
        }
        function onPropChanged(propName) {
            if (observedProps[propName]) {
                events.emit('propChanged', propName, getProp(propName, null));
            }
        }
        function onError(error) {
            events.emit('error', error);
            if (error.critical) {
                command('unload');
                video.dispatch({ type: 'command', commandName: 'unload' });
            }
        }
        function getProp(propName, videoPropValue) {
            switch (propName) {
                case 'stream': {
                    return loadArgs !== null ? loadArgs.stream : null;
                }
                case 'videoParams': {
                    return videoParams;
                }
                default: {
                    return videoPropValue;
                }
            }
        }
        function observeProp(propName) {
            switch (propName) {
                case 'stream':
                case 'videoParams': {
                    events.emit('propValue', propName, getProp(propName, null));
                    observedProps[propName] = true;
                    return true;
                }
                default: {
                    return false;
                }
            }
        }
        function command(commandName, commandArgs) {
            switch (commandName) {
                case 'load': {
                    // THE entry point for streaming-server urls: everything
                    // below (convertStream, the hlsv2 and subtitles.vtt urls,
                    // fetchVideoParams, the error payloads) derives from this
                    // one value, so the symbolic -> real mapping happens here
                    // and exactly once. The sync map covers the common case
                    // (address already resolved); the pipeline below ALSO waits
                    // for the shell's address handshake (bounded), because a
                    // load can fire before the server has reported where it
                    // bound, and the symbolic port must never reach mpv.
                    // loadArgs is kept pointing at the rewritten copy, which
                    // keeps the `commandArgs !== loadArgs` staleness checks
                    // below coherent.
                    if (commandArgs && typeof commandArgs.streamingServerURL === 'string') {
                        commandArgs = Object.assign({}, commandArgs, {
                            streamingServerURL: serverContext.resolveServerUrl(commandArgs.streamingServerURL)
                        });
                    }
                    var hasStreamingServer = commandArgs && typeof commandArgs.streamingServerURL === 'string';
                    if (commandArgs && commandArgs.stream && (hasStreamingServer || canLoadDirectlyWithoutServer(commandArgs))) {
                        command('unload');
                        video.dispatch({ type: 'command', commandName: 'unload' });
                        loadArgs = commandArgs;
                        onPropChanged('stream');
                        (hasStreamingServer ?
                            serverContext.resolveServerUrlWhenReady(commandArgs.streamingServerURL)
                            :
                            Promise.resolve(null))
                            .then(function(resolvedServerURL) {
                                if (commandArgs !== loadArgs) {
                                    return null;
                                }

                                if (hasStreamingServer && typeof resolvedServerURL === 'string' && resolvedServerURL !== commandArgs.streamingServerURL) {
                                    commandArgs = Object.assign({}, commandArgs, {
                                        streamingServerURL: resolvedServerURL
                                    });
                                    loadArgs = commandArgs;
                                }

                                return convertStream(commandArgs.streamingServerURL, commandArgs.stream, commandArgs.seriesInfo, commandArgs.streamingServerSettings);
                            })
                            .then(function(result) {
                                if (result === null || commandArgs !== loadArgs) {
                                    return null;
                                }

                                // A stream url built from the SYMBOLIC default is
                                // pointed at the real server here: the core builds
                                // its torrent urls from the persisted setting, and
                                // this value is what mpv (or the transcode probe)
                                // actually opens.
                                var mediaURL = serverContext.resolveServerUrl(result.url);
                                var infoHash = result.infoHash;
                                var fileIdx = result.fileIdx;
                                var formats = Array.isArray(commandArgs.formats) ?
                                    commandArgs.formats
                                    :
                                    mediaCapabilities.formats;
                                var videoCodecs = Array.isArray(commandArgs.videoCodecs) ?
                                    commandArgs.videoCodecs
                                    :
                                    mediaCapabilities.videoCodecs;
                                var audioCodecs = Array.isArray(commandArgs.audioCodecs) ?
                                    commandArgs.audioCodecs
                                    :
                                    mediaCapabilities.audioCodecs;
                                var maxAudioChannels = commandArgs.maxAudioChannels !== null && isFinite(commandArgs.maxAudioChannels) ?
                                    commandArgs.maxAudioChannels
                                    :
                                    mediaCapabilities.maxAudioChannels;
                                var canPlayStreamOptions = Object.assign({}, commandArgs, {
                                    formats: formats,
                                    videoCodecs: videoCodecs,
                                    audioCodecs: audioCodecs,
                                    maxAudioChannels: maxAudioChannels
                                });
                                return (commandArgs.forceTranscoding ? Promise.resolve(false) : VideoWithStreamingServer.canPlayStream({ url: mediaURL }, canPlayStreamOptions))
                                    .catch(function(error) {
                                        console.warn('Media probe error', error);
                                        return false;
                                    })
                                    .then(function(canPlay) {
                                        if (canPlay) {
                                            return {
                                                mediaURL: mediaURL,
                                                infoHash: infoHash,
                                                fileIdx: fileIdx,
                                                // The identity rides along so a
                                                // player that can read the
                                                // torrent in-process knows WHICH
                                                // file this url is (see
                                                // torrentIdentity). Absent for
                                                // every other kind of stream, and
                                                // for the transcode branch below,
                                                // whose url is a server route with
                                                // no byte-plane equivalent.
                                                stream: Object.assign(
                                                    {
                                                        url: mediaURL,
                                                        subtitles: serverSubtitles(commandArgs.streamingServerURL, commandArgs.stream.subtitles)
                                                    },
                                                    torrentIdentity(commandArgs.streamingServerURL, mediaURL, infoHash, fileIdx)
                                                )
                                            };
                                        }

                                        var id = hat();
                                        var queryParams = new URLSearchParams([['mediaURL', mediaURL]]);
                                        if (commandArgs.forceTranscoding) {
                                            queryParams.set('forceTranscoding', '1');
                                        }

                                        videoCodecs.forEach(function(videoCodec) {
                                            queryParams.append('videoCodecs', videoCodec);
                                        });

                                        audioCodecs.forEach(function(audioCodec) {
                                            queryParams.append('audioCodecs', audioCodec);
                                        });

                                        queryParams.set('maxAudioChannels', maxAudioChannels);

                                        return {
                                            mediaURL: mediaURL,
                                            infoHash: infoHash,
                                            fileIdx: fileIdx,
                                            stream: {
                                                url: url.resolve(commandArgs.streamingServerURL, '/hlsv2/' + id + '/master.m3u8?' + queryParams.toString()),
                                                subtitles: serverSubtitles(commandArgs.streamingServerURL, commandArgs.stream.subtitles),
                                                behaviorHints: {
                                                    headers: {
                                                        'content-type': 'application/vnd.apple.mpegurl'
                                                    }
                                                }
                                            }
                                        };
                                    });
                            })
                            .then(function(result) {
                                if (!result || commandArgs !== loadArgs) {
                                    return;
                                }

                                video.dispatch({
                                    type: 'command',
                                    commandName: 'load',
                                    commandArgs: Object.assign({}, commandArgs, {
                                        stream: result.stream
                                    })
                                });
                                loaded = true;
                                flushActionsQueue();

                                isPlayerLoaded(video, Video.manifest.props)
                                    .then(function() {
                                        return hasStreamingServer ?
                                            fetchVideoParams(commandArgs.streamingServerURL, result.mediaURL, result.infoHash, result.fileIdx, commandArgs.stream.behaviorHints)
                                            :
                                            {
                                                hash: null,
                                                size: null,
                                                filename: commandArgs.stream.behaviorHints && commandArgs.stream.behaviorHints.filename || null
                                            };
                                    })
                                    .then(function(result) {
                                        if (commandArgs !== loadArgs) {
                                            return;
                                        }

                                        videoParams = result;
                                        onPropChanged('videoParams');
                                    })
                                    .catch(function(error) {
                                        if (commandArgs !== loadArgs) {
                                            return;
                                        }

                                        // eslint-disable-next-line no-console
                                        console.error(error);
                                        videoParams = { hash: null, size: null, filename: null };
                                        onPropChanged('videoParams');
                                    });
                            })
                            .catch(function(error) {
                                if (commandArgs !== loadArgs) {
                                    return;
                                }

                                onError(Object.assign({}, ERROR.WITH_STREAMING_SERVER.CONVERT_FAILED, {
                                    error: error,
                                    critical: true,
                                    stream: commandArgs.stream,
                                    streamingServerURL: commandArgs.streamingServerURL
                                }));
                            });
                    } else {
                        onError(Object.assign({}, ERROR.UNSUPPORTED_STREAM, {
                            critical: true,
                            stream: commandArgs ? commandArgs.stream : null,
                            streamingServerURL: commandArgs && typeof commandArgs.streamingServerURL === 'string' ? commandArgs.streamingServerURL : null
                        }));
                    }

                    return true;
                }
                case 'addExtraSubtitlesTracks': {
                    if (loadArgs && commandArgs && Array.isArray(commandArgs.tracks)) {
                        if (loaded) {
                            video.dispatch({
                                type: 'command',
                                commandName: 'addExtraSubtitlesTracks',
                                commandArgs: Object.assign({}, commandArgs, {
                                    tracks: serverSubtitles(loadArgs.streamingServerURL, commandArgs.tracks)
                                })
                            });
                        } else {
                            actionsQueue.push({
                                type: 'command',
                                commandName: 'addExtraSubtitlesTracks',
                                commandArgs: commandArgs
                            });
                        }
                    }

                    return true;
                }
                case 'unload': {
                    loadArgs = null;
                    loaded = false;
                    actionsQueue = [];
                    videoParams = null;
                    onPropChanged('stream');
                    onPropChanged('videoParams');
                    return false;
                }
                case 'destroy': {
                    command('unload');
                    destroyed = true;
                    video.dispatch({ type: 'command', commandName: 'destroy' });
                    events.removeAllListeners();
                    return true;
                }
                default: {
                    if (!loaded) {
                        actionsQueue.push({
                            type: 'command',
                            commandName: commandName,
                            commandArgs: commandArgs
                        });

                        return true;
                    }

                    return false;
                }
            }
        }

        this.on = function(eventName, listener) {
            if (destroyed) {
                throw new Error('Video is destroyed');
            }

            events.on(eventName, listener);
        };
        this.dispatch = function(action) {
            if (destroyed) {
                throw new Error('Video is destroyed');
            }

            if (action) {
                action = deepFreeze(cloneDeep(action));
                switch (action.type) {
                    case 'observeProp': {
                        if (observeProp(action.propName)) {
                            return;
                        }

                        break;
                    }
                    case 'command': {
                        if (command(action.commandName, action.commandArgs)) {
                            return;
                        }

                        break;
                    }
                }
            }

            video.dispatch(action);
        };
    }

    VideoWithStreamingServer.canPlayStream = function(stream, options) {
        if (!options || typeof options.streamingServerURL !== 'string') {
            return Video.canPlayStream(stream);
        }

        // The other entry point: this is called straight from the app as well
        // as from `load` above, so it maps the symbolic url itself.
        var streamingServerURL = serverContext.resolveServerUrl(options.streamingServerURL);

        return supportsTranscoding()
            .then(function(supported) {
                if (!supported) {
                    // we cannot probe the video in this case
                    return Video.canPlayStream(stream);
                }
                // probing normally gives more accurate results
                var queryParams = new URLSearchParams([['mediaURL', stream.url]]);
                return serverContext.fetch(url.resolve(streamingServerURL, '/hlsv2/probe?' + queryParams.toString()))
                    .then(function(resp) {
                        return resp.json();
                    })
                    .then(function(probe) {
                        var isFormatSupported = options.formats.some(function(format) {
                            return probe.format.name.indexOf(format) !== -1;
                        });

                        var areStreamsSupported = probe.streams.every(function(stream) {
                            if (stream.track === 'audio') {
                                return stream.channels <= options.maxAudioChannels &&
                                    options.audioCodecs.indexOf(stream.codec) !== -1;
                            } else if (stream.track === 'video') {
                                return options.videoCodecs.indexOf(stream.codec) !== -1;
                            }

                            return true;
                        });
                        var hasEmbeddedSubtitles = probe.streams.some(function(stream) {
                            return stream.track === 'subtitle';
                        });

                        // HTML5 video doesn't support multiple audio tracks, so we can't switch languages
                        var supportedAudioTracks = probe.streams.filter(function(stream) {
                            return stream.track === 'audio' && options.audioCodecs.indexOf(stream.codec) !== -1;
                        });

                        return isFormatSupported && areStreamsSupported && !hasEmbeddedSubtitles && supportedAudioTracks.length < 2;
                    })
                    .catch(function() {
                        // this uses content-type header in HTMLVideo which
                        // is unreliable, check can also fail due to CORS
                        return Video.canPlayStream(stream);
                    });
            });
    };

    VideoWithStreamingServer.manifest = {
        name: Video.manifest.name + 'WithStreamingServer',
        external: Video.manifest.external,
        props: Video.manifest.props.concat(['stream', 'videoParams'])
            .filter(function(value, index, array) { return array.indexOf(value) === index; }),
        commands: Video.manifest.commands.concat(['load', 'unload', 'destroy', 'addExtraSubtitlesTracks'])
            .filter(function(value, index, array) { return array.indexOf(value) === index; }),
        events: Video.manifest.events.concat(['propValue', 'propChanged', 'error'])
            .filter(function(value, index, array) { return array.indexOf(value) === index; })
    };

    return VideoWithStreamingServer;
}

module.exports = withStreamingServer;
