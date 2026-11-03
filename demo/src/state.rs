use std::fmt;

use actix_web::http::StatusCode;
use openraft::errors::ClientWriteError;
use openraft::raft::ClientWriteResponse;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use matchengine_raft::MatchEngineTypeConfig;
use matchengine_raft::client::ClientError;
use matchengine_raft::client::MatchEngineClient;
use matchengine_raft::match_result::EventId;
use matchengine_raft::match_result::MatchEvent;
use matchengine_raft::network::api::EventsRequest;
use matchengine_raft::order_book::DepthLevel;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::MAX_FEE_RATE;
use matchengine_raft::order_book::MIN_FEE_RATE;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::SymbolConfig;
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::order_book::VolumeUnit;
use matchengine_raft::store::MatchEngineRequest;

type C = MatchEngineTypeConfig;
pub const MAX_STATE_ORDERS: usize = 200;
pub const DEFAULT_EVENTS_LIMIT: usize = 500;

/// The demo server's connection to one running engine node.
pub struct DemoApp {
    pub engine_id: u64,
    pub engine_addr: String,
    client: MatchEngineClient<C>,
}

impl DemoApp {
    pub fn new(engine_id: u64, engine_addr: String) -> Self {
        let client = MatchEngineClient::new(engine_id, engine_addr.clone());
        Self {
            engine_id,
            engine_addr,
            client,
        }
    }

    pub async fn state(&self, depth: usize) -> Result<StateResponse, ApiError> {
        let depth = depth.clamp(1, 200);
        let symbol: SymbolConfig = self.read_json("symbol_config").await?;
        let orders: Vec<Order> = self.read_json("orderbook_orders").await?;
        let stops: Vec<Order> = self.read_json("orderbook_stops").await?;
        let sequence: u64 = self
            .read_text("orderbook_sequence")
            .await?
            .parse()
            .map_err(|error| ApiError::decode(format!("orderbook_sequence: {error}")))?;
        let spread = self.read_optional_fixed("orderbook_spread").await?;
        let last_trade_price = self
            .read_optional_fixed("orderbook_last_trade_price")
            .await?;
        let events = self.client.events_status().await.map_err(client_error)?;
        let metrics = self.client.metrics().await.map_err(client_error)?;
        let market_data = self
            .read_json::<Value>("marketdata_status")
            .await
            .ok()
            .filter(|value| value.as_object().is_some_and(|object| !object.is_empty()));

        let leader_id = metrics.current_leader;
        let leader_addr = leader_id
            .and_then(|id| metrics.membership_config.membership().get_node(&id))
            .map(|node| node.addr.clone());

        let mut visible_orders = orders.clone();
        visible_orders.sort_by_key(|order| order.sequence);
        let orders_total = visible_orders.len();
        let orders_truncated = orders_total > MAX_STATE_ORDERS;
        visible_orders.truncate(MAX_STATE_ORDERS);

        let stops_total = stops.len();
        let stops_truncated = stops_total > MAX_STATE_ORDERS;
        let stops: Vec<Order> = stops.into_iter().take(MAX_STATE_ORDERS).collect();

        Ok(StateResponse {
            schema_version: 1,
            node: NodeState {
                id: self.engine_id,
                http_addr: self.engine_addr.clone(),
                is_leader: metrics.current_leader == Some(self.engine_id),
                leader_id,
                leader_addr,
                term: metrics.current_term,
                raft_state: format!("{:?}", metrics.state),
                healthy: metrics.running_state.is_ok(),
                ready: metrics.running_state.is_ok() && metrics.current_leader.is_some(),
            },
            symbol: SymbolState {
                config: symbol,
                // There is no public `symbol_locked` read key; the first accepted
                // order is the point after which configuration is refused.
                locked: sequence > 0,
            },
            book: BookState {
                sequence,
                spread,
                last_trade_price,
                depth: DepthState {
                    bids: depth_from_orders(&orders, OrderSide::Buy, depth),
                    asks: depth_from_orders(&orders, OrderSide::Sell, depth),
                },
            },
            orders: OrdersState {
                total: orders_total,
                truncated: orders_truncated,
                items: visible_orders,
            },
            stops: OrdersState {
                total: stops_total,
                truncated: stops_truncated,
                items: stops,
            },
            events: EventsState {
                oldest_event_id: events.oldest_event_id,
                last_event_id: events.last_event_id,
                retained_events: events.retained_events,
                evictions_total: events.evictions_total,
                retention_limit: events.retention_limit,
            },
            market_data: MarketDataState {
                enabled: market_data.is_some(),
                status: market_data,
            },
        })
    }

    pub async fn book(&self, limit: usize) -> Result<FullBookResponse, ApiError> {
        let orders: Vec<Order> = self.read_json("orderbook_orders").await?;
        let stops: Vec<Order> = self.read_json("orderbook_stops").await?;
        let sequence: u64 = self
            .read_text("orderbook_sequence")
            .await?
            .parse()
            .map_err(|error| ApiError::decode(format!("orderbook_sequence: {error}")))?;

        let mut bids: Vec<Order> = orders
            .iter()
            .filter(|order| order.side == OrderSide::Buy)
            .copied()
            .collect();
        let mut asks: Vec<Order> = orders
            .iter()
            .filter(|order| order.side == OrderSide::Sell)
            .copied()
            .collect();
        bids.sort_by(|a, b| b.price.cmp(&a.price).then(a.sequence.cmp(&b.sequence)));
        asks.sort_by(|a, b| a.price.cmp(&b.price).then(a.sequence.cmp(&b.sequence)));
        let bids_total = bids.len();
        let asks_total = asks.len();
        let stops_total = stops.len();
        bids.truncate(limit);
        asks.truncate(limit);
        let stops: Vec<Order> = stops.into_iter().take(limit).collect();

        Ok(FullBookResponse {
            schema_version: 1,
            market_seq: sequence,
            counts: BookCounts {
                bids: bids_total,
                asks: asks_total,
                stops: stops_total,
            },
            bids,
            asks,
            stops,
            depth: DepthState {
                bids: depth_from_orders(&orders, OrderSide::Buy, usize::MAX),
                asks: depth_from_orders(&orders, OrderSide::Sell, usize::MAX),
            },
            truncated: limit != usize::MAX
                && (limit < bids_total || limit < asks_total || limit < stops_total),
        })
    }

    pub async fn events(
        &self,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<EventsPage, ApiError> {
        let request = EventsRequest {
            after,
            limit: Some(limit),
        };
        let page = self
            .client
            .events(&request)
            .await
            .map_err(client_error)?
            .map_err(|error| ApiError::unavailable(error.to_string()))?;
        Ok(EventsPage {
            schema_version: 1,
            events: page.events,
            next_after: page.next_after,
            oldest_event_id: page.oldest_event_id,
            last_event_id: page.last_event_id,
            has_more: page.has_more,
        })
    }

    pub async fn place(&self, ticket: PlaceTicket) -> Result<OrderResult, ApiError> {
        let symbol_id = self.symbol_id().await?;
        let tif = parse_tif(&ticket.tif, ticket.expiry_ms)?;
        let maker_fee_rate = validate_fee_rate(ticket.maker_fee_rate, "maker_fee_rate")?;
        let taker_fee_rate = validate_fee_rate(ticket.taker_fee_rate, "taker_fee_rate")?;

        let mut order = Order::new(
            ticket.side,
            ticket.price.unwrap_or(Fixed::ZERO),
            ticket.volume,
            0,
        );
        order.live = ticket.order_type;
        order.symbol_id = symbol_id;
        order.volume_unit = ticket.volume_unit;
        order.tif = tif;
        order.post_only = ticket.post_only;
        order.stop_price = ticket.stop_price.unwrap_or(Fixed::ZERO);
        order.display_volume = ticket.display_volume.unwrap_or(Fixed::ZERO);
        order.maker_fee_rate = maker_fee_rate;
        order.taker_fee_rate = taker_fee_rate;

        self.write(MatchEngineRequest::Place {
            order,
            client_order_id: ticket.client_order_id,
        })
        .await
    }

    pub async fn cancel(&self, request: CancelTicket) -> Result<OrderResult, ApiError> {
        let mut order = Order::new(request.side, request.price, Fixed::ZERO, request.sequence);
        order.symbol_id = self.symbol_id().await?;
        self.write(MatchEngineRequest::Cancel { order }).await
    }

    pub async fn amend(&self, request: AmendTicket) -> Result<OrderResult, ApiError> {
        let mut order = Order::new(request.side, request.price, Fixed::ZERO, request.sequence);
        order.symbol_id = self.symbol_id().await?;
        self.write(MatchEngineRequest::Amend {
            order,
            price: request.new_price,
            volume: request.new_volume,
        })
        .await
    }

    pub async fn configure_symbol(&self, config: SymbolConfig) -> Result<OrderResult, ApiError> {
        if self.sequence().await? > 0 {
            return Err(ApiError::conflict(
                "symbol_locked",
                "trading has started; the symbol configuration is locked",
            ));
        }
        config
            .validate()
            .map_err(|error| ApiError::bad_request(error.to_string()))?;
        self.write(MatchEngineRequest::ConfigureSymbol { config })
            .await
    }

    async fn write(&self, request: MatchEngineRequest) -> Result<OrderResult, ApiError> {
        match self.client.write(&request).await {
            Ok(Ok(response)) => Ok(order_result(response)),
            Ok(Err(error)) => Err(write_error(error)),
            Err(error) => Err(client_error(error)),
        }
    }

    async fn symbol_id(&self) -> Result<u32, ApiError> {
        self.read_text("symbol_id")
            .await?
            .parse()
            .map_err(|error| ApiError::decode(format!("symbol_id: {error}")))
    }

    async fn sequence(&self) -> Result<u64, ApiError> {
        self.read_text("orderbook_sequence")
            .await?
            .parse()
            .map_err(|error| ApiError::decode(format!("orderbook_sequence: {error}")))
    }

    async fn read_text(&self, key: &str) -> Result<String, ApiError> {
        self.client
            .read(&key.to_string())
            .await
            .map_err(client_error)
    }

    async fn read_json<T: DeserializeOwned>(&self, key: &str) -> Result<T, ApiError> {
        let raw = self.read_text(key).await?;
        serde_json::from_str(&raw)
            .map_err(|error| ApiError::decode(format!("{key} decode failed: {error}")))
    }

    async fn read_optional_fixed(&self, key: &str) -> Result<Option<Fixed>, ApiError> {
        let raw = self.read_text(key).await?;
        if raw.is_empty() || raw == "null" {
            return Ok(None);
        }
        raw.parse()
            .map(Some)
            .map_err(|error| ApiError::decode(format!("{key}: {error}")))
    }
}

fn depth_from_orders(orders: &[Order], side: OrderSide, levels: usize) -> Vec<DepthLevel> {
    if levels == 0 {
        return Vec::new();
    }
    let mut side_orders: Vec<&Order> = orders.iter().filter(|order| order.side == side).collect();
    match side {
        OrderSide::Buy => {
            side_orders.sort_by(|a, b| b.price.cmp(&a.price).then(a.sequence.cmp(&b.sequence)))
        }
        OrderSide::Sell => {
            side_orders.sort_by(|a, b| a.price.cmp(&b.price).then(a.sequence.cmp(&b.sequence)))
        }
    }

    let mut depth: Vec<DepthLevel> = Vec::new();
    for order in side_orders {
        if let Some(level) = depth.last_mut()
            && level.price == order.price
        {
            level.volume += order.volume;
            level.orders += 1;
            continue;
        }
        if depth.len() >= levels {
            break;
        }
        depth.push(DepthLevel {
            price: order.price,
            volume: order.volume,
            orders: 1,
        });
    }
    depth
}

fn order_result(response: ClientWriteResponse<C>) -> OrderResult {
    OrderResult {
        schema_version: 1,
        sequence: response.data.sequence,
        log_index: response.log_id.index,
        events: response.data.events,
        events_truncated: response.data.events_truncated,
        error: response.data.error,
    }
}

/// A stable error envelope for the demo API.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub message: String,
    pub leader_id: Option<u64>,
    pub leader_addr: Option<String>,
}

impl ApiError {
    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad_request", message)
    }

    pub fn conflict(kind: &'static str, message: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, kind, message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    pub fn decode(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_GATEWAY, "decode", message)
    }

    fn new(status: StatusCode, kind: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            leader_id: None,
            leader_addr: None,
        }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind, self.message)
    }
}

impl std::error::Error for ApiError {}

fn client_error(error: ClientError<C>) -> ApiError {
    let status = match &error {
        ClientError::NoLeader => StatusCode::SERVICE_UNAVAILABLE,
        ClientError::Timeout => StatusCode::GATEWAY_TIMEOUT,
        ClientError::Api { status, .. } => {
            StatusCode::from_u16(*status).unwrap_or(StatusCode::BAD_GATEWAY)
        }
        ClientError::Rpc(_) | ClientError::Decode(_) => StatusCode::BAD_GATEWAY,
    };
    let kind = match &error {
        ClientError::NoLeader => "no_leader",
        ClientError::Timeout => "timeout",
        ClientError::Api { .. } => "bad_request",
        ClientError::Rpc(_) | ClientError::Decode(_) => "transport",
    };
    ApiError::new(status, kind, error.to_string())
}

fn write_error(error: ClientWriteError<C>) -> ApiError {
    let forward = match &error {
        ClientWriteError::ForwardToLeader(forward)
        | ClientWriteError::LogEntryDiscarded(forward) => Some(forward),
        _ => None,
    };
    let mut api = ApiError::conflict("write_rejected", error.to_string());
    if let Some(forward) = forward {
        api.leader_id = forward.leader_id;
        api.leader_addr = forward.leader_node.as_ref().map(|node| node.addr.clone());
    }
    api
}

fn parse_tif(tif: &str, expiry_ms: Option<u64>) -> Result<TimeInForce, ApiError> {
    match tif.to_ascii_uppercase().as_str() {
        "" | "GTC" => Ok(TimeInForce::Gtc),
        "IOC" => Ok(TimeInForce::Ioc),
        "FOK" => Ok(TimeInForce::Fok),
        "DAY" => Ok(TimeInForce::Day),
        "GTD" => {
            let expiry =
                expiry_ms.ok_or_else(|| ApiError::bad_request("GTD requires expiry_ms"))?;
            if expiry == 0 {
                return Err(ApiError::bad_request(
                    "GTD expiry_ms must be greater than 0",
                ));
            }
            Ok(TimeInForce::Gtd(expiry))
        }
        other => Err(ApiError::bad_request(format!(
            "unsupported time in force: {other}"
        ))),
    }
}

fn validate_fee_rate(rate: Fixed, field: &str) -> Result<Fixed, ApiError> {
    if !(MIN_FEE_RATE..=MAX_FEE_RATE).contains(&rate) {
        return Err(ApiError::bad_request(format!(
            "{field} must be between -1 and 1 (0.001 = 10 bps)"
        )));
    }
    Ok(rate)
}

#[derive(Serialize)]
pub struct StateResponse {
    pub schema_version: u8,
    pub node: NodeState,
    pub symbol: SymbolState,
    pub book: BookState,
    pub orders: OrdersState,
    pub stops: OrdersState,
    pub events: EventsState,
    pub market_data: MarketDataState,
}

#[derive(Serialize)]
pub struct NodeState {
    pub id: u64,
    pub http_addr: String,
    pub is_leader: bool,
    pub leader_id: Option<u64>,
    pub leader_addr: Option<String>,
    pub term: u64,
    pub raft_state: String,
    pub healthy: bool,
    pub ready: bool,
}

#[derive(Serialize)]
pub struct SymbolState {
    pub config: SymbolConfig,
    pub locked: bool,
}

#[derive(Serialize)]
pub struct BookState {
    pub sequence: u64,
    pub spread: Option<Fixed>,
    pub last_trade_price: Option<Fixed>,
    pub depth: DepthState,
}

#[derive(Serialize)]
pub struct DepthState {
    pub bids: Vec<DepthLevel>,
    pub asks: Vec<DepthLevel>,
}

#[derive(Serialize)]
pub struct OrdersState {
    pub total: usize,
    pub truncated: bool,
    pub items: Vec<Order>,
}

#[derive(Serialize)]
pub struct EventsState {
    pub oldest_event_id: Option<EventId>,
    pub last_event_id: Option<EventId>,
    pub retained_events: usize,
    pub evictions_total: u64,
    pub retention_limit: usize,
}

#[derive(Serialize)]
pub struct MarketDataState {
    pub enabled: bool,
    pub status: Option<Value>,
}

#[derive(Serialize)]
pub struct FullBookResponse {
    pub schema_version: u8,
    pub market_seq: u64,
    pub counts: BookCounts,
    pub bids: Vec<Order>,
    pub asks: Vec<Order>,
    pub stops: Vec<Order>,
    pub depth: DepthState,
    pub truncated: bool,
}

#[derive(Serialize)]
pub struct BookCounts {
    pub bids: usize,
    pub asks: usize,
    pub stops: usize,
}

#[derive(Serialize)]
pub struct EventsPage {
    pub schema_version: u8,
    pub events: Vec<MatchEvent>,
    pub next_after: Option<EventId>,
    pub oldest_event_id: Option<EventId>,
    pub last_event_id: Option<EventId>,
    pub has_more: bool,
}

#[derive(Serialize)]
pub struct OrderResult {
    pub schema_version: u8,
    pub sequence: Option<u64>,
    pub log_index: u64,
    pub events: Vec<MatchEvent>,
    pub events_truncated: bool,
    pub error: Option<String>,
}

#[derive(Deserialize)]
pub struct PlaceTicket {
    pub side: OrderSide,
    #[serde(rename = "type")]
    pub order_type: OrderType,
    #[serde(default)]
    pub tif: String,
    #[serde(default)]
    pub price: Option<Fixed>,
    pub volume: Fixed,
    #[serde(default)]
    pub volume_unit: VolumeUnit,
    #[serde(default)]
    pub post_only: bool,
    #[serde(default)]
    pub stop_price: Option<Fixed>,
    #[serde(default)]
    pub display_volume: Option<Fixed>,
    #[serde(default)]
    pub expiry_ms: Option<u64>,
    #[serde(default)]
    pub maker_fee_rate: Fixed,
    #[serde(default)]
    pub taker_fee_rate: Fixed,
    #[serde(default)]
    pub client_order_id: Option<String>,
}

#[derive(Deserialize)]
pub struct CancelTicket {
    pub side: OrderSide,
    pub price: Fixed,
    pub sequence: u64,
}

#[derive(Deserialize)]
pub struct AmendTicket {
    pub side: OrderSide,
    pub price: Fixed,
    pub sequence: u64,
    #[serde(default)]
    pub new_price: Option<Fixed>,
    #[serde(default)]
    pub new_volume: Option<Fixed>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn order(side: OrderSide, price: i128, volume: i128, sequence: u64) -> Order {
        Order::new(
            side,
            Fixed::from_units(price),
            Fixed::from_units(volume),
            sequence,
        )
    }

    #[test]
    fn time_in_force_parsing() {
        assert_eq!(TimeInForce::Gtc, parse_tif("GTC", None).unwrap());
        assert_eq!(TimeInForce::Gtc, parse_tif("", None).unwrap());
        assert_eq!(TimeInForce::Ioc, parse_tif("ioc", None).unwrap());
        assert_eq!(TimeInForce::Fok, parse_tif("FOK", None).unwrap());
        assert_eq!(TimeInForce::Day, parse_tif("Day", None).unwrap());
        assert_eq!(TimeInForce::Gtd(42), parse_tif("GTD", Some(42)).unwrap());
        assert!(parse_tif("GTD", None).is_err());
        assert!(parse_tif("GTD", Some(0)).is_err());
        assert!(parse_tif("forever", None).is_err());
    }

    #[test]
    fn fee_rates_are_exact_fractions() {
        let maker: Fixed = "0.001".parse().unwrap();
        let taker: Fixed = "0.002".parse().unwrap();
        assert_eq!(1_000_000_000_000_000, maker.raw());
        assert_eq!(2_000_000_000_000_000, taker.raw());
        assert_eq!(maker, validate_fee_rate(maker, "maker_fee_rate").unwrap());
        assert_eq!(
            -1_000_000_000_000_000_000,
            validate_fee_rate("-1".parse().unwrap(), "maker_fee_rate")
                .unwrap()
                .raw()
        );
        assert!(validate_fee_rate("1.000000000000000001".parse().unwrap(), "x").is_err());
        assert!(validate_fee_rate("-1.1".parse().unwrap(), "x").is_err());
        assert_eq!(0, validate_fee_rate(Fixed::ZERO, "x").unwrap().raw());
    }

    #[test]
    fn depth_is_best_first_and_aggregated() {
        let orders = vec![
            order(OrderSide::Buy, 100, 2, 1),
            order(OrderSide::Buy, 100, 3, 2),
            order(OrderSide::Buy, 99, 4, 3),
            order(OrderSide::Sell, 101, 5, 4),
            order(OrderSide::Sell, 102, 6, 5),
        ];
        let bids = depth_from_orders(&orders, OrderSide::Buy, 10);
        assert_eq!(2, bids.len());
        assert_eq!(Fixed::from_units(100), bids[0].price);
        assert_eq!(Fixed::from_units(5), bids[0].volume);
        assert_eq!(2, bids[0].orders);
        assert_eq!(Fixed::from_units(99), bids[1].price);

        let asks = depth_from_orders(&orders, OrderSide::Sell, 10);
        assert_eq!(Fixed::from_units(101), asks[0].price);
        assert_eq!(Fixed::from_units(5), asks[0].volume);
    }
}
