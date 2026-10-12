# Vendored crates

## librqbit 8.1.1 (Apache-2.0, https://github.com/ikatson/rqbit)

The published crate, copied from the crates.io package as it is, with ONE
change. `[patch.crates-io]` in `crates/streaming-server/Cargo.toml` and in
`apps/desktop/src-tauri/Cargo.toml` points both workspaces at this copy.

### The change

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
