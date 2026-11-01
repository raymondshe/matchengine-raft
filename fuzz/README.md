# Fuzz targets

This is a separate `cargo-fuzz` crate; it is intentionally not part of the main
package's workspace, so stable `cargo test` and `cargo clippy` do not compile
libFuzzer.

| Target | What it exercises |
|---|---|
| `order_book` | `OrderBook::execute_order` with structured arbitrary orders, including TIF, post-only, stops, icebergs and quote volume. Asserts sequence continuity and that the resting book never crosses. |
| `wal_decode` | `read_wal_bytes`: raw header/frame/CRC rejection, plus valid frames that reach the postcard `WalRecord` decoder. |
| `snapshot_decode` | `read_snapshot_bytes`: raw header/version/CRC rejection, valid frames that reach the postcard `SnapshotPayload` decoder, and direct `StateMachineData` decoding. |

Run them from the repository root (install once with `cargo install cargo-fuzz`; the
targets need a nightly toolchain):

```shell
cargo +nightly fuzz build
cargo +nightly fuzz run order_book
cargo +nightly fuzz run wal_decode
cargo +nightly fuzz run snapshot_decode
```

If `cargo +nightly` reports `no such command: +nightly`, `cargo` is not a rustup
proxy. Put the nightly toolchain first on `PATH` instead:

```shell
PATH="$(dirname "$(rustup which --toolchain nightly cargo)"):$PATH" cargo fuzz build
```

The scheduled `.github/workflows/fuzz.yml` job builds all three targets and runs
a short smoke pass on every target. Corpora and artifacts are generated under
`fuzz/corpus/` and `fuzz/artifacts/` and are gitignored.
