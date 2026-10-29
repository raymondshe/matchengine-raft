pub mod api;
pub mod grpc;
pub mod management;
pub mod raft;
pub mod raft_network_impl;
pub mod transport;

/// Ports for the Raft transport are derived from the public HTTP port so that
/// `BasicNode.addr` keeps one meaning (the API address) for every node.
pub const RAFT_PORT_OFFSET: u16 = 1000;

/// Maximum HTTP/JSON body accepted by the legacy Raft append RPC.
///
/// Matches the gRPC message cap; append batches are the only public Raft body
/// that routinely grows beyond a single order.
pub const MAX_HTTP_RAFT_BODY_BYTES: usize = 64 * 1024 * 1024;

/// Maximum snapshot size accepted by either transport.
///
/// The state machine still installs one contiguous buffer, so the receiver
/// enforces a hard total before allocating. This is large enough for the
/// retained event window and order book while bounding a hostile peer.
pub const MAX_SNAPSHOT_BYTES: usize = 256 * 1024 * 1024;

/// Which transport the node uses for outbound Raft RPCs.
///
/// The server always accepts both: the HTTP endpoints remain mounted on the API
/// listener, and the gRPC service listens on [`RAFT_PORT_OFFSET`] above it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
    /// HTTP/1.1 + JSON, one request per RPC.
    Http,
    /// gRPC over HTTP/2, with streaming AppendEntries and snapshots.
    #[default]
    Grpc,
}

impl std::fmt::Display for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Transport::Http => write!(f, "http"),
            Transport::Grpc => write!(f, "grpc"),
        }
    }
}

/// Derives the Raft transport address from the public API address.
///
/// Examples:
/// - `127.0.0.1:21001` -> `127.0.0.1:22001`
/// - `[::1]:21001` -> `[::1]:22001`
pub fn raft_addr_from_http(http_addr: &str) -> Result<String, String> {
    let (host, port) = split_host_port(http_addr)?;
    let port = port.checked_add(RAFT_PORT_OFFSET).ok_or_else(|| {
        format!("raft port for {http_addr} exceeds 65535 with offset {RAFT_PORT_OFFSET}")
    })?;
    Ok(format!("{host}:{port}"))
}

fn split_host_port(addr: &str) -> Result<(&str, u16), String> {
    if let Some(rest) = addr.strip_prefix('[') {
        // Bracketed IPv6, e.g. `[::1]:21001`.
        let end = rest
            .find(']')
            .ok_or_else(|| format!("invalid address `{addr}`: missing `]`"))?;
        let host = &addr[..end + 2];
        let port = rest[end + 1..]
            .strip_prefix(':')
            .ok_or_else(|| format!("invalid address `{addr}`: missing port"))?;
        let port = port
            .parse::<u16>()
            .map_err(|e| format!("invalid port in `{addr}`: {e}"))?;
        return Ok((host, port));
    }

    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| format!("invalid address `{addr}`: missing port"))?;
    if host.is_empty() {
        return Err(format!("invalid address `{addr}`: empty host"));
    }
    let port = port
        .parse::<u16>()
        .map_err(|e| format!("invalid port in `{addr}`: {e}"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_raft_addr_for_ipv4_and_hostnames() {
        assert_eq!(
            "127.0.0.1:22001",
            raft_addr_from_http("127.0.0.1:21001").unwrap()
        );
        assert_eq!(
            "localhost:21000",
            raft_addr_from_http("localhost:20000").unwrap()
        );
    }

    #[test]
    fn derives_raft_addr_for_ipv6() {
        assert_eq!("[::1]:22001", raft_addr_from_http("[::1]:21001").unwrap());
    }

    #[test]
    fn rejects_invalid_addresses() {
        assert!(raft_addr_from_http("127.0.0.1").is_err());
        assert!(raft_addr_from_http(":21001").is_err());
        assert!(raft_addr_from_http("[::1:21001").is_err());
        assert!(raft_addr_from_http("127.0.0.1:65535").is_err());
    }
}
