# Storage

## Write-ahead log (`src/store/log_store.rs`)

Raft log entries are appended to `log/NNNNNNNNNN.wal` segment files:

- Each record is framed as `[len: u32 LE][crc32: u32 LE][postcard payload]`. On open, recovery walks
  every segment and truncates a torn or corrupt tail instead of parsing it as a valid entry.
- `append()` writes the whole openraft batch with a single `write_all` followed by a single
  `sync_data`, so N entries replicated in one AppendEntries RPC cost one fsync.
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
log id is appended to the WAL without its own fsync and becomes durable with the next append,
avoiding a second fsync per batch.

## Snapshots (`src/store/state_machine.rs`)

The state machine (order book + sequence counter + retained match-event window + submission dedupe
map) stays in memory so applying entries is fast. Snapshot v10 is written as
`snapshot/current.snap` using the same atomic temp-file/rename protocol; `install_snapshot`
validates the incoming bytes before replacing the durable copy, so a bad snapshot cannot destroy a
good one.

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
1. Recovers the WAL index, vote and purge marker (discarding a torn tail).
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
