# Raft transport: gRPC

Status: **gRPC only.** The HTTP/JSON Raft transport and the `--transport` switch were removed on
2026-10-30. The HTTP listener remains dedicated to the application and management API; peer-to-peer
Raft RPCs use gRPC on the same host at `http_addr` port + 1000.

Date: 2026-10-30 · Baseline: `openraft` 0.10.0-alpha.37, `tonic` gRPC, segmented WAL + atomic
snapshots.

## Current implementation

- `AppendEntries` and `Vote` are unary gRPC calls with `RPCOption::soft_ttl()` deadlines.
- `AppendStream` is a bidirectional stream used by openraft's `stream_append`, so followers can
  pipeline appends without a round trip per entry.
- `InstallSnapshot` is client-streaming; snapshots are split into 256 KiB chunks and the receiver
  enforces a 256 MiB total cap before allocating.
- Each peer gets separate replication, heartbeat and snapshot connections, so a large snapshot
  cannot head-of-line-block a vote.
- Both gRPC directions are capped at 64 MiB (`MAX_MESSAGE_SIZE`).
- `Unavailable` maps to `RPCError::Unreachable` (backoff), `DeadlineExceeded` or elapsed soft TTL
  to `RPCError::Timeout`, and other statuses to `RPCError::Network`.
- The transport is plaintext by design: this is an internal service. TLS and client authentication
  terminate at the gateway; see the deployment boundary in the README.

## Why gRPC

gRPC provides HTTP/2 multiplexing, native streaming for appends and snapshots, per-peer channels
and standard deadlines without a custom framing layer. The previous HTTP/JSON fallback has been
removed; keeping one transport simplifies the network code, tests and operational surface.

Aeron remains a future investigation only if a measured transport bottleneck justifies a second
transport implementation; see `docs/AERON_SOLUTION.md`.
