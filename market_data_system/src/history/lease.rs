//! Kubernetes `Lease` coordination for the single historical writer.
//!
//! The writer Deployment runs one replica, but a partitioned old pod is still
//! possible. The Lease is therefore advisory: this module only gates writes —
//! the durable cursor in `bar_state` and each bar row is what makes takeover
//! safe. A writer that loses the Lease stops flushing and stops advancing its
//! cursor, while it keeps consuming and spooling trades so it can resume if it
//! reacquires the Lease before a replacement pod does.
//!
//! Two forms of time are deliberately separate. `renewTime` is a Kubernetes
//! RFC3339 timestamp written to the Lease; expiry is decided from that value
//! and the Lease's `leaseDurationSeconds`, never from a bar timestamp. Bars
//! remain a pure function of the trade stream.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use reqwest::Client;
use reqwest::Method;
use reqwest::StatusCode;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

const SERVICE_ACCOUNT_DIR: &str = "/var/run/secrets/kubernetes.io/serviceaccount";
const LEASE_API_PATH: &str = "/apis/coordination.k8s.io/v1/namespaces";

/// Runtime configuration. `endpoint` and the file paths exist so tests and
/// out-of-cluster development can point at a mock API; production uses the
/// in-cluster service account and `KUBERNETES_SERVICE_HOST`.
#[derive(Clone, Debug)]
pub struct LeaseConfig {
    pub name: String,
    pub namespace: String,
    /// Stable identity of this writer (pod name in the Deployment).
    pub identity: String,
    pub ttl_secs: u64,
    pub renew_ms: u64,
    pub endpoint: Option<String>,
    pub token_file: Option<String>,
    pub ca_file: Option<String>,
}

impl LeaseConfig {
    /// Reads namespace/identity from the environment or the mounted service
    /// account. An explicit `identity` is required so two writers never share
    /// one by accident.
    pub fn from_env(
        name: impl Into<String>,
        identity: impl Into<String>,
        ttl_secs: u64,
        renew_ms: u64,
    ) -> io::Result<Self> {
        let namespace = std::env::var("KUBERNETES_NAMESPACE")
            .ok()
            .filter(|value| !value.is_empty())
            .or_else(|| {
                read_trimmed(&format!("{SERVICE_ACCOUNT_DIR}/namespace"))
                    .ok()
                    .filter(|value| !value.is_empty())
            })
            .unwrap_or_else(|| "default".to_string());
        Ok(Self {
            name: name.into(),
            namespace,
            identity: identity.into(),
            ttl_secs: ttl_secs.max(5),
            renew_ms: renew_ms.max(1_000),
            endpoint: None,
            token_file: None,
            ca_file: None,
        })
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LeaseSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_duration_seconds: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acquire_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub renew_time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lease_transitions: Option<i32>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LeaseMeta {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub namespace: String,
    #[serde(default, rename = "resourceVersion")]
    pub resource_version: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LeaseResource {
    #[serde(default)]
    pub api_version: String,
    #[serde(default, rename = "kind")]
    pub kind: String,
    #[serde(default)]
    pub metadata: LeaseMeta,
    #[serde(default)]
    pub spec: LeaseSpec,
}

/// True when `identity` may take the Lease at `now_ms`. A live holder always
/// wins; a missing holder, an absent duration or an expired renew time do not.
pub fn can_take_over(spec: &LeaseSpec, identity: &str, now_ms: u64) -> bool {
    match spec.holder_identity.as_deref() {
        None | Some("") => true,
        Some(holder) if holder == identity => true,
        Some(_) => {
            let ttl_ms = spec
                .lease_duration_seconds
                .unwrap_or(0)
                .max(0)
                .saturating_mul(1_000) as u64;
            if ttl_ms == 0 {
                return true;
            }
            let renewed = spec
                .renew_time
                .as_deref()
                .and_then(parse_rfc3339_ms)
                .unwrap_or(0);
            now_ms.saturating_sub(renewed) >= ttl_ms
        }
    }
}

/// Minimal Kubernetes API client for one named Lease.
pub struct LeaseClient {
    http: Client,
    resource_url: String,
    collection_url: String,
    token: Option<String>,
    identity: String,
    ttl_secs: u64,
}

impl LeaseClient {
    pub fn new(config: LeaseConfig) -> io::Result<Self> {
        let base = match config.endpoint.as_deref() {
            Some(endpoint) if !endpoint.is_empty() => endpoint.trim_end_matches('/').to_string(),
            Some(_) => return Err(io::Error::other("empty history lease endpoint")),
            None => {
                let host = std::env::var("KUBERNETES_SERVICE_HOST").map_err(|_| {
                    io::Error::other("history lease requires in-cluster Kubernetes")
                })?;
                let port =
                    std::env::var("KUBERNETES_SERVICE_PORT").unwrap_or_else(|_| "443".to_string());
                format!("https://{host}:{port}")
            }
        };
        let token_path = config
            .token_file
            .unwrap_or_else(|| format!("{SERVICE_ACCOUNT_DIR}/token"));
        let token = match read_trimmed(&token_path) {
            Ok(token) if !token.is_empty() => Some(token),
            Ok(_) => None,
            // A mock/out-of-cluster endpoint may be unauthenticated.
            Err(_) if config.endpoint.is_some() => None,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "history lease service account token: {error}"
                )));
            }
        };
        let mut builder = Client::builder().timeout(Duration::from_secs(10));
        if base.starts_with("https://") {
            let ca_path = config
                .ca_file
                .unwrap_or_else(|| format!("{SERVICE_ACCOUNT_DIR}/ca.crt"));
            if let Ok(pem) = std::fs::read(&ca_path)
                && let Ok(certificate) = reqwest::Certificate::from_pem(&pem)
            {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let http = builder
            .build()
            .map_err(|error| io::Error::other(format!("history lease HTTP client: {error}")))?;
        let namespace = config.namespace;
        let name = config.name;
        let collection_url = format!("{base}{LEASE_API_PATH}/{namespace}/leases");
        let resource_url = format!("{collection_url}/{name}");
        Ok(Self {
            http,
            resource_url,
            collection_url,
            token,
            identity: config.identity,
            ttl_secs: config.ttl_secs.max(5),
        })
    }

    fn request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        let mut request = self.http.request(method, url);
        if let Some(token) = &self.token {
            request = request.bearer_auth(token);
        }
        request
    }

    async fn read(&self) -> io::Result<Option<LeaseResource>> {
        let response = self
            .request(Method::GET, &self.resource_url)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("history lease read: {error}")))?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let (status, body) = response_text(response).await?;
        if !status.is_success() {
            return Err(io::Error::other(format!(
                "history lease read {status}: {}",
                truncate(&body)
            )));
        }
        if body.trim().is_empty() {
            return Ok(None);
        }
        let lease = serde_json::from_str(&body)
            .map_err(|error| io::Error::other(format!("history lease decode: {error}")))?;
        Ok(Some(lease))
    }

    /// Returns `None` on a 409 conflict so the caller can retry its read/modify
    /// /write cycle; other errors are fatal for this acquisition attempt.
    async fn write(&self, method: Method, url: &str, lease: &LeaseResource) -> io::Result<bool> {
        let response = self
            .request(method, url)
            .json(lease)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("history lease write: {error}")))?;
        let status = response.status();
        if status == StatusCode::CONFLICT {
            return Ok(false);
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(io::Error::other(format!(
                "history lease write {status}: {}",
                truncate(&body)
            )));
        }
        Ok(true)
    }

    fn blank_lease(&self) -> LeaseResource {
        LeaseResource {
            api_version: "coordination.k8s.io/v1".to_string(),
            kind: "Lease".to_string(),
            metadata: LeaseMeta {
                name: self
                    .resource_url
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                ..LeaseMeta::default()
            },
            spec: LeaseSpec::default(),
        }
    }

    /// Acquires the Lease, or renews it if this writer already owns it. A
    /// live holder belonging to someone else returns `false` without writing.
    pub async fn acquire(&self) -> io::Result<bool> {
        for _ in 0..3 {
            match self.read().await? {
                None => {
                    let mut lease = self.blank_lease();
                    lease.spec.lease_duration_seconds = Some(self.ttl_secs as i64);
                    if self
                        .write(Method::POST, &self.collection_url, &lease)
                        .await?
                    {
                        // Fill in identity with a follow-up update. A create
                        // with no holder is safe: expiry treats it as free.
                        if let Some(created) = self.read().await? {
                            return self.take(&created).await;
                        }
                        return Ok(true);
                    }
                }
                Some(lease) => {
                    if lease.spec.holder_identity.as_deref() == Some(self.identity.as_str()) {
                        let mut next = lease;
                        next.spec.renew_time = Some(format_rfc3339_ms(now_ms()));
                        next.spec.lease_duration_seconds = Some(self.ttl_secs as i64);
                        if self.write(Method::PUT, &self.resource_url, &next).await? {
                            return Ok(true);
                        }
                        continue;
                    }
                    if !can_take_over(&lease.spec, &self.identity, now_ms()) {
                        return Ok(false);
                    }
                    return self.take(&lease).await;
                }
            }
        }
        Ok(false)
    }

    async fn take(&self, lease: &LeaseResource) -> io::Result<bool> {
        let mut next = lease.clone();
        let now = format_rfc3339_ms(now_ms());
        next.spec.holder_identity = Some(self.identity.clone());
        next.spec.lease_duration_seconds = Some(self.ttl_secs as i64);
        next.spec.acquire_time = Some(now.clone());
        next.spec.renew_time = Some(now);
        next.spec.lease_transitions = Some(lease.spec.lease_transitions.unwrap_or(0) + 1);
        self.write(Method::PUT, &self.resource_url, &next).await
    }

    /// Drops the holder on a clean shutdown so a replacement need not wait for
    /// the TTL. A failed release is harmless: the next writer waits for expiry.
    pub async fn release(&self) {
        let Ok(Some(mut lease)) = self.read().await else {
            return;
        };
        if lease.spec.holder_identity.as_deref() != Some(self.identity.as_str()) {
            return;
        }
        lease.spec.holder_identity = None;
        lease.spec.renew_time = Some(format_rfc3339_ms(now_ms()));
        let _ = self.write(Method::PUT, &self.resource_url, &lease).await;
    }
}

/// DNS-1123-safe suffix for a per-symbol Lease name.
pub fn lease_symbol(symbol: &str) -> String {
    symbol.to_ascii_lowercase().replace('_', "-")
}

/// One Lease per symbol. A process that serves several symbols owns each one
/// independently: losing `eurusd` pauses only that symbol's writes, while its
/// other symbols keep their writers. The writer reads the shared flag before
/// every flush and every `bar_state` write; the renew task is its only writer.
pub struct LeaseManager {
    flags: HashMap<String, Arc<AtomicBool>>,
    stop: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl LeaseManager {
    /// Acquires a Lease for every symbol and starts the renewal loop. A Lease
    /// currently held by a live writer leaves that symbol paused; it is picked
    /// up on a later renewal once the old holder expires or releases it.
    pub async fn acquire_all(
        config: &LeaseConfig,
        symbols: &[String],
        mut stop: watch::Receiver<bool>,
    ) -> io::Result<Self> {
        let mut clients = Vec::with_capacity(symbols.len());
        let mut flags = HashMap::with_capacity(symbols.len());
        for symbol in symbols {
            let mut symbol_config = config.clone();
            symbol_config.name = format!("{}-{}", config.name, lease_symbol(symbol));
            let client = LeaseClient::new(symbol_config)?;
            let acquired = client.acquire().await?;
            if !acquired {
                tracing::warn!(%symbol, "history lease held by another writer; symbol paused");
            }
            let flag = Arc::new(AtomicBool::new(acquired));
            flags.insert(symbol.clone(), flag.clone());
            clients.push((symbol.clone(), client, flag));
        }
        let (stop_tx, mut stop_rx) = watch::channel(false);
        let renew_ms = config.renew_ms;
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(renew_ms.max(1_000)));
            tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
            tick.tick().await; // consume the immediate tick
            loop {
                tokio::select! {
                    _ = tick.tick() => {
                        for (symbol, client, flag) in &clients {
                            match client.acquire().await {
                                Ok(true) => { flag.store(true, Ordering::Release); }
                                Ok(false) => {
                                    if flag.swap(false, Ordering::AcqRel) {
                                        tracing::warn!(%symbol, "history lease lost; writer paused");
                                    }
                                }
                                Err(error) => {
                                    flag.store(false, Ordering::Release);
                                    tracing::warn!(%symbol, %error, "history lease renewal failed; writer paused");
                                }
                            }
                        }
                    }
                    result = stop_rx.changed() => {
                        if result.is_err() || *stop_rx.borrow() {
                            break;
                        }
                    }
                    _ = stop.changed() => {
                        if *stop.borrow_and_update() {
                            break;
                        }
                    }
                }
            }
            for (symbol, client, flag) in &clients {
                if flag.load(Ordering::Acquire) {
                    tracing::info!(%symbol, "releasing history lease");
                    client.release().await;
                }
            }
        });
        Ok(Self {
            flags,
            stop: stop_tx,
            task,
        })
    }

    pub fn owns(&self, symbol: &str) -> Arc<AtomicBool> {
        self.flags
            .get(symbol)
            .cloned()
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)))
    }

    pub fn any_owned(&self) -> bool {
        self.flags.values().any(|flag| flag.load(Ordering::Acquire))
    }

    pub async fn shutdown(self) {
        let _ = self.stop.send(true);
        let _ = self.task.await;
    }
}

fn read_trimmed(path: &str) -> io::Result<String> {
    std::fs::read_to_string(path).map(|value| value.trim().to_string())
}

fn truncate(text: &str) -> String {
    text.chars().take(200).collect()
}

async fn response_text(response: reqwest::Response) -> io::Result<(StatusCode, String)> {
    let status = response.status();
    let body = response
        .text()
        .await
        .map_err(|error| io::Error::other(format!("history lease response: {error}")))?;
    Ok((status, body))
}

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let shifted = if month > 2 {
        month as i64 - 3
    } else {
        month as i64 + 9
    };
    let doy = (153 * shifted + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if month <= 2 { year + 1 } else { year },
        month as u32,
        day as u32,
    )
}

/// Parses the RFC3339 subset emitted by Kubernetes, including fractional
/// seconds and numeric offsets, into epoch milliseconds.
fn parse_rfc3339_ms(value: &str) -> Option<u64> {
    let bytes = value.as_bytes();
    if bytes.len() < 20 || bytes[4] != b'-' || bytes[7] != b'-' || bytes[10] != b'T' {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u64> { value.get(range)?.parse().ok() };
    let year = number(0..4)? as i64;
    let month = number(5..7)? as u32;
    let day = number(8..10)? as u32;
    if bytes[13] != b':' || bytes[16] != b':' {
        return None;
    }
    let hour = number(11..13)? as i64;
    let minute = number(14..16)? as i64;
    let second = number(17..19)? as i64;
    if !(1..=12).contains(&month) || day == 0 || day > 31 || hour > 23 || minute > 59 || second > 60
    {
        return None;
    }
    let mut index = 19;
    let mut millis = 0u64;
    if bytes.get(index) == Some(&b'.') {
        index += 1;
        let start = index;
        while bytes.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        let digits = &value[start..index];
        if digits.is_empty() || digits.len() > 9 {
            return None;
        }
        let fraction: u64 = digits.parse().ok()?;
        millis = match digits.len() {
            0 => 0,
            1 => fraction * 100,
            2 => fraction * 10,
            3 => fraction,
            n => fraction / 10u64.pow((n - 3) as u32),
        };
    }
    let offset_ms: i64 = match bytes.get(index) {
        Some(b'Z') if index + 1 == bytes.len() => 0,
        Some(sign @ (b'+' | b'-')) => {
            if index + 6 != bytes.len()
                || bytes[index + 3] != b':'
                || !bytes[index + 1..index + 3].iter().all(u8::is_ascii_digit)
                || !bytes[index + 4..index + 6].iter().all(u8::is_ascii_digit)
            {
                return None;
            }
            let hours = number(index + 1..index + 3)? as i64;
            let minutes = number(index + 4..index + 6)? as i64;
            let offset = (hours * 3_600 + minutes * 60) * 1_000;
            if *sign == b'-' { -offset } else { offset }
        }
        _ => return None,
    };
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3_600 + minute * 60 + second;
    let total = seconds
        .saturating_mul(1_000)
        .saturating_add(millis as i64)
        .saturating_sub(offset_ms);
    Some(total.max(0) as u64)
}

fn format_rfc3339_ms(millis: u64) -> String {
    // Kubernetes decodes metav1.Time with the layout
    // `2006-01-02T15:04:05.000000Z07:00`: exactly six fractional digits are
    // required. Emit milliseconds padded to microseconds.
    let seconds = (millis / 1_000) as i64;
    let days = seconds.div_euclid(86_400);
    let within = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:06}Z",
        within / 3_600,
        (within % 3_600) / 60,
        within % 60,
        (millis % 1_000) * 1_000
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_helpers_round_trip() {
        let text = format_rfc3339_ms(1_795_536_123_456);
        assert_eq!(text, "2026-11-24T16:02:03.456000Z");
        assert_eq!(parse_rfc3339_ms(&text), Some(1_795_536_123_456));
        assert_eq!(
            parse_rfc3339_ms("2026-11-24T17:02:03.456+01:00"),
            Some(1_795_536_123_456)
        );
        assert_eq!(parse_rfc3339_ms("not-a-time"), None);
    }

    #[test]
    fn takeover_respects_live_holder_and_expiry() {
        let mut spec = LeaseSpec {
            holder_identity: Some("old".to_string()),
            lease_duration_seconds: Some(15),
            renew_time: Some(format_rfc3339_ms(1_000_000)),
            ..LeaseSpec::default()
        };
        assert!(!can_take_over(&spec, "new", 1_000_000 + 14_999));
        assert!(can_take_over(&spec, "new", 1_000_000 + 15_000));
        assert!(can_take_over(&spec, "old", 1_000_000));
        spec.renew_time = None;
        assert!(can_take_over(&spec, "new", 1_000_000));
        spec.holder_identity = None;
        assert!(can_take_over(&spec, "new", 1_000_000));
    }

    async fn mock_api() -> (
        String,
        JoinHandle<()>,
        Arc<std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
    ) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let store: Arc<std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>> =
            Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
        let task_store = store.clone();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let store = task_store.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    use tokio::io::AsyncWriteExt;
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 4_096];
                    let header_end = loop {
                        let Ok(size) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if size == 0 {
                            return;
                        }
                        buffer.extend_from_slice(&chunk[..size]);
                        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                            break position + 4;
                        }
                    };
                    let header = String::from_utf8_lossy(&buffer[..header_end]).to_string();
                    let length = header
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            if name.eq_ignore_ascii_case("content-length") {
                                value.trim().parse::<usize>().ok()
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0);
                    while buffer.len() < header_end + length {
                        let Ok(size) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if size == 0 {
                            break;
                        }
                        buffer.extend_from_slice(&chunk[..size]);
                    }
                    let request = header.lines().next().unwrap_or_default();
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    let (status, body) = if request.starts_with("GET ") {
                        match store.lock().unwrap().get(&path).cloned() {
                            Some(value) => ("200 OK", value.to_string()),
                            None => ("404 Not Found", "{}".to_string()),
                        }
                    } else if request.starts_with("POST ") || request.starts_with("PUT ") {
                        let mut value: serde_json::Value =
                            serde_json::from_slice(&buffer[header_end..]).unwrap_or_default();
                        // Kubernetes creates a named resource by POSTing to its
                        // collection; subsequent reads use the resource path.
                        let target = if request.starts_with("POST ") && path.ends_with("/leases") {
                            format!(
                                "{path}/{}",
                                value["metadata"]["name"].as_str().unwrap_or_default()
                            )
                        } else {
                            path
                        };
                        let version = store
                            .lock()
                            .unwrap()
                            .get(&target)
                            .and_then(|v| v["metadata"]["resourceVersion"].as_str())
                            .unwrap_or("0")
                            .parse::<u64>()
                            .unwrap_or(0)
                            + 1;
                        value["metadata"]["resourceVersion"] =
                            serde_json::Value::String(version.to_string());
                        store.lock().unwrap().insert(target, value.clone());
                        ("201 Created", value.to_string())
                    } else {
                        ("405 Method Not Allowed", "{}".to_string())
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), handle, store)
    }

    fn test_client(endpoint: &str, identity: &str) -> LeaseClient {
        LeaseClient::new(LeaseConfig {
            name: "historical-bars".to_string(),
            namespace: "test".to_string(),
            identity: identity.to_string(),
            ttl_secs: 15,
            renew_ms: 1_000,
            endpoint: Some(endpoint.to_string()),
            token_file: Some("/nonexistent-token".to_string()),
            ca_file: None,
        })
        .unwrap()
    }

    fn current(
        store: &Arc<std::sync::Mutex<std::collections::HashMap<String, serde_json::Value>>>,
        name: &str,
    ) -> serde_json::Value {
        let path = format!("/apis/coordination.k8s.io/v1/namespaces/test/leases/{name}");
        store
            .lock()
            .unwrap()
            .get(&path)
            .cloned()
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn acquire_renew_release_and_takeover() {
        let (endpoint, handle, store) = mock_api().await;
        let writer = test_client(&endpoint, "writer-a");
        assert!(writer.acquire().await.unwrap());
        assert_eq!(
            current(&store, "historical-bars")["spec"]["holderIdentity"],
            "writer-a"
        );

        let other = test_client(&endpoint, "writer-b");
        assert!(!other.acquire().await.unwrap());
        assert!(writer.acquire().await.unwrap()); // renewing its own Lease
        assert_eq!(
            current(&store, "historical-bars")["spec"]["holderIdentity"],
            "writer-a"
        );

        writer.release().await;
        assert!(other.acquire().await.unwrap());
        assert_eq!(
            current(&store, "historical-bars")["spec"]["holderIdentity"],
            "writer-b"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn manager_leases_are_independent_per_symbol() {
        let (endpoint, handle, store) = mock_api().await;
        let config = LeaseConfig {
            name: "historical-bars".to_string(),
            namespace: "test".to_string(),
            identity: "pod-a".to_string(),
            ttl_secs: 15,
            renew_ms: 1_000,
            endpoint: Some(endpoint.clone()),
            token_file: Some("/nonexistent-token".to_string()),
            ca_file: None,
        };
        let (_stop, stop_rx) = watch::channel(false);
        let manager = LeaseManager::acquire_all(
            &config,
            &["eurusd".to_string(), "btc_usdt".to_string()],
            stop_rx,
        )
        .await
        .unwrap();
        assert!(manager.owns("eurusd").load(Ordering::Acquire));
        assert!(manager.owns("btc_usdt").load(Ordering::Acquire));
        assert_eq!(
            current(&store, "historical-bars-eurusd")["spec"]["holderIdentity"],
            "pod-a"
        );
        assert_eq!(
            current(&store, "historical-bars-btc-usdt")["spec"]["holderIdentity"],
            "pod-a"
        );

        // A competing writer is refused on one symbol while it can still take
        // the other; losing one symbol never freezes the second.
        let mut eurusd_only = config.clone();
        eurusd_only.name = "historical-bars-eurusd".to_string();
        eurusd_only.identity = "pod-b".to_string();
        let other = LeaseClient::new(eurusd_only).unwrap();
        assert!(!other.acquire().await.unwrap());
        manager.shutdown().await;
        assert!(current(&store, "historical-bars-eurusd")["spec"]["holderIdentity"].is_null());
        assert!(other.acquire().await.unwrap());
        assert_eq!(
            current(&store, "historical-bars-eurusd")["spec"]["holderIdentity"],
            "pod-b"
        );
        handle.abort();
    }
}
