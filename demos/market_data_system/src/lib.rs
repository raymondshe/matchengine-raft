//! A separate market-data system: consume the engine's deterministic feed and
//! serve Binance-style WebSocket streams.
//!
//! The production design (`docs/MARKET_DATA_WEBSOCKET.md`) ingests Aeron
//! multicast. This reference implementation uses the engine's
//! `--market-data-log` JSONL feed as its demo transport; [`source::FrameSource`]
//! is the seam where an Aeron subscriber plugs in.

#[cfg(feature = "aeron")]
pub mod aeron_source;
pub mod archive;
pub mod config;
pub mod feed;
pub mod protocol;
pub mod server;
pub mod session;
pub mod source;

#[cfg(feature = "aeron")]
pub use aeron_source::AeronSource;
#[cfg(feature = "aeron")]
pub use aeron_source::aeron_source;
#[cfg(feature = "aeron")]
pub use aeron_source::aeron_source_with_requester;
pub use archive::archive_path;
pub use archive::archive_source;
pub use config::AeronSpec;
pub use config::Config;
pub use config::SymbolSpec;
pub use server::RunningService;
pub use server::start;
pub use server::start_with_snapshot_requesters;
pub use source::BoxedSource;
pub use source::FrameSource;
pub use source::SnapshotRequester;
pub use source::channel_source;
pub use source::jsonl_source;
