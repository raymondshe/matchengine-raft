//! REST tail and gap recovery for the historical writer.
//!
//! REST is only a backfill source for the window between the last sealed live
//! bar and the feed. It never overwrites a live row: a window that already has
//! a row is skipped, a filled window is marked `source=rest`, and the range is
//! bracketed by `bar_gaps` rows so a reconciler can find work that still needs
//! a live-derived confirmation. All values come from source strings, parsed
//! into 18-scale `Fixed` without any float step.

use std::collections::HashMap;
use std::collections::HashSet;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use matchengine_raft::order_book::Fixed;
use reqwest::Client;
use reqwest::StatusCode;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::config::HistoryConfig;
use crate::config::SymbolSpec;
use crate::feed::Stats;

use super::BarRow;
use super::FIVE_MIN_MS;
use super::MIN_MS;
use super::SEC_MS;
use super::decimal;
use super::state;
use super::tdengine::HistoryStore;

/// `source` value written to rows recovered from a REST source. Live rows use
/// the writer's own values (0/1 in deployed images); the archive importer owns
/// 1 as well, so REST rows are kept distinct at 2.
pub const SOURCE_REST: i8 = 2;
/// Published `GET /api/v3/klines` request weight.
const KLINE_WEIGHT: u32 = 2;
/// Upper bound on pages fetched in one poll, per symbol and interval.
const MAX_PAGES_PER_POLL: usize = 64;

#[derive(Clone, Debug)]
pub struct RestTailConfig {
    pub enabled: bool,
    pub base_url: String,
    /// Upstream symbol. Empty derives an uppercase Binance-style symbol from
    /// the local one (`btc-usdt` -> `BTCUSDT`).
    pub symbol: String,
    pub intervals: Vec<u64>,
    pub poll_secs: u64,
    /// Share of the published 6000 weight/min ceiling assigned to this tail.
    pub weight_per_min: u32,
    pub page_limit: usize,
}

impl RestTailConfig {
    pub fn from_config(config: &HistoryConfig) -> Self {
        Self {
            enabled: config.rest_tail_enabled,
            base_url: config.rest_tail_url.clone(),
            symbol: config.rest_tail_symbol.clone(),
            intervals: vec![SEC_MS, MIN_MS, FIVE_MIN_MS],
            poll_secs: config.rest_tail_poll_secs.max(5),
            weight_per_min: config.rest_tail_weight_per_min.max(KLINE_WEIGHT),
            page_limit: config.rest_tail_page_limit.clamp(1, 1_000),
        }
    }

    pub fn symbol_for(&self, local_symbol: &str) -> String {
        if self.symbol.is_empty() {
            local_symbol.to_ascii_uppercase().replace(['-', '_'], "")
        } else {
            self.symbol.clone()
        }
    }
}

#[derive(Debug)]
struct Budget {
    tokens: f64,
    capacity: f64,
    last: Instant,
    banned_until: Option<Instant>,
    last_denied: bool,
}

impl Budget {
    fn new(capacity: u32) -> Self {
        let capacity = capacity.max(KLINE_WEIGHT) as f64;
        Self {
            tokens: capacity,
            capacity,
            last: Instant::now(),
            banned_until: None,
            last_denied: false,
        }
    }

    fn allow(&mut self, weight: u32) -> bool {
        let now = Instant::now();
        let refill = now.duration_since(self.last).as_secs_f64() * (self.capacity / 60.0);
        self.tokens = (self.tokens + refill).min(self.capacity);
        self.last = now;
        if self.banned_until.is_some_and(|until| now < until) {
            self.last_denied = true;
            return false;
        }
        self.banned_until = None;
        if self.tokens >= weight as f64 {
            self.tokens -= weight as f64;
            self.last_denied = false;
            true
        } else {
            self.last_denied = true;
            false
        }
    }

    fn ban(&mut self, secs: u64) {
        self.banned_until = Some(Instant::now() + Duration::from_secs(secs.max(1)));
        self.tokens = 0.0;
        self.last_denied = true;
    }
}

pub struct RestTail {
    http: Client,
    config: RestTailConfig,
    budget: Mutex<Budget>,
}

impl RestTail {
    pub fn new(config: RestTailConfig) -> io::Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|error| io::Error::other(format!("REST tail HTTP client: {error}")))?;
        let budget = Budget::new(config.weight_per_min);
        Ok(Self {
            http,
            config,
            budget: Mutex::new(budget),
        })
    }

    /// Converts one Binance kline array to a stored row. Empty candles are
    /// dropped: the store is sparse and densification regenerates them.
    pub fn parse_kline(value: &Value) -> Option<BarRow> {
        let array = value.as_array()?;
        if array.len() < 11 {
            return None;
        }
        let trades = array[8].as_u64().unwrap_or(0);
        if trades == 0 {
            return None;
        }
        let decimal_at =
            |index: usize| -> Option<Fixed> { array[index].as_str().and_then(decimal::parse) };
        let open_ms = array[0].as_u64()?;
        let mut row = BarRow::new(open_ms.saturating_mul(1_000));
        row.open = decimal_at(1)?;
        row.high = decimal_at(2)?;
        row.low = decimal_at(3)?;
        row.close = decimal_at(4)?;
        row.volume = decimal_at(5)?;
        row.quote_volume = decimal_at(7)?;
        row.taker_base = decimal_at(9)?;
        row.taker_quote = decimal_at(10)?;
        row.trades = trades;
        row.source = SOURCE_REST;
        // A REST window is closed, but REST carries no trade ids and no
        // engine sequence. It stays unattributed until a reconciler confirms
        // it against live trades.
        row.sealed = true;
        Some(row)
    }

    /// True when the request budget has been exhausted or the upstream has
    /// banned this client. The caller must not advance its high-water mark
    /// while throttled, or the skipped window would never be retried.
    pub fn is_throttled(&self) -> bool {
        self.budget.lock().expect("budget lock").last_denied
    }

    async fn fetch(
        &self,
        symbol: &str,
        interval_ms: u64,
        from_ms: u64,
        to_ms: u64,
    ) -> io::Result<Vec<BarRow>> {
        if !self.budget.lock().expect("budget lock").allow(KLINE_WEIGHT) {
            return Ok(Vec::new());
        }
        let interval = match interval_ms {
            SEC_MS => "1s",
            MIN_MS => "1m",
            FIVE_MIN_MS => "5m",
            _ => return Ok(Vec::new()),
        };
        // reqwest is built without the URL-encoding helper here; every value
        // in this query is an ASCII number or an upstream symbol identifier.
        let url = format!(
            "{}/api/v3/klines?symbol={symbol}&interval={interval}&startTime={from_ms}&endTime={to_ms}&limit={}",
            self.config.base_url.trim_end_matches('/'),
            self.config.page_limit
        );
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| io::Error::other(format!("REST klines request: {error}")))?;
        let status = response.status();
        if status == StatusCode::TOO_MANY_REQUESTS {
            let secs = response
                .headers()
                .get("retry-after")
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or(60);
            self.budget.lock().expect("budget lock").ban(secs);
            tracing::warn!(secs, "REST tail rate limited");
            return Ok(Vec::new());
        }
        if status == StatusCode::IM_A_TEAPOT {
            self.budget.lock().expect("budget lock").ban(60);
            tracing::warn!("REST tail banned by upstream");
            return Ok(Vec::new());
        }
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(io::Error::other(format!(
                "REST klines {status}: {}",
                body.chars().take(200).collect::<String>()
            )));
        }
        let values: Vec<Value> = response
            .json()
            .await
            .map_err(|error| io::Error::other(format!("REST klines decode: {error}")))?;
        Ok(values.iter().filter_map(Self::parse_kline).collect())
    }

    /// Fills windows with no stored row in `[from_us, to_us)`, recording the
    /// repaired range in `bar_gaps` before and after the upsert. Live rows are
    /// never replaced. Returns the number of rows written.
    pub async fn backfill(
        &self,
        store: &HistoryStore,
        symbol: &str,
        interval_ms: u64,
        from_us: u64,
        to_us: u64,
    ) -> io::Result<usize> {
        if to_us <= from_us {
            return Ok(0);
        }
        let upstream = self.config.symbol_for(symbol);
        let width_us = interval_ms.saturating_mul(1_000);
        let mut cursor_ms = from_us / 1_000;
        let end_ms = to_us / 1_000;
        let mut written = 0;
        for _ in 0..MAX_PAGES_PER_POLL {
            if cursor_ms >= end_ms {
                break;
            }
            let rows = self
                .fetch(&upstream, interval_ms, cursor_ms, end_ms)
                .await?;
            if rows.is_empty() {
                break;
            }
            let first_ts = rows.first().map(|row| row.ts_us).unwrap_or(0);
            let last_ts = rows.last().map(|row| row.ts_us).unwrap_or(first_ts);
            let existing = store
                .bars(
                    interval_ms,
                    symbol,
                    first_ts,
                    last_ts.saturating_add(width_us),
                )
                .await?;
            let known: HashSet<u64> = existing.iter().map(|row| row.ts_us).collect();
            let missing: Vec<BarRow> = rows
                .into_iter()
                .filter(|row| !known.contains(&row.ts_us))
                .collect();
            let mut group: Vec<BarRow> = Vec::new();
            for row in missing {
                let contiguous = group
                    .last()
                    .is_some_and(|previous| row.ts_us == previous.ts_us.saturating_add(width_us));
                if !contiguous && !group.is_empty() {
                    written += self
                        .persist_group(store, symbol, interval_ms, width_us, &group)
                        .await?;
                    group.clear();
                }
                group.push(row);
            }
            if !group.is_empty() {
                written += self
                    .persist_group(store, symbol, interval_ms, width_us, &group)
                    .await?;
            }
            let next = last_ts.saturating_add(width_us) / 1_000;
            cursor_ms = next.max(cursor_ms.saturating_add(1));
        }
        Ok(written)
    }

    async fn persist_group(
        &self,
        store: &HistoryStore,
        symbol: &str,
        interval_ms: u64,
        width_us: u64,
        group: &[BarRow],
    ) -> io::Result<usize> {
        let Some(first) = group.first() else {
            return Ok(0);
        };
        let start = first.ts_us;
        let end = group
            .last()
            .map(|row| row.ts_us.saturating_add(width_us))
            .unwrap_or(start);
        state::append_gap(store, symbol, start, end, true).await?;
        if let Err(error) = store.upsert(interval_ms, symbol, group).await {
            tracing::warn!(%error, %symbol, "REST tail upsert failed; gap left open");
            return Err(error);
        }
        state::append_gap(store, symbol, start, end, false).await?;
        Ok(group.len())
    }
}

/// Periodic backfill loop. It starts with one `repair_window` look-back and
/// then advances a per-symbol high-water mark, so a quiet service does not
/// re-fetch an unchanged window on every tick.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    tail: Arc<RestTail>,
    store: Arc<HistoryStore>,
    config: &HistoryConfig,
    symbols: Vec<SymbolSpec>,
    owns: HashMap<String, Arc<AtomicBool>>,
    stats: Arc<Stats>,
    mut stop: watch::Receiver<bool>,
) -> JoinHandle<()> {
    let poll_secs = config.rest_tail_poll_secs.max(5);
    let repair_us = config.repair_window_secs.saturating_mul(1_000_000);
    let seal_lag_us = config.seal_lag_ms.saturating_mul(1_000);
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(poll_secs));
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tick.tick().await;
        let mut last_end: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                result = stop.changed() => {
                    if result.is_err() || *stop.borrow_and_update() {
                        break;
                    }
                    continue;
                }
            }
            for spec in &symbols {
                if !owns
                    .get(&spec.symbol)
                    .is_some_and(|flag| flag.load(Ordering::Acquire))
                {
                    continue;
                }
                let durable = match state::read_state(&store, &spec.symbol).await {
                    Ok(value) => value.unwrap_or_default(),
                    Err(error) => {
                        tracing::warn!(%error, symbol = %spec.symbol, "REST tail state read failed");
                        continue;
                    }
                };
                let from_us = last_end
                    .get(&spec.symbol)
                    .copied()
                    .unwrap_or_else(|| durable.sealed_through_us.saturating_sub(repair_us));
                let to_us = now_ms()
                    .saturating_mul(1_000)
                    .saturating_sub(seal_lag_us)
                    .max(from_us);
                let mut failed = false;
                for interval_ms in &tail.config.intervals {
                    match tail
                        .backfill(&store, &spec.symbol, *interval_ms, from_us, to_us)
                        .await
                    {
                        Ok(rows) => {
                            if rows > 0 {
                                stats
                                    .history_rest_rows
                                    .fetch_add(rows as u64, Ordering::Relaxed);
                                tracing::info!(symbol = %spec.symbol, rows, interval_ms, "REST tail filled bars");
                            }
                        }
                        Err(error) => {
                            failed = true;
                            tracing::warn!(%error, symbol = %spec.symbol, interval_ms, "REST tail backfill failed");
                        }
                    }
                }
                // Keep the high-water mark when any interval failed or the
                // upstream budget ran out: the next poll retries the same
                // window, skipping rows that are already stored.
                if !failed && !tail.is_throttled() {
                    last_end.insert(spec.symbol.clone(), to_us);
                }
            }
        }
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kline(ts_ms: u64, trades: u64) -> Value {
        serde_json::json!([
            ts_ms,
            "1.000000000000000001",
            "1.200000000000000003",
            "0.999999999999999999",
            "1.100000000000000005",
            "0.400000000000000006",
            ts_ms + 999,
            "0.440000000000000008",
            trades,
            "0.200000000000000003",
            "0.220000000000000004",
            "0"
        ])
    }

    #[test]
    fn parse_kline_is_exact_and_skips_empty_windows() {
        let row = RestTail::parse_kline(&kline(1_700_000_000_000, 2)).unwrap();
        assert_eq!(row.ts_us, 1_700_000_000_000_000);
        assert_eq!(decimal::to_sql(row.open), "1.000000000000000001");
        assert_eq!(decimal::to_sql(row.quote_volume), "0.440000000000000008");
        assert_eq!(row.trades, 2);
        assert_eq!(row.source, SOURCE_REST);
        assert!(row.sealed);
        assert!(row.last_log.is_none());
        assert!(RestTail::parse_kline(&kline(1_700_000_000_000, 0)).is_none());
    }

    #[test]
    fn budget_refills_and_bans() {
        let mut budget = Budget::new(4);
        assert!(budget.allow(2));
        assert!(budget.allow(2));
        assert!(!budget.allow(2));
        budget.ban(1);
        assert!(!budget.allow(2));
    }

    #[tokio::test]
    async fn fetch_parses_pages_and_honours_the_query() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = serde_json::json!([kline(1_700_000_000_000, 3)]).to_string();
        let handle = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            use tokio::io::AsyncWriteExt;
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut buffer = [0u8; 8_192];
            let _ = stream.read(&mut buffer).await;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        });
        let tail = RestTail::new(RestTailConfig {
            enabled: true,
            base_url: format!("http://{addr}"),
            symbol: "BTCUSDT".to_string(),
            intervals: vec![SEC_MS],
            poll_secs: 5,
            weight_per_min: 60,
            page_limit: 100,
        })
        .unwrap();
        let rows = tail
            .fetch("BTCUSDT", SEC_MS, 1_700_000_000_000, 1_700_000_010_000)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].trades, 3);
        handle.await.unwrap();
    }

    async fn mock_http(
        body: String,
        record: Option<Arc<Mutex<Vec<String>>>>,
    ) -> (String, JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let body = body.clone();
                let record = record.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncReadExt;
                    use tokio::io::AsyncWriteExt;
                    let mut buffer = Vec::new();
                    let mut chunk = [0u8; 8_192];
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
                    if let Some(record) = record {
                        record
                            .lock()
                            .unwrap()
                            .push(String::from_utf8_lossy(&buffer[header_end..]).to_string());
                    }
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn backfill_fills_missing_rows_and_brackets_gaps() {
        let ts_ms = 1_700_000_000_000;
        let (rest_url, rest_handle) =
            mock_http(serde_json::json!([kline(ts_ms, 1)]).to_string(), None).await;
        let statements = Arc::new(Mutex::new(Vec::new()));
        let (td_url, td_handle) = mock_http(
            r#"{"code":0,"data":[]}"#.to_string(),
            Some(statements.clone()),
        )
        .await;
        let config = HistoryConfig {
            url: td_url,
            ..HistoryConfig::default()
        };
        let store = HistoryStore::new(&config).unwrap();
        let tail = RestTail::new(RestTailConfig {
            enabled: true,
            base_url: rest_url,
            symbol: "BTCUSDT".to_string(),
            intervals: vec![SEC_MS],
            poll_secs: 5,
            weight_per_min: 60,
            page_limit: 100,
        })
        .unwrap();
        let filled = tail
            .backfill(
                &store,
                "btcusdt",
                SEC_MS,
                ts_ms.saturating_mul(1_000),
                ts_ms.saturating_mul(1_000).saturating_add(1_000_000),
            )
            .await
            .unwrap();
        assert_eq!(filled, 1);
        let sql = statements.lock().unwrap().clone();
        assert_eq!(
            sql.iter()
                .filter(|statement| statement.contains("INSERT INTO bar_gaps_btcusdt"))
                .count(),
            2,
            "one open and one closed gap bracket: {sql:?}"
        );
        assert!(
            sql.iter()
                .any(|statement| statement.contains("INSERT INTO bars_1s_btcusdt"))
        );
        rest_handle.abort();
        td_handle.abort();
    }
}
