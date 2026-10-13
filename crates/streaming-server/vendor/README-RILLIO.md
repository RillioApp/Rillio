# Vendored crates

## librqbit 8.1.1 (Apache-2.0, https://github.com/ikatson/rqbit)

The published crate, copied from the crates.io package as it is, with TWO
changes. `[patch.crates-io]` in `crates/streaming-server/Cargo.toml` and in
`apps/desktop/src-tauri/Cargo.toml` points both workspaces at this copy.

### Change 2: outgoing dials do not issue `connect()` from a runtime worker

`src/stream_connect.rs`, `StreamConnector::connect` now goes through
`dial_off_the_workers`, bounded by `DIAL_SLOTS` (half the blocking pool):

- on Windows the dial is an overlapped `ConnectEx`, awaited on a blocking-pool
  thread in an ordinary wait that the caller's timeout ends (`mod overlapped`);
- elsewhere it is tokio's own connect, with its first poll (the one that
  issues the non-blocking `connect()` call) run on the blocking pool.

`Cargo.toml` gains `socket2` and `windows-sys` for Windows (both already in
the tree through tokio). `src/lib.rs` re-exports `stream_connect::test_hooks`
as `rillio_test_hooks` (hidden), which lets a test hold every dial the way a
filter does.

Why: a kernel socket filter can hold that call. Measured on a user's machine
(2026-10-13, AdGuard's socket driver `adgnetworksockdrv` with its service
running): 20,007 ms for an address nobody answers, then `WSAEWOULDBLOCK`;
0.1 ms with the service stopped. tokio issues the call from whichever worker
polls the future, a live torrent dials up to 128 peers, most of them dead, so
all 28 workers of the runtime were parked inside the kernel
(`Wait:Executive`, CPU near zero). `/heartbeat` took up to 10 s or never
answered, and in the desktop shell, where the engine shared Tauri's runtime,
every window command waited too.

Why `ConnectEx` on Windows and not just "off the workers": a thread inside the
held call cannot be got out of it (`CancelSynchronousIo`, `CancelIoEx` and
closing the socket from another thread all left it held the full 20 s), and a
process does not finish exiting while it has such threads (8 held dials: gone
20,088 ms after `ExitProcess`), which is how a closed Rillio stayed alive,
holding its port and its WebView2 profile, when the next one was launched.
The same filter does not hold `ConnectEx`: 1.2 ms to start a dial, and a
process with 16 in flight exits at once (all measured 2026-10-13).

`tests/held_connect.rs` fails (a timer 48 s late on a two-worker runtime) if
the call goes back onto the workers; `tests/live_dial.rs` pins that a dial
still reaches a listening peer with a handshake.

### Change 1: the selection deadlock

`src/torrent_state/live/mod.rs`, `TorrentStateLive::update_only_files`: the
live-state write lock is released before the peer table is walked.

Upstream held the lock across `reconnect_all_not_needed_peers` (a DashMap
`iter_mut`, which takes every shard's write lock in turn). Peer handlers take
the same two locks in the opposite order: `PeerHandler::on_peer_died` holds the
peer's table entry and then calls `is_finished_and_no_active_streams`, which
takes the live-state lock. A file-selection change racing a dying peer
connection deadlocked, permanently, with the torrent's state lock held: every
later request for that torrent's state blocked behind it.

In Rillio a selection change is "play another episode of a season pack" or the
next-episode preload, and connections die constantly in a real swarm, so this
was reachable in ordinary use (since v0.1.45, the first release to change the
selection of a live torrent). It surfaced as a hung CI test on Linux; the
stacks of the two threads and the regression test are in
`tests/selection_vs_peer_churn.rs`.

### Keeping it honest

- Do not edit anything else in this copy. If another fix is needed, add it to
  this file with the same care.
- When upgrading librqbit, check whether upstream still holds the lock there.
  If it does not, drop the vendored copy and the two `[patch]` sections.
- `tests/selection_vs_peer_churn.rs` fails within a minute if the lock order
  comes back.
