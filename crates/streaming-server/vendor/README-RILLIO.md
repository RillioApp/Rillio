# Vendored crates

## librqbit 8.1.1 (Apache-2.0, https://github.com/ikatson/rqbit)

The published crate, copied from the crates.io package as it is, with TWO
changes. `[patch.crates-io]` in `crates/streaming-server/Cargo.toml` and in
`apps/desktop/src-tauri/Cargo.toml` points both workspaces at this copy.

### Change 2: outgoing dials do not issue `connect()` from a runtime worker

`src/stream_connect.rs`, `StreamConnector::connect` now goes through
`dial_off_the_workers`: the first poll of tokio's connect future, the one that
issues the non-blocking `connect()` system call, runs on the blocking pool
(bounded by `DIAL_SLOTS`, a quarter of the pool); the future is then awaited
on the worker as before. `src/lib.rs` re-exports `stream_connect::test_hooks`
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

`tests/held_connect.rs` fails (a timer 48 s late on a two-worker runtime) if
the call goes back onto the workers; `tests/live_dial.rs` pins that a dial
still reaches a listening peer with a handshake.

What this does NOT do, and what was tried (same machine, same day, the
service running): a thread inside the held call cannot be got out of it.
`CancelSynchronousIo`, `CancelIoEx` and closing the socket from another thread
all left it held its full 20 s. An overlapped `ConnectEx` is held exactly the
same (20,004 to 20,016 ms before it returns "pending", on a completion port,
with an event, blocking or non-blocking socket), so switching the call buys
nothing; a build that did so was measured and reverted. And a process does not
finish exiting while it has threads in the call (8 held dials: gone 20,088 ms
after `ExitProcess`). So with such a filter the dials still occupy pool
threads (at most `DIAL_SLOTS`) and the process can still take up to 20 s to
disappear after it exits; what the change guarantees is that the runtime's
workers are never among the held threads.

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
