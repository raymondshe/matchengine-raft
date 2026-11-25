//! Minimal TDengine SQL-over-REST client for historical bars.
//!
//! `taosAdapter` is stateless and returns DECIMAL columns as exact strings when
//! they are cast to `VARCHAR`. The serving path never decodes a binary DECIMAL
//! into a 96-bit client type, and never uses `f64`.

use std::io;

use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;

use crate::config::HistoryConfig;
use crate::config::SymbolSpec;
use crate::config::history_table_name;

use super::BarRow;
use super::decimal;

#[derive(Debug, Deserialize)]
struct RestResponse {
    #[serde(default)]
    code: i32,
    #[serde(default)]
    desc: String,
    #[serde(default)]
    data: Vec<Vec<Value>>,
}

#[derive(Clone)]
pub struct HistoryStore {
    http: Client,
    base: String,
    user: String,
    password: String,
    pub database: String,
}

impl HistoryStore {
    pub fn new(config: &HistoryConfig) -> io::Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .map_err(|error| io::Error::other(format!("TDengine HTTP client: {error}")))?;
        Ok(Self {
            http,
            base: config.url.trim_end_matches('/').to_string(),
            user: config.user.clone(),
            password: config.password.clone(),
            // The database name is also the /rest/sql/<db> path segment, so
            // sanitize it once here and use the same identifier everywhere.
            database: history_table_name(&config.database),
        })
    }

    /// Runs one SQL statement and returns its rows.
    pub async fn sql(&self, statement: &str) -> io::Result<Vec<Vec<Value>>> {
        let url = format!("{}/rest/sql/{}", self.base, self.database);
        let response = self
            .http
            .post(url)
            .basic_auth(&self.user, Some(&self.password))
            .body(statement.to_string())
            .send()
            .await
            .map_err(|error| io::Error::other(format!("TDengine request: {error}")))?;
        let status = response.status();
        let body: RestResponse = response
            .json()
            .await
            .map_err(|error| io::Error::other(format!("TDengine response ({status}): {error}")))?;
        if body.code != 0 {
            return Err(io::Error::other(format!(
                "TDengine SQL error {}: {} (statement: {})",
                body.code,
                body.desc,
                statement.chars().take(180).collect::<String>()
            )));
        }
        Ok(body.data)
    }

    pub async fn ping(&self) -> io::Result<()> {
        self.sql("SELECT SERVER_VERSION()").await.map(|_| ())
    }

    /// Creates the database and the four tables used by phase P0/P1. Safe to
    /// call on every start. Production deployments can set `auto_create=false`
    /// and apply the DDL from the runbook instead.
    pub async fn ensure_schema(
        &self,
        config: &HistoryConfig,
        symbols: &[SymbolSpec],
    ) -> io::Result<()> {
        let db_name = history_table_name(&self.database);
        let db = valid_identifier(&db_name)?;
        if config.auto_create {
            self.sql(&format!(
                "CREATE DATABASE IF NOT EXISTS {db} PRECISION 'us' KEEP {} DURATION 10d \
                 VGROUPS {} REPLICA {} CACHEMODEL 'last_row'",
                config.keep_days, config.vgroups, config.replica
            ))
            .await?;
        }
        // The same client is used with /rest/sql/<db>, so the database must
        // already exist even when auto_create is disabled.
        for interval in ["1s", "1m", "5m"] {
            self.sql(&format!(
                "CREATE STABLE IF NOT EXISTS bars_{interval} (\
                 ts TIMESTAMP, open DECIMAL(38,18), high DECIMAL(38,18), \
                 low DECIMAL(38,18), close DECIMAL(38,18), volume DECIMAL(38,18), \
                 quote_volume DECIMAL(38,18), taker_base DECIMAL(38,18), \
                 taker_quote DECIMAL(38,18), trades BIGINT UNSIGNED, last_ts TIMESTAMP, \
                 first_log BIGINT, first_ord INT, last_log BIGINT, last_ord INT, \
                 rev INT, source TINYINT, sealed BOOL, seq BIGINT\
                 ) TAGS (symbol VARCHAR(32))"
            ))
            .await?;
        }
        self.sql(
            "CREATE STABLE IF NOT EXISTS bar_state (\
             ts TIMESTAMP, sealed_through TIMESTAMP, first_ts TIMESTAMP, revision BIGINT, \
             repair_floor_seq BIGINT, last_log BIGINT, last_ord INT, writer_epoch VARCHAR(64)\
             ) TAGS (symbol VARCHAR(32))",
        )
        .await?;
        self.sql("CREATE TABLE IF NOT EXISTS bar_meta (ts TIMESTAMP, schema_version INT, created_at TIMESTAMP, note VARCHAR(256))")
            .await?;
        self.sql("INSERT INTO bar_meta VALUES (NOW, 1, NOW, 'historical bars P0')")
            .await?;

        for symbol in symbols {
            let table_symbol = history_table_name(&symbol.symbol);
            let name = valid_identifier(&table_symbol)?;
            for interval in ["1s", "1m", "5m"] {
                self.sql(&format!(
                    "CREATE TABLE IF NOT EXISTS bars_{interval}_{name} \
                     USING bars_{interval} TAGS ('{name}')"
                ))
                .await?;
            }
        }
        Ok(())
    }

    /// Drops the store, used by integration tests to isolate databases.
    pub async fn drop_database(&self) -> io::Result<()> {
        let db_name = history_table_name(&self.database);
        let db = valid_identifier(&db_name)?;
        // A DROP against /rest/sql without a database is handled by a fresh
        // client with the same credentials.
        let url = format!("{}/rest/sql", self.base);
        let response = self
            .http
            .post(url)
            .basic_auth(&self.user, Some(&self.password))
            .body(format!("DROP DATABASE IF EXISTS {db}"))
            .send()
            .await
            .map_err(|error| io::Error::other(format!("TDengine drop: {error}")))?;
        let body: RestResponse = response.json().await.map_err(io::Error::other)?;
        if body.code != 0 {
            return Err(io::Error::other(format!(
                "TDengine drop error: {}",
                body.desc
            )));
        }
        Ok(())
    }

    /// Upserts rows into one subtable. An existing `(symbol, ts)` is replaced,
    /// so retries are idempotent. One statement targets one subtable: TDengine
    /// batches are not atomic across subtables.
    pub async fn upsert(&self, interval_ms: u64, symbol: &str, rows: &[BarRow]) -> io::Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let table = table_name(interval_ms, symbol)?;
        let mut statement = format!("INSERT INTO {table} VALUES ");
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                statement.push(' ');
            }
            statement.push_str(&row_values(row));
        }
        self.sql(&statement).await.map(|_| ())
    }

    /// Reads raw stored rows in `[from_us, to_us)`.
    pub async fn bars(
        &self,
        interval_ms: u64,
        symbol: &str,
        from_us: u64,
        to_us: u64,
    ) -> io::Result<Vec<BarRow>> {
        let table = self.bar_table(interval_ms, symbol)?;
        let statement = format!(
            "SELECT {COLUMNS} FROM {table} WHERE ts >= {from_us} AND ts < {to_us} ORDER BY ts"
        );
        self.query_bars(&statement).await
    }

    /// Whether a materialized source table has any rows at all. Used to fall
    /// back from `bars_5m` to `bars_1m` on deployments that predate 5m rows.
    pub async fn has_bars(&self, interval_ms: u64, symbol: &str) -> bool {
        let Ok(table) = self.bar_table(interval_ms, symbol) else {
            return false;
        };
        matches!(
            self.sql(&format!("SELECT 1 FROM {table} LIMIT 1")).await,
            Ok(rows) if !rows.is_empty()
        )
    }

    /// Rolls a materialized source series into a coarser interval at read time.
    ///
    /// `source_ms` is chosen by the caller: `bars_5m` when it has data and the
    /// target is a whole number of 5-minute windows, otherwise `bars_1m`.
    pub async fn rollup(
        &self,
        interval_ms: u64,
        source_ms: u64,
        symbol: &str,
        from_us: u64,
        to_us: u64,
    ) -> io::Result<Vec<BarRow>> {
        let table = self.bar_table(source_ms, symbol)?;
        let window = tdengine_interval(interval_ms).ok_or_else(|| {
            io::Error::other(format!("unsupported historical interval {interval_ms}ms"))
        })?;
        let statement = format!(
            "SELECT _wstart AS ts, CAST(FIRST(open) AS VARCHAR), CAST(MAX(high) AS VARCHAR), \
             CAST(MIN(low) AS VARCHAR), CAST(LAST(close) AS VARCHAR), \
             CAST(SUM(volume) AS VARCHAR), CAST(SUM(quote_volume) AS VARCHAR), \
             CAST(SUM(taker_base) AS VARCHAR), CAST(SUM(taker_quote) AS VARCHAR), \
             SUM(trades), LAST(last_ts), MIN(first_log), MIN(first_ord), \
             MAX(last_log), MAX(last_ord), MAX(rev), MAX(source), \
             MAX(CAST(sealed AS INT)), MAX(seq) \
             FROM {table} WHERE ts >= {from_us} AND ts < {to_us} INTERVAL({window})"
        );
        self.query_bars(&statement).await
    }

    /// Close of the last stored bar before `ts_us`, used to synthesize flat
    /// windows. Never sourced from a wall clock.
    pub async fn previous_close(
        &self,
        interval_ms: u64,
        symbol: &str,
        ts_us: u64,
    ) -> io::Result<Option<matchengine_raft::order_book::Fixed>> {
        let table = self.bar_table(interval_ms, symbol)?;
        let data = self
            .sql(&format!(
                "SELECT CAST(close AS VARCHAR) FROM {table} WHERE ts < {ts_us} ORDER BY ts DESC LIMIT 1"
            ))
            .await?;
        Ok(data
            .first()
            .and_then(|row| row.first())
            .and_then(|value| value.as_str())
            .and_then(decimal::parse))
    }

    /// Last stored open time, for "most recent N bars" requests.
    pub async fn last_ts(&self, interval_ms: u64, symbol: &str) -> io::Result<Option<u64>> {
        let table = self.bar_table(interval_ms, symbol)?;
        // TDengine 3.3.6 rejects MAX() on TIMESTAMP; an ordered LIMIT 1 is
        // exact and uses the same primary index as a point lookup.
        let data = self
            .sql(&format!("SELECT ts FROM {table} ORDER BY ts DESC LIMIT 1"))
            .await?;
        Ok(data.first().and_then(|row| row.first()).and_then(json_u64))
    }

    fn bar_table(&self, interval_ms: u64, symbol: &str) -> io::Result<String> {
        table_name(interval_ms, symbol)
    }

    async fn query_bars(&self, statement: &str) -> io::Result<Vec<BarRow>> {
        let data = self.sql(statement).await?;
        let mut rows = Vec::with_capacity(data.len());
        for values in data {
            rows.push(parse_row(&values)?);
        }
        Ok(rows)
    }
}

const COLUMNS: &str = "ts, CAST(open AS VARCHAR), CAST(high AS VARCHAR), CAST(low AS VARCHAR), \
    CAST(close AS VARCHAR), CAST(volume AS VARCHAR), CAST(quote_volume AS VARCHAR), \
    CAST(taker_base AS VARCHAR), CAST(taker_quote AS VARCHAR), trades, last_ts, \
    first_log, first_ord, last_log, last_ord, rev, source, sealed, seq";

fn row_values(row: &BarRow) -> String {
    let opt_u64 = |value: Option<u64>| {
        value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".into())
    };
    let opt_u32 = |value: Option<u32>| {
        value
            .map(|v| v.to_string())
            .unwrap_or_else(|| "NULL".into())
    };
    format!(
        "({}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {})",
        row.ts_us,
        decimal::to_sql(row.open),
        decimal::to_sql(row.high),
        decimal::to_sql(row.low),
        decimal::to_sql(row.close),
        decimal::to_sql(row.volume),
        decimal::to_sql(row.quote_volume),
        decimal::to_sql(row.taker_base),
        decimal::to_sql(row.taker_quote),
        row.trades,
        opt_u64(row.last_ts_us),
        opt_u64(row.first_log),
        opt_u32(row.first_ord),
        opt_u64(row.last_log),
        opt_u32(row.last_ord),
        row.rev,
        row.source,
        if row.sealed { "true" } else { "false" },
        row.seq
    )
}

fn decimal_at(values: &[Value], index: usize) -> io::Result<matchengine_raft::order_book::Fixed> {
    values
        .get(index)
        .and_then(|value| value.as_str())
        .and_then(decimal::parse)
        .ok_or_else(|| io::Error::other(format!("invalid DECIMAL at column {index}")))
}

fn parse_row(values: &[Value]) -> io::Result<BarRow> {
    if values.len() < 19 {
        return Err(io::Error::other(format!(
            "short bar row: {} columns",
            values.len()
        )));
    }
    let mut row = BarRow::new(
        json_u64(&values[0])
            .ok_or_else(|| io::Error::other(format!("invalid timestamp: {:?}", values[0])))?,
    );
    row.open = decimal_at(values, 1)?;
    row.high = decimal_at(values, 2)?;
    row.low = decimal_at(values, 3)?;
    row.close = decimal_at(values, 4)?;
    row.volume = decimal_at(values, 5)?;
    row.quote_volume = decimal_at(values, 6)?;
    row.taker_base = decimal_at(values, 7)?;
    row.taker_quote = decimal_at(values, 8)?;
    row.trades = json_u64(&values[9]).unwrap_or(0);
    row.last_ts_us = values.get(10).and_then(json_u64);
    row.first_log = values.get(11).and_then(json_u64);
    row.first_ord = values.get(12).and_then(json_u64).map(|v| v as u32);
    row.last_log = values.get(13).and_then(json_u64);
    row.last_ord = values.get(14).and_then(json_u64).map(|v| v as u32);
    row.rev = values.get(15).and_then(json_i64).unwrap_or(0) as i32;
    row.source = values.get(16).and_then(json_i64).unwrap_or(0) as i8;
    row.sealed = matches!(values.get(17), Some(Value::Bool(true)))
        || matches!(values.get(17).and_then(Value::as_i64), Some(1));
    row.seq = values.get(18).and_then(json_u64).unwrap_or(0);
    Ok(row)
}

fn json_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|v| u64::try_from(v).ok())),
        Value::String(text) => parse_timestamp_us(text).or_else(|| text.parse().ok()),
        _ => None,
    }
}

fn json_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// Parses TDengine's REST timestamp form `2025-01-02T00:00:00.123456Z`.
fn parse_timestamp_us(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() < 21 || bytes.get(19) != Some(&b'.') || bytes.last() != Some(&b'Z') {
        return None;
    }
    let number = |range: std::ops::Range<usize>| -> Option<u64> {
        std::str::from_utf8(bytes.get(range)?).ok()?.parse().ok()
    };
    let year = number(0..4)? as i64;
    let month = number(5..7)? as u32;
    let day = number(8..10)? as u32;
    let hour = number(11..13)?;
    let minute = number(14..16)?;
    let second = number(17..19)?;
    let fraction_text = std::str::from_utf8(&bytes[20..bytes.len() - 1]).ok()?;
    if fraction_text.is_empty()
        || fraction_text.len() > 6
        || !fraction_text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let micros: u64 =
        fraction_text.parse::<u64>().ok()? * 10u64.pow((6 - fraction_text.len()) as u32);
    let days = days_from_civil(year, month, day)?;
    let seconds = days * 86_400 + hour as i64 * 3_600 + minute as i64 * 60 + second as i64;
    u64::try_from(seconds)
        .ok()?
        .checked_mul(1_000_000)?
        .checked_add(micros)
}

/// Howard Hinnant's civil-date algorithm; days since 1970-01-01.
fn days_from_civil(year: i64, month: u32, day: u32) -> Option<i64> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let m = month as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + day as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

/// Maps a Binance interval to a TDengine `INTERVAL(...)` window. UTC only;
/// time-zone anchors are phase P5.
pub fn tdengine_interval(interval_ms: u64) -> Option<&'static str> {
    let min = super::MIN_MS;
    if interval_ms == 3 * min {
        Some("3m")
    } else if interval_ms == 5 * min {
        Some("5m")
    } else if interval_ms == 15 * min {
        Some("15m")
    } else if interval_ms == 30 * min {
        Some("30m")
    } else if interval_ms == 60 * min {
        Some("1h")
    } else if interval_ms == 120 * min {
        Some("2h")
    } else if interval_ms == 240 * min {
        Some("4h")
    } else if interval_ms == 360 * min {
        Some("6h")
    } else if interval_ms == 480 * min {
        Some("8h")
    } else if interval_ms == 720 * min {
        Some("12h")
    } else if interval_ms == 1_440 * min {
        Some("1d")
    } else if interval_ms == 4_320 * min {
        Some("3d")
    } else if interval_ms == 10_080 * min {
        Some("1w, 4d") // Monday 00:00 UTC
    } else if interval_ms == 43_200 * min {
        Some("1n") // natural month
    } else {
        None
    }
}

/// Every table/tag identifier is derived through this one path so a symbol
/// accepted by the CLI (`btc-usdt`) always maps to the same TDengine name
/// (`bars_1s_btc_usdt`) in schema creation, upserts and queries.
fn table_name(interval_ms: u64, symbol: &str) -> io::Result<String> {
    let table_symbol = history_table_name(symbol);
    let name = valid_identifier(&table_symbol)?;
    let suffix = match interval_ms {
        super::SEC_MS => "1s",
        super::MIN_MS => "1m",
        super::FIVE_MIN_MS => "5m",
        other => {
            return Err(io::Error::other(format!("no table for interval {other}ms")));
        }
    };
    Ok(format!("bars_{suffix}_{name}"))
}

fn valid_identifier(name: &str) -> io::Result<&str> {
    let valid = !name.is_empty()
        && name.len() <= 32
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if valid {
        Ok(name)
    } else {
        Err(io::Error::other(format!(
            "unsafe TDengine identifier: {name}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_parser_matches_rest_format() {
        assert_eq!(
            parse_timestamp_us("2025-01-02T00:00:00.123456Z"),
            Some(1_735_776_000_123_456)
        );
        assert_eq!(parse_timestamp_us("not-a-date"), None);
    }

    #[test]
    fn hyphenated_symbols_use_one_sanitized_identifier_everywhere() {
        let symbol = "btc-usdt";
        let sanitized = history_table_name(symbol);
        assert_eq!("btc_usdt", sanitized);
        assert_eq!("btc_usdt", valid_identifier(&sanitized).unwrap());
        for (interval, expected) in [
            (super::super::SEC_MS, "bars_1s_btc_usdt"),
            (super::super::MIN_MS, "bars_1m_btc_usdt"),
            (super::super::FIVE_MIN_MS, "bars_5m_btc_usdt"),
        ] {
            assert_eq!(expected, table_name(interval, symbol).unwrap());
            assert_eq!(expected, table_name(interval, "btc_usdt").unwrap());
        }
        // The database identifier follows the same rule, including the REST
        // URL path segment used by `sql()` and `drop_database()`.
        let config = HistoryConfig {
            database: "market-bars".to_string(),
            ..HistoryConfig::default()
        };
        let store = HistoryStore::new(&config).unwrap();
        assert_eq!("market_bars", store.database);
        let sanitized_db = history_table_name(&store.database);
        assert_eq!("market_bars", valid_identifier(&sanitized_db).unwrap());
    }

    #[test]
    fn rollup_windows_cover_the_binance_set() {
        assert_eq!(tdengine_interval(3 * super::super::MIN_MS), Some("3m"));
        assert_eq!(tdengine_interval(1_440 * super::super::MIN_MS), Some("1d"));
        assert_eq!(
            tdengine_interval(10_080 * super::super::MIN_MS),
            Some("1w, 4d")
        );
        assert_eq!(tdengine_interval(43_200 * super::super::MIN_MS), Some("1n"));
        assert_eq!(tdengine_interval(super::super::SEC_MS), None);
    }
}
