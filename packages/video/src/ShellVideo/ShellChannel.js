// The ONE owner of the shell player's IPC, per transport, for the app's
// lifetime. The shell has exactly one mpv, so the web side has exactly one
// subscription to it: every player visit ATTACHES to this channel and
// DETACHES from it, and never subscribes to the transport itself. A visit
// that never cleaned up can therefore not leave anything listening (the
// leak this replaces: each destroyed ShellVideo kept its listeners and its
// mpv observers, so after N visits every END_FILE drew N extra `stop`s and
// every property change arrived N times).
//
// mpv's properties describe the player, not a visit: the channel observes
// each one once, keeps its latest value, and hands those to a player when it
// attaches (what a fresh observer used to receive from mpv). The handshake a
// load waits on (mpv version, whether the shell serves `rillio://`) is asked
// once and answered for every load after it.
//
// Loads get their id here, and the shell echoes it on the END_FILE of the
// mpv playlist entry that loadfile created, so an END_FILE names its load.

// How long to wait for the shell's answer to the `rillio://` capability query
// before assuming "no". A shell that predates the query never answers at all,
// and an unanswered promise would park every `loadfile` forever (the exact way
// the mpv-version handshake once broke playback), so this ALWAYS settles.
var STREAM_CB_QUERY_TIMEOUT = 3000;

var OBSERVED_PROPS = [
    'path',
    'time-pos', 'volume', 'pause', 'seeking', 'eof-reached',
    'duration', 'metadata', 'video-params', 'track-list', 'chapter-list',
    'paused-for-cache', 'cache-buffering-state', 'demuxer-cache-time',
    'aid', 'vid', 'sid', 'sub-scale', 'sub-pos', 'sub-delay', 'audio-delay', 'speed',
    'mpv-version', 'ffmpeg-version',
];

function ShellChannel(ipc) {
    var attached = null;
    var latest = {};
    var latestOrder = [];
    var nextLoadId = 1;

    var resolveMPVVersion;
    var mpvVersion = new Promise(function(resolve) {
        resolveMPVVersion = resolve;
    });
    var resolveStreamCb;
    var streamCb = new Promise(function(resolve) {
        resolveStreamCb = resolve;
    });
    setTimeout(function() {
        resolveStreamCb(false);
    }, STREAM_CB_QUERY_TIMEOUT);

    ipc.on('rillio-stream-cb', function(args) {
        resolveStreamCb(!!(args && args.available));
    });
    ipc.on('mpv-prop-change', function(args) {
        if (args.name === 'mpv-version') {
            resolveMPVVersion(args.data);
        }
        if (!Object.prototype.hasOwnProperty.call(latest, args.name)) {
            latestOrder.push(args.name);
        }
        latest[args.name] = args;
        if (attached !== null) {
            attached.onPropChange(args);
        }
    });
    ipc.on('mpv-event-ended', function(args) {
        if (attached !== null) {
            attached.onEndFile(args);
        }
    });

    OBSERVED_PROPS.forEach(function(name) {
        ipc.send('mpv-observe-prop', name);
    });
    ipc.send('rillio-stream-cb-query');

    // [mpvVersion, streamCbAvailable], settled once for the channel's life.
    this.handshake = Promise.all([mpvVersion, streamCb]);

    // `player`: { onPropChange(args), onEndFile(args) }. One player at a time:
    // attaching another detaches the previous one. The current value of every
    // property is replayed to the newcomer before anything live reaches it.
    this.attach = function(player) {
        attached = player;
        latestOrder.forEach(function(name) {
            if (attached === player) {
                player.onPropChange(latest[name]);
            }
        });
    };
    this.detach = function(player) {
        if (attached === player) {
            attached = null;
        }
    };
    // Whether `player` is the current owner. A visit may drive the one shared
    // mpv (loadfile, stop, set-prop) ONLY while this is true: a superseded or
    // torn-down visit that still sent `stop` would halt the live visit's
    // playback on the shared instance (the cross-visit interference the
    // singleton ends, at the command layer, not just the event layer).
    this.isAttached = function(player) {
        return attached === player;
    };
    this.nextLoadId = function() {
        return nextLoadId++;
    };
}

// The transport's channel, created on first use. Keyed by the transport
// object, which the app keeps stable for its lifetime (useShell `transport`).
var channels = new WeakMap();
ShellChannel.of = function(ipc) {
    var channel = channels.get(ipc);
    if (!channel) {
        channel = new ShellChannel(ipc);
        channels.set(ipc, channel);
    }
    return channel;
};

module.exports = ShellChannel;
