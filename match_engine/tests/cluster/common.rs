//! Shared helpers for the cluster integration tests.

use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Environment variable that shifts every integration-test port.
pub const PORT_OFFSET_ENV: &str = "MATCHENGINE_TEST_PORT_OFFSET";

/// Offset added to the HTTP base port and the derived gRPC port.
pub fn port_offset() -> u16 {
    match std::env::var(PORT_OFFSET_ENV) {
        Ok(value) => value.parse().unwrap_or_else(|_| {
            panic!("{PORT_OFFSET_ENV} must be a non-negative integer, got {value:?}")
        }),
        Err(_) => 0,
    }
}

fn shifted(port: u32) -> u16 {
    let port = port + u32::from(port_offset());
    u16::try_from(port)
        .unwrap_or_else(|_| panic!("test port {port} exceeds 65535; lower {PORT_OFFSET_ENV}"))
}

/// Returns the loopback HTTP address for a test's base port.
pub fn http_addr(base: u16) -> String {
    format!("127.0.0.1:{}", shifted(u32::from(base)))
}

/// Returns the derived loopback gRPC address for a test's base HTTP port.
pub fn raft_addr(base: u16) -> String {
    format!("127.0.0.1:{}", shifted(u32::from(base) + 1000))
}

/// Returns an HTTP URL for a test's base port.
pub fn http_url(base: u16) -> String {
    format!("http://{}", http_addr(base))
}

/// Returns a gRPC URL for a test's base HTTP port.
pub fn raft_url(base: u16) -> String {
    format!("http://{}", raft_addr(base))
}

/// A test's base HTTP port, rendered as an address or URL with the global
/// `MATCHENGINE_TEST_PORT_OFFSET` applied.
///
/// Tests keep one `const ADDR: TestAddr = TestAddr::new(21933)` and use it in
/// `format!`/`.to_string()` as before; the offset is applied at render time, so
/// two test processes can run concurrently without editing every call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestAddr(u16);

impl TestAddr {
    /// Creates a test address from its unshifted HTTP base port.
    pub const fn new(base: u16) -> Self {
        Self(base)
    }

    /// The shifted loopback HTTP address.
    pub fn http(self) -> String {
        http_addr(self.0)
    }

    /// The shifted loopback gRPC address.
    pub fn raft(self) -> String {
        raft_addr(self.0)
    }

    /// The shifted HTTP URL.
    pub fn http_url(self) -> String {
        http_url(self.0)
    }

    /// The shifted gRPC URL.
    pub fn raft_url(self) -> String {
        raft_url(self.0)
    }
}

impl fmt::Display for TestAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.http())
    }
}

impl From<TestAddr> for String {
    fn from(addr: TestAddr) -> Self {
        addr.http()
    }
}

/// A unique temporary directory that is removed on drop.
///
/// Tests that start real nodes must not share a data directory across runs:
/// persisted Raft membership would make `init` fail on the next run.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(label: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "matchengine-test-{label}-{}-{nanos}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
