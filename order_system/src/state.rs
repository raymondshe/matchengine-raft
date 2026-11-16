use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

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

use crate::freeze::AssetFreeze;
use crate::freeze::FreezeEstimate;
use crate::freeze::OrderEconomics;
use crate::freeze::Product;
use crate::freeze::estimate_order_economics;
use crate::freeze::estimate_ticket_economics;
use crate::freeze::perpetual_supported;
use crate::freeze::summarize_freezes;
use crate::freeze::validate_leverage;

type C = MatchEngineTypeConfig;
pub const MAX_STATE_ORDERS: usize = 200;
pub const DEFAULT_EVENTS_LIMIT: usize = 500;
pub const SCHEMA_VERSION: u8 = 2;
const PERPETUAL_POLICY_HINT: &str =
    "linear perpetual requires fee_asset_policy=quote; configure the symbol before trading";
const PERPETUAL_BACKEND_HINT: &str =
    "perpetual backend is not configured; start the demo with --perp-engine <addr>";

/// One demo backend: a connection to a running engine node and the product
/// whose own orderbook that engine serves. Spot and Perpetual are separate
/// processes/clusters, so an order sequence is only unique within a backend.
pub struct EngineBackend {
    pub product: Product,
    pub engine_id: u64,
    pub http_addr: String,
    client: MatchEngineClient<C>,
    /// Leverage chosen when the demo submitted a perpetual order, keyed by
    /// that backend's sequence. The engine has no leverage field.
    leverages: Mutex<HashMap<u64, u32>>,
}

impl EngineBackend {
    fn new(product: Product, engine_id: u64, http_addr: String) -> Self {
        let client = MatchEngineClient::new(engine_id, http_addr.clone());
        Self {
            product,
            engine_id,
            http_addr,
            client,
            leverages: Mutex::new(HashMap::new()),
        }
    }

    pub fn info(&self) -> BackendInfo {
        BackendInfo {
            product: self.product,
            engine_id: self.engine_id,
            http_addr: self.http_addr.clone(),
        }
    }

    fn remember_leverage(&self, sequence: Option<u64>, leverage: u32) {
        if self.product != Product::Perpetual {
            return;
        }
        if let Some(sequence) = sequence {
            self.leverages
                .lock()
                .expect("leverage metadata lock")
                .insert(sequence, leverage);
        }
    }

    fn take_leverage(&self, sequence: u64) -> Option<u32> {
        self.leverages
            .lock()
            .expect("leverage metadata lock")
            .remove(&sequence)
    }

    fn restore_leverage(&self, sequence: u64, leverage: u32) {
        self.leverages
            .lock()
            .expect("leverage metadata lock")
            .insert(sequence, leverage);
    }

    /// A resting or parked order plus its demo-side product/leverage tag and
    /// freeze estimate. A perpetual order placed before a demo restart has no
    /// remembered leverage, so it stays unestimated until it is cancelled.
    fn desk_orders(&self, orders: &[Order], symbol: &SymbolConfig) -> Vec<DeskOrder> {
        let leverages = self.leverages.lock().expect("leverage metadata lock");
        orders
            .iter()
            .map(|order| {
                let leverage = if self.product == Product::Perpetual {
                    leverages.get(&order.sequence).copied()
                } else {
                    None
                };
                let economics = match (self.product, leverage) {
                    (Product::Perpetual, None) => None,
                    _ => {
                        estimate_order_economics(order, symbol, self.product, leverage.unwrap_or(1))
                    }
                };
                DeskOrder {
                    order: *order,
                    product: self.product,
                    leverage,
                    economics,
                }
            })
            .collect()
    }

    async fn state(&self, depth: usize) -> Result<EngineState, ApiError> {
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

        let mut all_orders = self.desk_orders(&orders, &symbol);
        let all_stops = self.desk_orders(&stops, &symbol);
        let freeze = freeze_state(&all_orders, &all_stops);

        all_orders.sort_by_key(|entry| entry.order.sequence);
        let orders_total = all_orders.len();
        let orders_truncated = orders_total > MAX_STATE_ORDERS;
        all_orders.truncate(MAX_STATE_ORDERS);

        let stops_total = all_stops.len();
        let stops_truncated = stops_total > MAX_STATE_ORDERS;
        let stops: Vec<DeskOrder> = all_stops.into_iter().take(MAX_STATE_ORDERS).collect();

        Ok(EngineState {
            node: NodeState {
                id: self.engine_id,
                http_addr: self.http_addr.clone(),
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
                items: all_orders,
            },
            stops: OrdersState {
                total: stops_total,
                truncated: stops_truncated,
                items: stops,
            },
            freeze,
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

        let symbol: SymbolConfig = self.read_json("symbol_config").await?;
        let all_orders = self.desk_orders(&orders, &symbol);
        let all_stops = self.desk_orders(&stops, &symbol);
        let freeze = freeze_state(&all_orders, &all_stops);

        let mut bids: Vec<DeskOrder> = all_orders
            .iter()
            .filter(|entry| entry.order.side == OrderSide::Buy)
            .cloned()
            .collect();
        let mut asks: Vec<DeskOrder> = all_orders
            .iter()
            .filter(|entry| entry.order.side == OrderSide::Sell)
            .cloned()
            .collect();
        bids.sort_by(|a, b| {
            b.order
                .price
                .cmp(&a.order.price)
                .then(a.order.sequence.cmp(&b.order.sequence))
        });
        asks.sort_by(|a, b| {
            a.order
                .price
                .cmp(&b.order.price)
                .then(a.order.sequence.cmp(&b.order.sequence))
        });
        let bids_total = bids.len();
        let asks_total = asks.len();
        let stops_total = all_stops.len();
        bids.truncate(limit);
        asks.truncate(limit);
        let stops: Vec<DeskOrder> = all_stops.into_iter().take(limit).collect();

        Ok(FullBookResponse {
            schema_version: SCHEMA_VERSION,
            product: self.product,
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
            freeze,
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
            wait_ms: None,
        };
        let page = self
            .client
            .events(&request)
            .await
            .map_err(client_error)?
            .map_err(|error| ApiError::unavailable(error.to_string()))?;
        Ok(EventsPage {
            schema_version: SCHEMA_VERSION,
            product: self.product,
            events: page.events,
            next_after: page.next_after,
            oldest_event_id: page.oldest_event_id,
            last_event_id: page.last_event_id,
            has_more: page.has_more,
        })
    }

    pub async fn place(&self, ticket: PlaceTicket) -> Result<OrderResult, ApiError> {
        let product = self.product;
        let leverage = ticket.leverage.unwrap_or(1);
        let symbol: SymbolConfig = self.read_json("symbol_config").await?;
        validate_product(&symbol, product, leverage)?;
        let order = ticket.build_order(symbol.symbol_id)?;
        let result = self
            .write(MatchEngineRequest::Place {
                order,
                client_order_id: ticket.client_order_id,
            })
            .await?;
        if result.error.is_none() {
            self.remember_leverage(result.sequence, leverage);
        }
        Ok(result)
    }

    /// Previews the order-system freeze and settlement legs for an order
    /// ticket without submitting it. Nothing here is engine state: the demo
    /// derives the estimate from the replicated symbol configuration, the
    /// order fields and the entered fee rates.
    pub async fn preview_freeze(&self, ticket: PlaceTicket) -> Result<FreezePreview, ApiError> {
        let product = self.product;
        let leverage = ticket.leverage.unwrap_or(1);
        if product == Product::Perpetual {
            validate_leverage(leverage).map_err(ApiError::bad_request)?;
        }
        let symbol: SymbolConfig = self.read_json("symbol_config").await?;
        let order = ticket.build_order(symbol.symbol_id)?;
        let fallback_price = self
            .read_optional_fixed("orderbook_last_trade_price")
            .await?;
        let economics =
            estimate_ticket_economics(&order, &symbol, fallback_price, product, leverage);
        let reason = economics.is_none().then(|| {
            if product == Product::Perpetual && !perpetual_supported(&symbol) {
                PERPETUAL_POLICY_HINT.to_string()
            } else {
                match order.live {
                    OrderType::Market => {
                        "no reference price: enter a stop price or trade first".to_string()
                    }
                    _ => "no reference price: enter a limit price".to_string(),
                }
            }
        });
        Ok(FreezePreview {
            schema_version: SCHEMA_VERSION,
            product,
            economics,
            reason,
        })
    }

    pub async fn cancel(&self, request: CancelTicket) -> Result<OrderResult, ApiError> {
        let sequence = request.sequence;
        let mut order = Order::new(request.side, request.price, Fixed::ZERO, sequence);
        order.symbol_id = self.symbol_id().await?;
        let result = self.write(MatchEngineRequest::Cancel { order }).await?;
        if result.error.is_none() {
            self.take_leverage(sequence);
        }
        Ok(result)
    }

    pub async fn amend(&self, request: AmendTicket) -> Result<OrderResult, ApiError> {
        let old_sequence = request.sequence;
        let leverage = self.take_leverage(old_sequence);
        let mut order = Order::new(request.side, request.price, Fixed::ZERO, old_sequence);
        order.symbol_id = self.symbol_id().await?;
        let result = self
            .write(MatchEngineRequest::Amend {
                order,
                price: request.new_price,
                volume: request.new_volume,
            })
            .await?;
        if let Some(leverage) = leverage {
            match (result.error.as_ref(), result.sequence) {
                (None, Some(new_sequence)) => self.restore_leverage(new_sequence, leverage),
                _ => self.restore_leverage(old_sequence, leverage),
            }
        }
        Ok(result)
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
            Ok(Ok(response)) => Ok(order_result(response, self.product)),
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

/// The demo server's two independent engine backends. Spot is required;
/// Perpetual is optional so a single-engine spot-only demo still works.
pub struct DemoApp {
    spot: EngineBackend,
    perpetual: Option<EngineBackend>,
}

impl DemoApp {
    pub fn new(engine_id: u64, engine_addr: String, perpetual: Option<(u64, String)>) -> Self {
        Self {
            spot: EngineBackend::new(Product::Spot, engine_id, engine_addr),
            perpetual: perpetual
                .map(|(engine_id, addr)| EngineBackend::new(Product::Perpetual, engine_id, addr)),
        }
    }

    pub fn backends(&self) -> BackendsState {
        BackendsState {
            spot: self.spot.info(),
            perpetual: self.perpetual.as_ref().map(EngineBackend::info),
        }
    }

    fn backend(&self, product: Product) -> Result<&EngineBackend, ApiError> {
        match product {
            Product::Spot => Ok(&self.spot),
            Product::Perpetual => self
                .perpetual
                .as_ref()
                .ok_or_else(|| ApiError::not_configured(PERPETUAL_BACKEND_HINT)),
        }
    }

    pub async fn state(&self, product: Product, depth: usize) -> Result<StateResponse, ApiError> {
        let backend = self.backend(product)?;
        let state = backend.state(depth).await?;
        Ok(StateResponse {
            schema_version: SCHEMA_VERSION,
            product,
            backend: backend.info(),
            backends: self.backends(),
            node: state.node,
            symbol: state.symbol,
            book: state.book,
            orders: state.orders,
            stops: state.stops,
            freeze: state.freeze,
            events: state.events,
            market_data: state.market_data,
        })
    }

    pub async fn book(&self, product: Product, limit: usize) -> Result<FullBookResponse, ApiError> {
        self.backend(product)?.book(limit).await
    }

    pub async fn events(
        &self,
        product: Product,
        after: Option<EventId>,
        limit: usize,
    ) -> Result<EventsPage, ApiError> {
        self.backend(product)?.events(after, limit).await
    }

    pub async fn place(&self, ticket: PlaceTicket) -> Result<OrderResult, ApiError> {
        self.backend(ticket.product)?.place(ticket).await
    }

    pub async fn preview_freeze(&self, ticket: PlaceTicket) -> Result<FreezePreview, ApiError> {
        self.backend(ticket.product)?.preview_freeze(ticket).await
    }

    pub async fn cancel(&self, request: CancelTicket) -> Result<OrderResult, ApiError> {
        self.backend(request.product)?.cancel(request).await
    }

    pub async fn amend(&self, request: AmendTicket) -> Result<OrderResult, ApiError> {
        self.backend(request.product)?.amend(request).await
    }

    pub async fn configure_symbol(
        &self,
        product: Product,
        config: SymbolConfig,
    ) -> Result<OrderResult, ApiError> {
        self.backend(product)?.configure_symbol(config).await
    }
}

fn validate_product(
    symbol: &SymbolConfig,
    product: Product,
    leverage: u32,
) -> Result<(), ApiError> {
    if product != Product::Perpetual {
        return Ok(());
    }
    validate_leverage(leverage).map_err(ApiError::bad_request)?;
    if !perpetual_supported(symbol) {
        return Err(ApiError::bad_request(PERPETUAL_POLICY_HINT));
    }
    Ok(())
}

fn estimated_freezes(items: &[DeskOrder]) -> impl Iterator<Item = &FreezeEstimate> {
    items
        .iter()
        .filter_map(|entry| entry.economics.as_ref().map(|economics| &economics.freeze))
}

fn freeze_state(orders: &[DeskOrder], stops: &[DeskOrder]) -> FreezeState {
    let total_orders = orders.len() + stops.len();
    let estimated = orders
        .iter()
        .chain(stops.iter())
        .filter(|entry| entry.economics.is_some())
        .count();
    FreezeState {
        orders: summarize_freezes(estimated_freezes(orders)),
        stops: summarize_freezes(estimated_freezes(stops)),
        total: summarize_freezes(estimated_freezes(orders).chain(estimated_freezes(stops))),
        estimated,
        unestimated: total_orders - estimated,
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

fn order_result(response: ClientWriteResponse<C>, product: Product) -> OrderResult {
    OrderResult {
        schema_version: SCHEMA_VERSION,
        product,
        sequence: response.data.sequence,
        log_index: response.log_id.index,
        events: response.data.events.to_vec(),
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

    pub fn not_configured(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "not_configured", message)
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

/// Engine-owned portion of one product's state; `DemoApp` adds the backend
/// metadata shared by both products.
struct EngineState {
    node: NodeState,
    symbol: SymbolState,
    book: BookState,
    orders: OrdersState,
    stops: OrdersState,
    freeze: FreezeState,
    events: EventsState,
    market_data: MarketDataState,
}

#[derive(Serialize)]
pub struct StateResponse {
    pub schema_version: u8,
    pub product: Product,
    pub backend: BackendInfo,
    pub backends: BackendsState,
    pub node: NodeState,
    pub symbol: SymbolState,
    pub book: BookState,
    pub orders: OrdersState,
    pub stops: OrdersState,
    pub freeze: FreezeState,
    pub events: EventsState,
    pub market_data: MarketDataState,
}

/// One configured engine backend and the product whose orderbook it serves.
#[derive(Serialize, Clone)]
pub struct BackendInfo {
    pub product: Product,
    pub engine_id: u64,
    pub http_addr: String,
}

/// Both demo backends. `perpetual` is `null` when the demo was started without
/// `--perp-engine`, in which case the UI disables the Perpetual product.
#[derive(Serialize)]
pub struct BackendsState {
    pub spot: BackendInfo,
    pub perpetual: Option<BackendInfo>,
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

/// A resting or parked order plus its demo-side freeze/settlement estimate.
#[derive(Serialize, Clone)]
pub struct DeskOrder {
    pub order: Order,
    pub product: Product,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leverage: Option<u32>,
    pub economics: Option<OrderEconomics>,
}

#[derive(Serialize)]
pub struct OrdersState {
    pub total: usize,
    pub truncated: bool,
    pub items: Vec<DeskOrder>,
}

/// Aggregate estimated freezes for every order the demo can price.
#[derive(Serialize)]
pub struct FreezeState {
    pub orders: Vec<AssetFreeze>,
    pub stops: Vec<AssetFreeze>,
    pub total: Vec<AssetFreeze>,
    pub estimated: usize,
    pub unestimated: usize,
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
    pub product: Product,
    pub market_seq: u64,
    pub counts: BookCounts,
    pub bids: Vec<DeskOrder>,
    pub asks: Vec<DeskOrder>,
    pub stops: Vec<DeskOrder>,
    pub depth: DepthState,
    pub freeze: FreezeState,
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
    pub product: Product,
    pub events: Vec<MatchEvent>,
    pub next_after: Option<EventId>,
    pub oldest_event_id: Option<EventId>,
    pub last_event_id: Option<EventId>,
    pub has_more: bool,
}

#[derive(Serialize)]
pub struct OrderResult {
    pub schema_version: u8,
    pub product: Product,
    pub sequence: Option<u64>,
    pub log_index: u64,
    pub events: Vec<MatchEvent>,
    pub events_truncated: bool,
    pub error: Option<String>,
}

/// Demo-side estimate for a ticket that has not been submitted.
#[derive(Serialize)]
pub struct FreezePreview {
    pub schema_version: u8,
    pub product: Product,
    pub economics: Option<OrderEconomics>,
    pub reason: Option<String>,
}

#[derive(Deserialize)]
pub struct PlaceTicket {
    pub side: OrderSide,
    #[serde(default)]
    pub product: Product,
    #[serde(default)]
    pub leverage: Option<u32>,
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

impl PlaceTicket {
    fn build_order(&self, symbol_id: u32) -> Result<Order, ApiError> {
        let tif = parse_tif(&self.tif, self.expiry_ms)?;
        let maker_fee_rate = validate_fee_rate(self.maker_fee_rate, "maker_fee_rate")?;
        let taker_fee_rate = validate_fee_rate(self.taker_fee_rate, "taker_fee_rate")?;

        let mut order = Order::new(self.side, self.price.unwrap_or(Fixed::ZERO), self.volume, 0);
        order.live = self.order_type;
        order.symbol_id = symbol_id;
        order.volume_unit = self.volume_unit;
        order.tif = tif;
        order.post_only = self.post_only;
        order.stop_price = self.stop_price.unwrap_or(Fixed::ZERO);
        order.display_volume = self.display_volume.unwrap_or(Fixed::ZERO);
        order.maker_fee_rate = maker_fee_rate;
        order.taker_fee_rate = taker_fee_rate;
        Ok(order)
    }
}

#[derive(Deserialize)]
pub struct CancelTicket {
    #[serde(default)]
    pub product: Product,
    pub side: OrderSide,
    pub price: Fixed,
    pub sequence: u64,
}

#[derive(Deserialize)]
pub struct AmendTicket {
    #[serde(default)]
    pub product: Product,
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

    use matchengine_raft::order_book::FeeAssetPolicy;

    fn order(side: OrderSide, price: i128, volume: i128, sequence: u64) -> Order {
        Order::new(
            side,
            Fixed::from_units(price),
            Fixed::from_units(volume),
            sequence,
        )
    }

    fn spot_economics(order: &Order, symbol: &SymbolConfig) -> Option<OrderEconomics> {
        estimate_order_economics(order, symbol, Product::Spot, 1)
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
    fn desk_order_serializes_economics_alongside_order_fields() {
        let symbol = SymbolConfig::default();
        let order = order(OrderSide::Buy, 100, 1, 7);
        let entry = DeskOrder {
            order,
            product: Product::Spot,
            leverage: None,
            economics: spot_economics(&order, &symbol),
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!(7, value["order"]["sequence"]);
        assert_eq!("100", value["order"]["price"]);
        assert_eq!("spot", value["product"]);
        assert!(value.get("leverage").is_none());
        assert_eq!("USDT", value["economics"]["freeze"]["asset"]);
        assert_eq!("100", value["economics"]["freeze"]["amount"]);
        assert_eq!("BTC", value["economics"]["taker"]["fee"]["asset"]);
    }

    #[test]
    fn perpetual_desk_order_serializes_margin_terms() {
        let symbol = SymbolConfig::default().with_fee_asset_policy(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 100, 1, 8);
        order.maker_fee_rate = "0.002".parse().unwrap();
        order.taker_fee_rate = "0.002".parse().unwrap();
        let entry = DeskOrder {
            order,
            product: Product::Perpetual,
            leverage: Some(10),
            economics: estimate_order_economics(&order, &symbol, Product::Perpetual, 10),
        };
        let value = serde_json::to_value(&entry).unwrap();
        assert_eq!("perpetual", value["product"]);
        assert_eq!(10, value["leverage"]);
        assert_eq!("USDT", value["economics"]["freeze"]["asset"]);
        assert_eq!("10.2", value["economics"]["freeze"]["amount"]);
        assert_eq!(
            "perpetual_margin_plus_fee",
            value["economics"]["freeze"]["basis"]
        );
        assert!(value["economics"]["taker"].is_null());
        assert_eq!(10, value["economics"]["perpetual"]["leverage"]);
        assert_eq!("10", value["economics"]["perpetual"]["initial_margin"]);
        assert_eq!("0.2", value["economics"]["perpetual"]["fee"]["amount"]);
    }

    #[test]
    fn ticket_product_defaults_to_spot_and_accepts_perpetual_leverage() {
        let spot: PlaceTicket =
            serde_json::from_str(r#"{"side":"Buy","type":"Limit","volume":"1","price":"100"}"#)
                .unwrap();
        assert_eq!(Product::Spot, spot.product);
        assert_eq!(None, spot.leverage);

        let perpetual: PlaceTicket = serde_json::from_str(
            r#"{"side":"Sell","product":"perpetual","leverage":10,"type":"Limit","volume":"1","price":"100"}"#,
        )
        .unwrap();
        assert_eq!(Product::Perpetual, perpetual.product);
        assert_eq!(Some(10), perpetual.leverage);
    }

    #[test]
    fn cancel_and_amend_tickets_default_to_spot() {
        let cancel: CancelTicket =
            serde_json::from_str(r#"{"side":"Buy","price":"100","sequence":7}"#).unwrap();
        assert_eq!(Product::Spot, cancel.product);
        assert_eq!(7, cancel.sequence);

        let amend: AmendTicket = serde_json::from_str(
            r#"{"product":"perpetual","side":"Sell","price":"100","sequence":8,"new_price":"101"}"#,
        )
        .unwrap();
        assert_eq!(Product::Perpetual, amend.product);
        assert_eq!(Some(Fixed::from_units(101)), amend.new_price);
    }

    #[test]
    fn demo_app_reports_the_optional_perpetual_backend() {
        let spot_only = DemoApp::new(1, "127.0.0.1:24401".to_string(), None);
        let backends = spot_only.backends();
        assert_eq!(Product::Spot, backends.spot.product);
        assert_eq!("127.0.0.1:24401", backends.spot.http_addr);
        assert!(backends.perpetual.is_none());
        let error = match spot_only.backend(Product::Perpetual) {
            Ok(_) => panic!("perpetual backend unexpectedly configured"),
            Err(error) => error,
        };
        assert_eq!("not_configured", error.kind);

        let pair = DemoApp::new(
            1,
            "127.0.0.1:24401".to_string(),
            Some((2, "127.0.0.1:24402".to_string())),
        );
        let backends = pair.backends();
        let perpetual = backends.perpetual.expect("perpetual backend");
        assert_eq!(Product::Perpetual, perpetual.product);
        assert_eq!(2, perpetual.engine_id);
        assert_eq!("127.0.0.1:24402", perpetual.http_addr);
    }

    #[test]
    fn each_backend_labels_its_orders_and_remembers_leverage() {
        let spot = EngineBackend::new(Product::Spot, 1, "127.0.0.1:24401".to_string());
        let perpetual = EngineBackend::new(Product::Perpetual, 1, "127.0.0.1:24402".to_string());
        let symbol = SymbolConfig::default();
        let mut resting = order(OrderSide::Buy, 100, 1, 7);
        resting.maker_fee_rate = "0.002".parse().unwrap();
        resting.taker_fee_rate = "0.002".parse().unwrap();

        let spot_entries = spot.desk_orders(std::slice::from_ref(&resting), &symbol);
        assert_eq!(Product::Spot, spot_entries[0].product);
        assert_eq!(None, spot_entries[0].leverage);
        assert!(spot_entries[0].economics.is_some());

        perpetual.remember_leverage(Some(7), 10);
        let perp_symbol = SymbolConfig::default().with_fee_asset_policy(FeeAssetPolicy::Quote);
        let perp_entries = perpetual.desk_orders(std::slice::from_ref(&resting), &perp_symbol);
        assert_eq!(Product::Perpetual, perp_entries[0].product);
        assert_eq!(Some(10), perp_entries[0].leverage);
        assert_eq!(
            "10.2",
            perp_entries[0]
                .economics
                .as_ref()
                .unwrap()
                .freeze
                .amount
                .to_string()
        );

        // A sequence placed before a demo restart has no remembered leverage.
        let unknown = order(OrderSide::Buy, 100, 1, 8);
        let entries = perpetual.desk_orders(std::slice::from_ref(&unknown), &perp_symbol);
        assert_eq!(None, entries[0].leverage);
        assert!(entries[0].economics.is_none());
    }

    #[test]
    fn validate_product_guards_perpetual_leverage_and_policy() {
        let quote_symbol = SymbolConfig::default().with_fee_asset_policy(FeeAssetPolicy::Quote);
        let cash_symbol = SymbolConfig::default().with_fee_asset_policy(FeeAssetPolicy::Received);

        // Spot ignores the leverage and the fee policy entirely.
        assert!(validate_product(&cash_symbol, Product::Spot, 0).is_ok());
        assert!(validate_product(&cash_symbol, Product::Spot, 999).is_ok());

        assert!(validate_product(&quote_symbol, Product::Perpetual, 1).is_ok());
        assert!(validate_product(&quote_symbol, Product::Perpetual, 100).is_ok());
        assert_eq!(
            "leverage must be between 1 and 100",
            validate_product(&quote_symbol, Product::Perpetual, 0)
                .unwrap_err()
                .message
        );
        assert_eq!(
            PERPETUAL_POLICY_HINT,
            validate_product(&cash_symbol, Product::Perpetual, 10)
                .unwrap_err()
                .message
        );
    }

    #[test]
    fn leverage_metadata_moves_with_an_amend_and_ignores_spot() {
        // The spot backend never records leverage, even if a client sends it.
        let spot = EngineBackend::new(Product::Spot, 1, "127.0.0.1:24401".to_string());
        spot.remember_leverage(Some(7), 25);
        assert_eq!(None, spot.take_leverage(7));

        // The perpetual backend keys leverage by sequence: `take` removes it
        // (the amend path), and `restore` re-inserts it under the sequence the
        // engine assigned, so the tag survives a priority-losing amendment.
        let perpetual = EngineBackend::new(Product::Perpetual, 1, "127.0.0.1:24402".to_string());
        perpetual.remember_leverage(Some(7), 25);
        let moved = perpetual.take_leverage(7);
        assert_eq!(Some(25), moved);
        assert_eq!(None, perpetual.take_leverage(7));
        perpetual.restore_leverage(9, moved.unwrap());
        assert_eq!(Some(25), perpetual.take_leverage(9));
    }

    #[test]
    fn perpetual_ticket_leverage_is_optional_and_must_be_an_integer() {
        let optional: PlaceTicket = serde_json::from_str(
            r#"{"side":"Buy","product":"perpetual","type":"Limit","volume":"1","price":"100"}"#,
        )
        .unwrap();
        assert_eq!(Product::Perpetual, optional.product);
        assert_eq!(None, optional.leverage);

        // An out-of-range integer still deserializes; `validate_product`
        // rejects it before any write is proposed.
        let zero: PlaceTicket = serde_json::from_str(
            r#"{"side":"Buy","product":"perpetual","leverage":0,"type":"Limit","volume":"1","price":"100"}"#,
        )
        .unwrap();
        assert_eq!(Some(0), zero.leverage);

        // A fractional leverage is a malformed ticket, not a rounded one.
        assert!(serde_json::from_str::<PlaceTicket>(
            r#"{"side":"Buy","product":"perpetual","leverage":10.5,"type":"Limit","volume":"1","price":"100"}"#
        )
        .is_err());
    }

    #[test]
    fn freeze_state_aggregates_orders_and_stops_by_asset() {
        let symbol = SymbolConfig::default();
        let buy = order(OrderSide::Buy, 100, 2, 1);
        let sell = order(OrderSide::Sell, 100, 1, 2);
        let orders = vec![DeskOrder {
            order: buy,
            product: Product::Spot,
            leverage: None,
            economics: spot_economics(&buy, &symbol),
        }];
        let stops = vec![DeskOrder {
            order: sell,
            product: Product::Spot,
            leverage: None,
            economics: spot_economics(&sell, &symbol),
        }];
        let freeze = freeze_state(&orders, &stops);
        assert_eq!(2, freeze.estimated);
        assert_eq!(0, freeze.unestimated);
        assert_eq!(1, freeze.orders.len());
        assert_eq!("USDT", freeze.orders[0].asset);
        assert_eq!("200", freeze.orders[0].amount.to_string());
        assert_eq!(1, freeze.stops.len());
        assert_eq!("BTC", freeze.stops[0].asset);
        assert_eq!(2, freeze.total.len());
        assert_eq!("BTC", freeze.total[0].asset);
        assert_eq!("USDT", freeze.total[1].asset);
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
