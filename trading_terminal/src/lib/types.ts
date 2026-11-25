// Shared protocol types.
//
// The market-data shapes mirror `market_data_system/src/protocol.rs` (the
// Binance-style public payloads); the order-desk shapes mirror the JSON the
// order-system BFF returns (`order_system/src/api.rs` + `state.rs`).

// ---------- market data: /info and /ready ----------

export interface SymbolInfo {
  symbol: string;
  symbol_id: number;
  base_asset: string;
  quote_asset: string;
}

export interface MdInfo {
  symbols: SymbolInfo[];
  fast_only: boolean;
  streams: string[];
}

export interface MdSymbolReady {
  ready: boolean;
  snapshot_seen: boolean;
  depth_stale: boolean;
  heartbeat_fresh: boolean;
  last_ts_ms: number;
  epoch: number;
  leader_id: number | null;
}

export interface MdReady {
  ready: boolean;
  symbols: Record<string, MdSymbolReady>;
}

// ---------- market data: stream payloads ----------

export type Speed = "100ms" | "1000ms";

export interface BookTickerFrame {
  e?: "bookTicker";
  u: number;
  s: string;
  b: string;
  B: string;
  a: string;
  A: string;
}

export interface TradeFrame {
  e: "trade";
  E: number;
  s: string;
  t: number;
  p: string;
  q: string;
  T: number;
  m: boolean;
  M: boolean;
}

/** `[price, volume]` pairs as strings. */
export type DepthLevel = [string, string];

/** `[price, volume, orders]` snapshot rows. */
export type SnapshotLevel = [string, string, number];

export interface DepthSnapshotFrame {
  e: "depthSnapshot";
  E: number;
  s: string;
  lastUpdateId: number;
  bids: SnapshotLevel[];
  asks: SnapshotLevel[];
}

export interface DepthUpdateFrame {
  e: "depthUpdate";
  E: number;
  s: string;
  U: number;
  u: number;
  b: DepthLevel[];
  a: DepthLevel[];
}

export interface KlineFrame {
  e: "kline";
  E: number;
  s: string;
  k: {
    t: number;
    T: number;
    s: string;
    i: string;
    o: string;
    c: string;
    h: string;
    l: string;
    v: string;
    n: number;
    x: boolean;
    q: string;
  };
}

export interface StatusFrame {
  e: "status";
  E: number;
  s: string;
  lastUpdateId: number;
  lastLogIndex: number;
  lastOrdinal: number;
  bookSequence: number;
  pendingStops: number;
  epoch: number;
  leaderId: number;
  ready: boolean;
}

export interface ResyncFrame {
  e: "resync";
  s: string;
  reason: string;
  dropped: number;
}

export interface ControlResultFrame {
  result: string[] | null;
  id: number;
}

export interface ControlErrorFrame {
  code: number;
  msg: string;
  id?: number;
}

export type MarketFrame =
  | BookTickerFrame
  | TradeFrame
  | DepthSnapshotFrame
  | DepthUpdateFrame
  | KlineFrame
  | StatusFrame
  | ResyncFrame
  | ControlResultFrame
  | ControlErrorFrame;

/** A frame as delivered on the wire, with the combined-stream wrapper. */
export interface WireFrame {
  stream?: string;
  data?: MarketFrame;
}

// ---------- order desk REST ----------

export interface AssetInfo {
  code: string;
  class: string;
  decimals: number;
}

export interface SymbolConfig {
  symbol_id: number;
  base_asset: AssetInfo;
  quote_asset: AssetInfo;
  price_decimals: number;
  price_tick: string;
  base_lot: string;
  fee_asset_policy: string;
}

export interface DeskSymbol {
  config: SymbolConfig;
  locked: boolean;
}

export interface DeskNode {
  id: number;
  http_addr: string;
  healthy: boolean;
  ready: boolean;
  is_leader: boolean;
  leader_id: number | null;
  leader_addr: string | null;
  raft_state: string;
  term: number;
}

export interface DepthEntry {
  price: string;
  volume: string;
  orders: number;
}

export interface DeskBook {
  sequence: number;
  spread: string | null;
  last_trade_price: string | null;
  depth: { bids: DepthEntry[]; asks: DepthEntry[] };
}

export type Economics =
  | {
      freeze: Freeze;
      maker: SettlementLeg | null;
      taker: SettlementLeg | null;
      perpetual?: never;
      fee_funding?: string;
    }
  | {
      freeze: Freeze;
      maker?: never;
      taker?: never;
      perpetual: {
        leverage: number;
        position: string;
        notional: string;
        initial_margin: string;
        fee: Money;
        buffer: string;
      };
      fee_funding?: string;
    };

export interface Money {
  amount: string;
  asset: string;
}

export interface Freeze {
  amount: string;
  asset: string;
  role: string;
  basis: string;
  fee_rate: string;
  reference_price: string;
}

export interface SettlementLeg {
  role: string;
  delivers: Money;
  receives: Money;
  fee: Money;
  fee_rate: string;
}

export interface DeskOrderEntry {
  order: {
    sequence: number;
    side: string;
    price: string;
    volume: string;
    live: string;
    tif: unknown;
    post_only: boolean;
    display_volume: string;
    hidden_volume: string;
    stop_price: string;
  };
  economics: Economics | null;
}

export interface DeskState {
  backends?: DeskBackends;
  node: DeskNode;
  symbol: DeskSymbol;
  book: DeskBook;
  orders: { items: DeskOrderEntry[]; total: number };
  stops: { items: DeskOrderEntry[] };
  freeze: { total: { amount: string; asset: string; orders: number }[]; estimated: number; unestimated: number } | null;
}

export interface DeskTarget {
  id: string;
  label?: string;
  http_addr: string;
  product?: string;
}

export interface DeskBackends {
  targets: DeskTarget[];
  user_id_min?: number;
  user_id_max?: number;
}

export interface DeskEvent {
  event_id: { log_index: number; ordinal: number };
  kind: string;
  side: string;
  order_sequence: number;
  counterparty_sequence: number | null;
  price: string;
  volume: string;
  quote_volume: string;
  taker_fee: string;
  taker_fee_asset: string | null;
  maker_fee: string;
  maker_fee_asset: string | null;
  error: string[];
  reason?: string;
}

export interface DeskEventPage {
  events: DeskEvent[];
  oldest_event_id: { log_index: number; ordinal: number } | null;
  next_after: { log_index: number; ordinal: number } | null;
  has_more: boolean;
}

export interface DeskWriteResult {
  product?: string;
  sequence?: number;
  log_index?: number;
  events?: DeskEvent[];
  error?: string;
}

// ---------- order ticket ----------

export type Side = "Buy" | "Sell";
export type OrderType = "Limit" | "Market";
export type Tif = "GTC" | "IOC" | "FOK" | "GTD";
export type VolumeUnit = "base" | "quote";

export interface TicketForm {
  side: Side;
  type: OrderType;
  tif: Tif;
  price: string;
  volume: string;
  volumeUnit: VolumeUnit;
  postOnly: boolean;
  iceberg: boolean;
  displayVolume: string;
  stopPrice: string;
  expiry: string;
  makerFeeRate: string;
  takerFeeRate: string;
  leverage: number;
}

export interface PlaceBody {
  side: Side;
  user_id: number;
  target: string;
  type: OrderType;
  tif?: Tif;
  volume: string;
  volume_unit: VolumeUnit;
  post_only?: boolean;
  maker_fee_rate?: string;
  taker_fee_rate?: string;
  price?: string;
  stop_price?: string;
  display_volume?: string;
  expiry_ms?: number;
  client_order_id?: string;
  leverage?: number;
}
