# Storage

## Write-ahead log (`src/store/log_store.rs`)

Raft log entries are appended to `log/NNNNNNNNNN.wal` segment files:

- Each record is framed as `[len: u32 LE][crc32: u32 LE][postcard payload]`. On open, recovery walks
  every segment and truncates a torn or corrupt tail instead of parsing it as a valid entry; it
  also rejects an index gap, because openraft requires a hole-free log.
- `append()` encodes the openraft batch into byte-bounded chunks
  (`StorageConfig::max_wal_append_bytes`, default 8 MiB) and writes them under one disk lock with
  one `sync_data`, so N entries replicated in one AppendEntries RPC cost one fsync. Entries are
  never split; one entry larger than the cap is written alone. A failed chunk rolls the active
  segment back and removes any segment the failed batch created, so recovery never sees a partial
  batch. `0` disables chunking.
- Reads use `pread` (`read_at`) against cached file handles, so replication can read while an
  append is in flight and no lock is held during disk IO.
- All blocking IO runs on `spawn_blocking`; the async runtime only encodes and decodes.
- Segments are 64 MiB by default and are deleted only when a purge fully covers them. The active
  segment is rotated before deletion, so a purge never leaves a hole.
- Each segment starts with an `MRFT` magic and a `u32` format version. `LogStore` implements
  `RaftLogStorage` (`get_log_state`, `read_vote`/`save_vote`, `read_committed`/`save_committed`,
  `append`, `truncate_after`, `purge`, `get_log_reader`).

Vote and the last purged log id are safety-critical and rare, so they live in a small `log/state`
file that is rewritten atomically (temp file → fsync → rename → directory fsync). The committed
log id is appended to the WAL without its own fsync: every-append mode makes it durable with the
next append, periodic mode on the next tick, and no-fsync mode not at all. This avoids a second
fsync per batch.

### WAL append byte cap

OpenRaft limits a batch by entry count (`max_payload_entries = 300`), not bytes. The log store also
caps each encoded write:

- `--max-wal-append-bytes` (env `MATCHENGINE_MAX_WAL_APPEND_BYTES`) defaults to
  `DEFAULT_MAX_WAL_APPEND_BYTES` (8 MiB); `0` writes the whole batch in one call.
- A larger apply batch is split into whole-record chunks. Chunking holds the disk lock and performs
  one fsync for the batch; rotation may happen between chunks, and each rotated segment is synced
  before the switch when fsync is enabled.
- A partial write or fsync failure truncates the pre-batch segment back to its original length and
  deletes segments created by the failed batch. The index is only updated after every chunk is
  written, so a failed append leaves no visible entries.
- `--max-wal-append-bytes` does not split an individual entry; one oversized entry may exceed the
  bound and is written alone.

### WAL durability and recovery from peers

WAL fsync is controlled by `RAFT_WAL_FSYNC_MS`:

- unset: no fsync (the default).
- `0`: fsync every append batch.
- `N > 0`: a Tokio `wal-fsync` task fsyncs the active segment every `N` ms. It clones the active
  file handle under a short lock and runs `sync_data` on the clone in `spawn_blocking`, so appends
  do not wait on the disk lock during the fsync.

Only every-append mode satisfies `RaftLogStorage::append`'s contract fully: entries are persisted
when the callback runs, and the log has no holes. With no fsync or periodic fsync, the callback can
fire after the page-cache write, so a crash can lose the writes since the last fsync (the last
`N` ms in periodic mode).

If a node crashes in those modes, its WAL may be shorter than the cluster's committed log. Raft
handles a **valid shorter log**:

- The leader replicates the missing entries, or installs a snapshot if the node is too far behind.
- Election restriction prevents a node with a shorter log from winning against a majority that has
  the committed entries.
- `/ready` waits for `wait_for_recovery`, which confirms the cluster commit covers the node's
  durable log tail before reads are served.
- Vote and purge state stay fsynced even when WAL fsync is disabled or periodic, so the node cannot
  vote twice in one term.

A crash can also leave a torn record in a non-last segment. When fsync is not every-append,
`LogStore::open` uses best-effort recovery: it truncates at the first corrupt record, index gap or
segment, deletes later segments, and returns the longest valid prefix. The node then catches up from
peers. This can discard entries that only this node had; if no peer has them, they are unrecoverable.
In every-append mode the same corruption is a fatal open error and the node refuses to start.

For production, choose `RAFT_WAL_FSYNC_MS=0` for durable writes, or leave it unset with at least
four voters for maximum throughput. The 100 ms periodic mode measured within noise of disabled but
is still lossy and is not guarded by the four-voter floor; use it only with four voters and an
accepted 100 ms loss window. The 10 ms mode is dominated (7.4k sequential writes/s and a 2.2-3.0 ms
p99), so it is not a production choice. See the production configuration section in
[`PERFORMANCE.md`](./PERFORMANCE.md#production-configuration-decisions-2026-11-08).

When `RAFT_WAL_FSYNC_MS` is unset, the binary also requires at least **four voters**: `/init` and
`/change-membership` reject smaller voter sets, and `/write`/`/write_batch` return `503` until four
voters exist. Four voters put a committed entry on at least three nodes, so one node losing its
unsynced tail still leaves at least two copies. Bootstrap such a cluster by starting four nodes and
calling `/init` with all four `[id, addr]` pairs; the single-node `[]` form is rejected. The demo
scripts default to `RAFT_WAL_FSYNC_MS=0` so their three-node and single-node setups remain valid.

### Async WAL prototype (`async-wal` feature)

The async WAL path is **off by default** and requires a binary built with `--features async-wal`
plus `--async-wal` (env `MATCHENGINE_ASYNC_WAL`) and `RAFT_WAL_FSYNC_MS=0`:

- `append` writes the batch and updates the visible log, but defers `sync_data` to a Tokio
  `async-wal` worker. Openraft's `IOFlushed` callback fires only after the fsync.
- Entries pending durability are served from an in-memory pending cache, so
  `try_get_log_entries` remains correct before the callback.
- `truncate_after` and `purge` wait for pending appends to flush before changing the log.
- A crash before `IOFlushed` can lose the unsynced tail. Those entries were not acknowledged as
  durable, and recovery keeps the valid prefix; `tests/cluster/test_storage.rs` and
  `tests/cluster/test_async_wal.rs` cover the callback gate, lost-tail recovery and a Raft
  commit/reopen path.
- Only every-append mode is supported by the prototype; periodic and no-fsync modes keep their
  existing behavior.
- The pending cache is unbounded while fsyncs lag behind appends; productionizing needs a cap or
  backpressure so a slow disk cannot grow memory without limit.

The prototype is not the default. Productionizing it still needs a Raft-level A/B, periodic-mode
support if desired, and broader crash/failover coverage.

## Snapshots (`src/store/state_machine.rs`)

The state machine (order book + sequence counter + retained match-event window + submission dedupe
map) stays in memory so applying entries is fast. Snapshot v11 is written as
`snapshot/current.snap` using the same atomic temp-file/rename protocol; `install_snapshot`
validates the incoming bytes before replacing the durable copy, so a bad snapshot cannot destroy a
good one.

A built or cached snapshot is held as `Arc<[u8]>` (`StoredSnapshot::data`,
`SnapshotData::Memory`), so `build_snapshot` and `get_current_snapshot` share the encoded body
instead of copying it. The gRPC receiver does not accumulate the body in memory: it streams chunks
into a `snapshot-staging-*` file under the snapshot directory, tracking the total length against
`MAX_SNAPSHOT_BYTES` and a running CRC32. `install_snapshot` reads the staged file, verifies its
length and CRC, decodes the postcard body, and only then writes the durable snapshot. A staging
file is removed on success, on rejection, or when the value is dropped; startup also deletes stale
`snapshot-staging-*` files. A completed install stores the body back as `Arc<[u8]>`.

The snapshot payload is postcard-encoded `StateMachineData`:

```rust
pub struct StateMachineData {
    pub orders: Vec<Order>,
    pub sequence: u64,
    pub stops: Vec<Order>,
    pub last_trade_price: Option<Fixed>,
    pub symbol_config: SymbolConfig, // the one symbol this process/cluster trades
    pub symbol_locked: bool,         // true after the first accepted order
    pub events: Vec<StoredMatchEvent>, // retained match events, oldest first
    pub events_evicted: u64,
    pub submissions: Vec<SubmissionRecord>, // client_order_id dedupe, oldest first
    pub submissions_evicted: u64,
}
```

`StoredMatchEvent` mirrors `MatchEvent` but always writes both optional fields; `MatchEvent` omits
absent options from JSON, which postcard cannot decode positionally. Event identity stays
`(log_index, ordinal)`, so no extra counter is stored. The file framing is:

```text
"MRFSNAP" | version u32 | crc32 u32 | postcard(SnapshotPayload)
```

## Per-node data directory

Each node writes to `<data-dir>/<instance-prefix>-<node-id>/{log,snapshot}`. Configure it with:

```bash
./target/release/matchengine-raft --id 1 --http-addr 127.0.0.1:21001 \
    --data-dir /var/lib/matchengine-raft --instance-prefix prod
```

The equivalent environment variables are `RAFT_DATA_DIR` and `RAFT_INSTANCE_PREFIX`; the defaults
are `/tmp/matchengine-raft` and `matchengine`.

## Snapshot configuration

`default_raft_config()` (`src/lib.rs`) sets `snapshot_policy = LogsSinceLast(100_000)` and the
node also runs a 30-minute snapshot timer (`StorageConfig::snapshot_interval`, default
`DEFAULT_SNAPSHOT_INTERVAL`). The timer only requests a snapshot when new entries have been
applied since the last one, so an idle node does not rewrite the same file. `--snapshot-interval-ms`
(env `SNAPSHOT_INTERVAL_MS`) overrides the interval; `0` disables the timer while leaving the
100,000-entry policy active. Tests that need a snapshot quickly override the policy:

```rust
let mut config = default_raft_config();
config.snapshot_policy = SnapshotPolicy::LogsSinceLast(10);
config.max_in_snapshot_log_to_keep = 0; // purge everything the snapshot covers
```

- A snapshot is built after the configured number of applied log entries (100,000 by default),
  or after 30 minutes when newer entries exist.
- Logs covered by a snapshot plus `max_in_snapshot_log_to_keep` are purged. The snapshot test sets
  both values so a joining learner cannot catch up by log replication and must install a snapshot.

## Data recovery

On startup the node:
1. Recovers the WAL index, vote and purge marker (discarding a torn tail; when fsync is not
   every-append it truncates at the first corrupt segment and lets Raft catch up).
2. Selects and loads `snapshot/current.snap` if it exists. The file is replaced atomically, so it
   is always the latest complete snapshot; a crash leaves only an ignored `.tmp` file.
3. Sets the state machine's `last_applied` from the snapshot metadata and lets openraft replay
   retained WAL entries whose log index is greater than the snapshot's last log id.

A single-node cluster therefore comes back with all live orders, the sequence counter, the retained
event window and the idempotency map, whether recovery is pure log replay or snapshot + newer WAL
tail. Both paths are covered by `tests/cluster/test_restart.rs`.

## Dumping durable state

The storage examples are read-only and validate the WAL/snapshot CRCs:

```shell
cargo run --example dump_wal -- <node-dir|log-dir|segment.wal>
cargo run --example dump_snapshot -- <node-dir|snapshot-dir|current.snap>
```

`dump_wal` prints one JSON object per WAL record (`entry` or `committed`) with file and offset;
`dump_snapshot` prints a metadata line, a state line, then one line per order, stop, event or
submission. Both write JSONL to stdout, so they compose with `jq` and other line-oriented tools.
