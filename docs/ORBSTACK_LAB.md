# OrbStack Linux lab

`scripts/orbstack-cluster.sh` runs a four-container Linux Raft lab on
[OrbStack](https://orbstack.dev/) Free (personal, non-commercial use). It is
the container counterpart of `scripts/test-cluster.sh`: one container per node,
fixed IPs on a private bridge, and a one-call `/init` for all four voters.

This is intentionally not a production deployment. The four containers share
OrbStack's Linux kernel; they have separate network namespaces, filesystems and
volumes, but they are not four independent kernels. Use OrbStack machines, Lima
or Apple `container` when you need per-node kernels or kernel-level fault
injection.

## Prerequisites

- macOS 14 or later (Apple silicon recommended).
- OrbStack installed with `brew install --cask orbstack`.
- Start it once with `orb start`.
- A Docker CLI in `PATH`. OrbStack does not install one itself:
  `brew install docker docker-buildx docker-compose`.
- The Docker CLI must find the Homebrew plugins. Add this to
  `~/.docker/config.json` if it is not already present:

  ```json
  {
    "cliPluginsExtraDirs": ["/opt/homebrew/lib/docker/cli-plugins"]
  }
  ```

The lab assumes the free personal/non-commercial OrbStack license. Do not use
it for company work without an OrbStack Pro license.

## Quick start

```shell
./scripts/orbstack-cluster.sh start
./scripts/orbstack-cluster.sh status
```

The defaults are:

| Node | HTTP / gRPC |
|---|---|
| node 1 | `172.30.0.11:21001` / `172.30.0.11:22001` |
| node 2 | `172.30.0.12:21001` / `172.30.0.12:22001` |
| node 3 | `172.30.0.13:21001` / `172.30.0.13:22001` |
| node 4 | `172.30.0.14:21001` / `172.30.0.14:22001` |

Every container uses the same HTTP port because each has its own IP. OrbStack
makes those IPs reachable from macOS directly, so no `-p` publishing is needed.

`start` removes existing lab containers and node volumes, creates the
`172.30.0.0/24` bridge, starts the containers, initialises all four voters in
one `/init` call, and places one resting order. Use `--keep-data` to preserve
volumes across a restart.

```shell
./scripts/orbstack-cluster.sh start --no-fsync   # production no-fsync profile
./scripts/orbstack-cluster.sh start --nodes 3    # smaller lab
./scripts/orbstack-cluster.sh stop
./scripts/orbstack-cluster.sh stop --clean       # also remove volumes/network
```

## Network partitions

The runtime image includes `iptables` and the containers run with
`NET_ADMIN`. To isolate node 1 from nodes 2, 3 and 4:

```shell
./scripts/orbstack-cluster.sh partition 1 2 3 4
./scripts/orbstack-cluster.sh status
```

The isolated node keeps its old term until it hears from the new leader, while
the other three elect a new leader and keep quorum. `status` therefore prints
each node's state, term and leader view, and reports the leader from the
highest term.

Heal all nodes with:

```shell
./scripts/orbstack-cluster.sh heal
```

For a symmetric 2+2 split, run `partition` on both sides:

```shell
./scripts/orbstack-cluster.sh partition 1 3 4
./scripts/orbstack-cluster.sh partition 2 3 4
./scripts/orbstack-cluster.sh partition 3 1 2
./scripts/orbstack-cluster.sh partition 4 1 2
```

## Linux checks for TODO items

`scripts/orbstack-cluster.sh cargo ...` runs Cargo in a Linux builder image
with this repository mounted at `/src` and named volumes for the Cargo registry
and target directory. The macOS target directory is never used.

```shell
./scripts/orbstack-cluster.sh cargo test
./scripts/orbstack-cluster.sh cargo bench --bench apply_path -- --check
./scripts/orbstack-cluster.sh cargo bench --bench e2e -- --pin 0
./scripts/orbstack-cluster.sh cargo bench --bench cluster_e2e -- --rounds 1
```

`linux-check` runs the current Linux gate:

```shell
./scripts/orbstack-cluster.sh linux-check
```

It performs:

1. the full `cargo test` suite on Linux;
2. the async-WAL cluster tests with `--features async-wal`;
3. `apply_path --check` allocation ceilings;
4. the pinned-core `e2e` pass with `--pin 0`, once with WAL fsync disabled and once with
   every-append fsync.

On 2026-11-11 this passed on the OrbStack 7.2.12 aarch64 kernel: 220 lib + 3 bridge + 74 cluster
tests, 2 async-WAL tests, the allocation check, and both pinned e2e modes. The measured numbers
are in [`docs/PERFORMANCE.md`](./PERFORMANCE.md#linux-container-pinned-pass-2026-11-11). The Linux
`apply_path --check` first exposed a one-time 3-allocation blank baseline; `benches/apply_path.rs`
now warms up 64 blank entries before resetting the counter.

This covers the Linux build, the four-voter no-fsync tests, the allocator checks and the
pinned-core `e2e` surface. It does **not** satisfy the multi-host `local_lease` item: these
containers run on one host, one kernel and one clock. The Aeron external-driver/multicast
validation is a separate lab (`scripts/aeron-linux-lab.sh`) that uses a supervised media driver
and a loopback multicast group; see below.

## Aeron external-driver/multicast

`scripts/aeron-linux-lab.sh` runs the market-data transport checks in a separate
`matchengine-aeron-lab` container. It builds the engine with `--features market-data-aeron` and
the demo with `--features aeron`, including
`demos/market_data_system/src/bin/aeron_driver.rs` — a small supervised driver that exposes the
same Aeron C media driver as `aeronmd`.

```shell
./scripts/aeron-linux-lab.sh ipc
./scripts/aeron-linux-lab.sh multicast
./scripts/aeron-linux-lab.sh all
```

The container has a 1 GiB `/dev/shm` and `NET_ADMIN`; the multicast path uses
`239.1.2.3:24001` on `lo`, with one driver for the engine and one for each of two service
instances.

On 2026-11-11 both modes passed:

- IPC: a separate driver over `/dev/shm/aeron-ipc`, service ready `200`, `snapshot_requests=1`,
  and a populated `depthSnapshot` in ~0.06 s.
- Multicast: both services ready `200`, both metrics advanced on the same order flow, both depth
  clients received a populated snapshot, and killing one service left the other ready and
  receiving.

The first stream-2010 request can arrive before the request publication has connected; the source
now keeps it pending and retries instead of dropping it. This is a single-host loopback multicast
test. A real multi-host multicast fabric remains a deployment-network check.

`rusteron` 0.2.13 publishes precompiled Linux static libraries only for x86_64, so this lab runs
in an amd64 container under Rosetta. A native aarch64 build needs CMake >= 3.30 and a source build;
the OrbStack toolchain image currently ships CMake 3.25 from Debian.

## Image

`docker/linux.Dockerfile` builds:

- `matchengine-raft:linux` — Debian slim runtime with the release binary,
  `iptables`, `iproute2`, `curl` and `procps`;
- `matchengine-raft:builder` — the Rust toolchain and build dependencies used
  by `cargo` and `linux-check`.

Rebuild after source changes:

```shell
./scripts/orbstack-cluster.sh build
```

`start` builds `matchengine-raft:linux` automatically if it is missing. Pass
`--build` to force a rebuild or `--no-build` to fail instead of building.

## Troubleshooting

- `Docker is not ready; run 'orb start' first`: OrbStack is not running, or the
  Docker CLI is not using the `orbstack` context. Check `docker context ls`.
- `docker build` cannot resolve `docker.io`: retry; the first metadata fetch
  occasionally times out on a cold OrbStack network.
- A node fails to start: `./scripts/orbstack-cluster.sh logs 1` and
  `docker inspect matchengine-raft-node1`.
- A stale cluster from an earlier run: the default `start` removes volumes; use
  `--keep-data` only when you intentionally want a restart test.
- Port or IP conflict: pass `--network`, `--subnet`, `--base-ip` and
  `--base-port`; the base IP must be inside the subnet.
