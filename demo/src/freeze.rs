//! Demo-side freeze and settlement estimates.
//!
//! The matching engine is balance-agnostic: it never holds a freeze. The demo
//! therefore reconstructs the order-system policy from the replicated symbol
//! configuration, the order fields and the fee rates, and labels the result as
//! an estimate. The rules mirror `docs/FEE_DEDUCTION.md` §6.6.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde::Serialize;

use matchengine_raft::order_book::{
    FeeAsset, FeeAssetPolicy, Fixed, Order, OrderSide, OrderType, SymbolConfig, VolumeUnit,
};

/// Instrument overlay modelled by the demo order desk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Product {
    /// Cash/spot order; the engine's normal base/quote delivery.
    #[default]
    Spot,
    /// Linear USDT-margined perpetual overlay. The engine still matches the
    /// order as its configured symbol; this demo only estimates margin.
    Perpetual,
}

/// Direction of the perpetual position opened by the order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PositionSide {
    Long,
    Short,
}

/// Accepted leverage range for the demo perpetual overlay.
pub const MIN_PERPETUAL_LEVERAGE: u32 = 1;
pub const MAX_PERPETUAL_LEVERAGE: u32 = 100;

/// Role used for the freeze estimate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreezeRole {
    Maker,
    Taker,
}

/// Why the selected asset is frozen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreezeBasis {
    /// `Received` buy: quote notional paid to the seller.
    QuoteNotional,
    /// `Received`/`Quote` sell: base quantity delivered.
    BasePrincipal,
    /// `Quote` buy: quote notional plus the quote fee.
    QuoteNotionalPlusFee,
    /// `Base`: quote collateral plus a quote reserve for the base fee.
    QuoteCollateralPlusFee,
    /// Linear perpetual: initial margin plus the estimated quote fee.
    PerpetualMarginPlusFee,
}

/// How clearing funds the fee for this side under the pair's policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeFunding {
    /// `Received` buy: withhold the base fee from the base delivery.
    WithholdFromBaseDelivery,
    /// `Received`/`Quote` sell: withhold the quote fee from the proceeds.
    WithholdFromQuoteProceeds,
    /// `Quote` buy: deduct the quote fee from the frozen quote.
    DeductFromQuoteFreeze,
    /// `Base` buy: withhold the base fee from the base delivery.
    BaseFromBaseDelivery,
    /// `Base` sell: withhold or acquire the base fee against the quote collateral.
    BaseAgainstQuoteCollateral,
    /// Linear perpetual: deduct the quote fee from the frozen margin.
    DeductFromMargin,
}

/// One asset amount, serialized as an exact decimal string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetAmount {
    pub asset: String,
    pub amount: Fixed,
}

/// Assets delivered and received by one side of a fill, plus its fee.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SettlementLeg {
    pub role: FreezeRole,
    /// Signed maker/taker rate, so a negative value is visibly a rebate.
    pub fee_rate: Fixed,
    pub delivers: AssetAmount,
    pub receives: AssetAmount,
    pub fee: AssetAmount,
}

/// Demo-side terms for a linear perpetual order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PerpetualTerms {
    pub leverage: u32,
    pub position: PositionSide,
    pub notional: Fixed,
    pub initial_margin: Fixed,
    pub fee: AssetAmount,
    /// Explicit maintenance buffer. The current demo uses zero, so the freeze
    /// is exactly `initial_margin + fee` after rounding.
    pub buffer: Fixed,
}

/// Freeze plus maker/taker settlement estimates for one order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrderEconomics {
    pub product: Product,
    pub freeze: FreezeEstimate,
    /// `None` for market orders, IOC/FOK limits and stop children, which never
    /// rest, and for perpetual orders, which have no spot delivery legs.
    pub maker: Option<SettlementLeg>,
    /// `None` for perpetual orders; see [`OrderEconomics::perpetual`].
    pub taker: Option<SettlementLeg>,
    pub fee_funding: FeeFunding,
    /// Present only for the linear perpetual overlay.
    pub perpetual: Option<PerpetualTerms>,
}

/// Estimated order-system freeze.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FreezeEstimate {
    pub asset: String,
    pub amount: Fixed,
    pub basis: FreezeBasis,
    pub role: FreezeRole,
    /// Effective non-negative fee rate selected for the estimate. Policies that
    /// take the fee from a settlement leg still report the rate even though it
    /// does not enlarge the frozen principal.
    pub fee_rate: Fixed,
    /// Price used for the notional and the base-fee reserve.
    pub reference_price: Fixed,
}

/// Aggregate freeze across orders, grouped by asset code.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AssetFreeze {
    pub asset: String,
    pub amount: Fixed,
    pub orders: usize,
}

struct Amounts {
    price: Fixed,
    base: Fixed,
    quote: Fixed,
}

/// Estimates the freeze and both settlement legs for a resting order or a
/// parked stop. A resting spot limit order has both a maker and a taker leg; a
/// market order, an IOC/FOK limit, or a stop child only has a taker leg.
pub fn estimate_order_economics(
    order: &Order,
    symbol: &SymbolConfig,
    product: Product,
    leverage: u32,
) -> Option<OrderEconomics> {
    estimate(order, symbol, None, false, product, leverage)
}

/// Estimates economics for a ticket that has not been submitted yet. A
/// restable limit uses the more expensive maker/taker rate, while a taker-only
/// order uses the taker rate; a market order may use the last trade price as its
/// reference.
pub fn estimate_ticket_economics(
    order: &Order,
    symbol: &SymbolConfig,
    fallback_price: Option<Fixed>,
    product: Product,
    leverage: u32,
) -> Option<OrderEconomics> {
    estimate(order, symbol, fallback_price, true, product, leverage)
}

/// Whether the demo can model a linear perpetual for this symbol. The engine's
/// cash/spot fee policy must name quote as the fee asset, because a linear
/// USDT-margined contract has no base delivery to withhold a base fee from.
pub fn perpetual_supported(symbol: &SymbolConfig) -> bool {
    symbol.fee_asset_policy == FeeAssetPolicy::Quote
}

/// Validates the demo's leverage bounds.
pub fn validate_leverage(leverage: u32) -> Result<(), String> {
    if !(MIN_PERPETUAL_LEVERAGE..=MAX_PERPETUAL_LEVERAGE).contains(&leverage) {
        return Err(format!(
            "leverage must be between {MIN_PERPETUAL_LEVERAGE} and {MAX_PERPETUAL_LEVERAGE}"
        ));
    }
    Ok(())
}

fn estimate(
    order: &Order,
    symbol: &SymbolConfig,
    fallback_price: Option<Fixed>,
    worst_case_freeze: bool,
    product: Product,
    leverage: u32,
) -> Option<OrderEconomics> {
    let amounts = order_amounts(order, symbol, fallback_price)?;
    let can_rest = can_rest(order);
    let (role, rate) = selected_rate(
        can_rest,
        order.maker_fee_rate,
        order.taker_fee_rate,
        worst_case_freeze,
    );

    if product == Product::Perpetual {
        return perpetual_economics(order, symbol, &amounts, role, rate, leverage);
    }

    let maker = if can_rest {
        Some(settlement_leg(
            order,
            symbol,
            &amounts,
            FreezeRole::Maker,
            order.maker_fee_rate,
        )?)
    } else {
        None
    };
    let taker = settlement_leg(
        order,
        symbol,
        &amounts,
        FreezeRole::Taker,
        order.taker_fee_rate,
    )?;
    let freeze = freeze_estimate(order, symbol, &amounts, role, rate)?;
    Some(OrderEconomics {
        product: Product::Spot,
        freeze,
        maker,
        taker: Some(taker),
        fee_funding: fee_funding(symbol.fee_asset_policy, order.side),
        perpetual: None,
    })
}

fn selected_rate(
    can_rest: bool,
    maker_rate: Fixed,
    taker_rate: Fixed,
    worst_case_freeze: bool,
) -> (FreezeRole, Fixed) {
    if !can_rest {
        (FreezeRole::Taker, taker_rate.max(Fixed::ZERO))
    } else if worst_case_freeze {
        let maker_rate = maker_rate.max(Fixed::ZERO);
        let taker_rate = taker_rate.max(Fixed::ZERO);
        if taker_rate >= maker_rate {
            (FreezeRole::Taker, taker_rate)
        } else {
            (FreezeRole::Maker, maker_rate)
        }
    } else {
        (FreezeRole::Maker, maker_rate.max(Fixed::ZERO))
    }
}

fn perpetual_economics(
    order: &Order,
    symbol: &SymbolConfig,
    amounts: &Amounts,
    role: FreezeRole,
    fee_rate: Fixed,
    leverage: u32,
) -> Option<OrderEconomics> {
    if !perpetual_supported(symbol) || validate_leverage(leverage).is_err() {
        return None;
    }

    // Worst-case notional: a quote-volume market sell can derive a larger base
    // quantity than the requested quote budget, so collateralise the larger of
    // the budget and `base * reference_price`.
    let notional = amounts.quote.max(amounts.base.checked_mul(amounts.price)?);
    let leverage_value = Fixed::from_units(i128::from(leverage));
    let initial_margin = notional.checked_div(leverage_value)?;
    let fee_amount = notional.checked_mul(fee_rate)?;
    // No extra maintenance buffer is modelled: the freeze is the initial
    // margin plus the estimated fee. Risk layers can add their own buffer.
    let buffer = Fixed::ZERO;
    let amount = ceil_quote(
        initial_margin
            .saturating_add(fee_amount)
            .saturating_add(buffer),
        symbol,
    );
    let position = match order.side {
        OrderSide::Buy => PositionSide::Long,
        OrderSide::Sell => PositionSide::Short,
    };
    Some(OrderEconomics {
        product: Product::Perpetual,
        freeze: FreezeEstimate {
            asset: symbol.quote_asset.code.clone(),
            amount,
            basis: FreezeBasis::PerpetualMarginPlusFee,
            role,
            fee_rate,
            reference_price: amounts.price,
        },
        maker: None,
        taker: None,
        fee_funding: FeeFunding::DeductFromMargin,
        perpetual: Some(PerpetualTerms {
            leverage,
            position,
            notional,
            initial_margin,
            fee: AssetAmount {
                asset: symbol.quote_asset.code.clone(),
                amount: fee_amount,
            },
            buffer,
        }),
    })
}

/// Whether the order can ever rest on the book and therefore earn the maker
/// rate. Stop children and IOC/FOK orders are taker-only by construction.
fn can_rest(order: &Order) -> bool {
    order.live == OrderType::Limit && !order.is_stop() && !order.tif.is_immediate()
}

fn fee_funding(policy: FeeAssetPolicy, side: OrderSide) -> FeeFunding {
    match (policy, side) {
        (FeeAssetPolicy::Received, OrderSide::Buy) => FeeFunding::WithholdFromBaseDelivery,
        (FeeAssetPolicy::Received, OrderSide::Sell) => FeeFunding::WithholdFromQuoteProceeds,
        (FeeAssetPolicy::Quote, OrderSide::Buy) => FeeFunding::DeductFromQuoteFreeze,
        (FeeAssetPolicy::Quote, OrderSide::Sell) => FeeFunding::WithholdFromQuoteProceeds,
        (FeeAssetPolicy::Base, OrderSide::Buy) => FeeFunding::BaseFromBaseDelivery,
        (FeeAssetPolicy::Base, OrderSide::Sell) => FeeFunding::BaseAgainstQuoteCollateral,
    }
}

fn order_amounts(
    order: &Order,
    symbol: &SymbolConfig,
    fallback_price: Option<Fixed>,
) -> Option<Amounts> {
    let price = if order.price > Fixed::ZERO {
        order.price
    } else if order.stop_price > Fixed::ZERO {
        order.stop_price
    } else if order.live == OrderType::Market {
        // Only a market ticket may fall back to the last trade; a limit order
        // without a price is invalid and has no honest estimate.
        fallback_price.filter(|price| *price > Fixed::ZERO)?
    } else {
        return None;
    };

    let (base, quote) = match order.volume_unit {
        VolumeUnit::Base => {
            let base = order.open_volume();
            let quote = price.checked_mul(base)?;
            (base, quote)
        }
        VolumeUnit::Quote => {
            // A quote limit order is normalized to base at its limit price, so
            // the notional that can actually trade is `base * price`. A quote
            // market order keeps its quote budget; buys floor the derived base
            // and sells ceil it. Both mirror the engine's `base_lot` rounding
            // (a zero lot leaves the derived base unchanged); the freeze
            // rounding below still applies the asset precision.
            let requested = order.volume;
            let raw_base = requested.checked_div(price)?;
            match order.live {
                OrderType::Limit => {
                    let base = raw_base.floor_to_multiple(symbol.base_lot);
                    let quote = price.checked_mul(base)?;
                    (base, quote)
                }
                OrderType::Market => {
                    let base = match order.side {
                        OrderSide::Buy => raw_base.floor_to_multiple(symbol.base_lot),
                        OrderSide::Sell => raw_base.ceil_to_multiple(symbol.base_lot),
                    };
                    (base, requested)
                }
            }
        }
    };

    if base <= Fixed::ZERO || quote <= Fixed::ZERO {
        return None;
    }
    Some(Amounts { price, base, quote })
}

fn settlement_leg(
    order: &Order,
    symbol: &SymbolConfig,
    amounts: &Amounts,
    role: FreezeRole,
    fee_rate: Fixed,
) -> Option<SettlementLeg> {
    let fee_asset = symbol.fee_asset_policy.fee_asset(order.side);
    let fee_amount = match fee_asset {
        FeeAsset::Base => amounts.base.checked_mul(fee_rate)?,
        FeeAsset::Quote => amounts.quote.checked_mul(fee_rate)?,
    };
    let fee = AssetAmount {
        asset: asset_code(symbol, fee_asset).to_string(),
        amount: fee_amount,
    };
    let (delivers, receives) = match order.side {
        OrderSide::Buy => (
            AssetAmount {
                asset: symbol.quote_asset.code.clone(),
                amount: amounts.quote,
            },
            AssetAmount {
                asset: symbol.base_asset.code.clone(),
                amount: amounts.base,
            },
        ),
        OrderSide::Sell => (
            AssetAmount {
                asset: symbol.base_asset.code.clone(),
                amount: amounts.base,
            },
            AssetAmount {
                asset: symbol.quote_asset.code.clone(),
                amount: amounts.quote,
            },
        ),
    };
    Some(SettlementLeg {
        role,
        fee_rate,
        delivers,
        receives,
        fee,
    })
}

fn freeze_estimate(
    order: &Order,
    symbol: &SymbolConfig,
    amounts: &Amounts,
    role: FreezeRole,
    fee_rate: Fixed,
) -> Option<FreezeEstimate> {
    let fee_asset = symbol.fee_asset_policy.fee_asset(order.side);
    let (asset, amount, basis) = match (symbol.fee_asset_policy, order.side) {
        (FeeAssetPolicy::Received, OrderSide::Buy) => (
            symbol.quote_asset.code.clone(),
            ceil_quote(amounts.quote, symbol),
            FreezeBasis::QuoteNotional,
        ),
        (FeeAssetPolicy::Received, OrderSide::Sell) => (
            symbol.base_asset.code.clone(),
            ceil_base(amounts.base, symbol),
            FreezeBasis::BasePrincipal,
        ),
        (FeeAssetPolicy::Quote, OrderSide::Buy) => {
            let fee = amounts.quote.checked_mul(fee_rate)?;
            (
                symbol.quote_asset.code.clone(),
                ceil_quote(amounts.quote.saturating_add(fee), symbol),
                FreezeBasis::QuoteNotionalPlusFee,
            )
        }
        (FeeAssetPolicy::Quote, OrderSide::Sell) => (
            symbol.base_asset.code.clone(),
            ceil_base(amounts.base, symbol),
            FreezeBasis::BasePrincipal,
        ),
        (FeeAssetPolicy::Base, _) => {
            if fee_asset != FeeAsset::Base {
                return None;
            }
            // A quote-volume market sell derives a ceiling base quantity and
            // receives a fixed quote budget, so the buy-back notional can
            // exceed that budget. Always collateralise the larger of the two.
            let notional = amounts.quote.max(amounts.base.checked_mul(amounts.price)?);
            let base_fee = amounts.base.checked_mul(fee_rate)?;
            let reserve = base_fee.checked_mul(amounts.price)?;
            (
                symbol.quote_asset.code.clone(),
                ceil_quote(notional.saturating_add(reserve), symbol),
                FreezeBasis::QuoteCollateralPlusFee,
            )
        }
    };
    Some(FreezeEstimate {
        asset,
        amount,
        basis,
        role,
        fee_rate,
        reference_price: amounts.price,
    })
}

fn asset_code(symbol: &SymbolConfig, asset: FeeAsset) -> &str {
    match asset {
        FeeAsset::Base => &symbol.base_asset.code,
        FeeAsset::Quote => &symbol.quote_asset.code,
    }
}

fn ceil_quote(value: Fixed, symbol: &SymbolConfig) -> Fixed {
    value.ceil_to_multiple(precision_step(symbol.quote_asset.decimals))
}

fn ceil_base(value: Fixed, symbol: &SymbolConfig) -> Fixed {
    value.ceil_to_multiple(base_step(symbol))
}

/// The base lot, never finer than the asset's settlement precision. A zero lot
/// disables lot validation but must not disable precision rounding.
fn base_step(symbol: &SymbolConfig) -> Fixed {
    symbol
        .base_lot
        .max(precision_step(symbol.base_asset.decimals))
}

fn precision_step(decimals: u8) -> Fixed {
    let decimals = u32::from(decimals.min(18));
    Fixed::from_raw(10i128.pow(18 - decimals))
}

/// Aggregates freeze estimates by asset code, preserving deterministic order.
pub fn summarize_freezes<'a>(
    estimates: impl IntoIterator<Item = &'a FreezeEstimate>,
) -> Vec<AssetFreeze> {
    let mut by_asset: BTreeMap<String, (Fixed, usize)> = BTreeMap::new();
    for estimate in estimates {
        let entry = by_asset
            .entry(estimate.asset.clone())
            .or_insert((Fixed::ZERO, 0));
        entry.0 = entry.0.saturating_add(estimate.amount);
        entry.1 += 1;
    }
    by_asset
        .into_iter()
        .map(|(asset, (amount, orders))| AssetFreeze {
            asset,
            amount,
            orders,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use matchengine_raft::order_book::{AssetClass, AssetId, FeeAssetPolicy, TimeInForce};

    fn symbol(policy: FeeAssetPolicy) -> SymbolConfig {
        SymbolConfig::default()
            .with_assets(
                AssetId::new("BTC", AssetClass::Crypto, 8).unwrap(),
                AssetId::new("USDT", AssetClass::Stablecoin, 2).unwrap(),
            )
            .unwrap()
            .with_fee_asset_policy(policy)
    }

    fn order(side: OrderSide, price: i128, volume: i128, maker: &str, taker: &str) -> Order {
        let mut order = Order::new(side, Fixed::from_units(price), Fixed::from_units(volume), 0);
        order.maker_fee_rate = maker.parse().unwrap();
        order.taker_fee_rate = taker.parse().unwrap();
        order
    }

    #[test]
    fn received_buy_freezes_quote_notional_and_charges_base_fee() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let order = order(OrderSide::Buy, 50_000, 1, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("USDT", economics.freeze.asset);
        assert_eq!("50000", economics.freeze.amount.to_string());
        assert_eq!(FreezeBasis::QuoteNotional, economics.freeze.basis);
        assert_eq!(
            "0.001",
            economics.maker.as_ref().unwrap().fee.amount.to_string()
        );
        assert_eq!("BTC", economics.maker.as_ref().unwrap().fee.asset);
        assert_eq!(
            "0.002",
            economics.taker.as_ref().unwrap().fee.amount.to_string()
        );
    }

    #[test]
    fn received_sell_freezes_base_principal_and_charges_quote_fee() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let order = order(OrderSide::Sell, 50_000, 1, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("BTC", economics.freeze.asset);
        assert_eq!("1", economics.freeze.amount.to_string());
        assert_eq!(FreezeBasis::BasePrincipal, economics.freeze.basis);
        assert_eq!(
            "50",
            economics.maker.as_ref().unwrap().fee.amount.to_string()
        );
        assert_eq!("USDT", economics.maker.as_ref().unwrap().fee.asset);
    }

    #[test]
    fn quote_buy_adds_the_quote_fee_to_the_freeze() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Buy, 100, 10, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("USDT", economics.freeze.asset);
        assert_eq!("1001", economics.freeze.amount.to_string());
        assert_eq!(FreezeBasis::QuoteNotionalPlusFee, economics.freeze.basis);
        assert_eq!(
            "1",
            economics.maker.as_ref().unwrap().fee.amount.to_string()
        );
    }

    #[test]
    fn base_policy_freezes_quote_collateral_and_charges_base_fee() {
        let symbol = symbol(FeeAssetPolicy::Base);
        let order = order(OrderSide::Buy, 100, 10, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("USDT", economics.freeze.asset);
        assert_eq!("1001", economics.freeze.amount.to_string());
        assert_eq!(FreezeBasis::QuoteCollateralPlusFee, economics.freeze.basis);
        assert_eq!(
            "0.01",
            economics.maker.as_ref().unwrap().fee.amount.to_string()
        );
        assert_eq!("BTC", economics.maker.as_ref().unwrap().fee.asset);
    }

    #[test]
    fn quote_sell_freezes_base_and_takes_the_fee_from_proceeds() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Sell, 100, 10, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("BTC", economics.freeze.asset);
        assert_eq!("10", economics.freeze.amount.to_string());
        assert_eq!("USDT", economics.maker.as_ref().unwrap().fee.asset);
    }

    #[test]
    fn iceberg_freeze_includes_the_hidden_reserve() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Sell, 100, 10, "0", "0");
        order.hidden_volume = Fixed::from_units(5);
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("15", economics.freeze.amount.to_string());
    }

    #[test]
    fn stop_children_use_the_taker_rate_without_a_maker_leg() {
        let symbol = symbol(FeeAssetPolicy::Base);
        let mut order = order(OrderSide::Buy, 0, 10, "0", "0.002");
        order.live = OrderType::Market;
        order.stop_price = Fixed::from_units(100);
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert!(economics.maker.is_none());
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("1002", economics.freeze.amount.to_string());
    }

    #[test]
    fn immediate_limit_orders_are_taker_only() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Buy, 100, 10, "0.01", "0.002");
        order.tif = TimeInForce::Ioc;
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert!(economics.maker.is_none());
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("0.002", economics.freeze.fee_rate.to_string());
    }

    #[test]
    fn quote_volume_sell_estimates_the_base_principal() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Sell, 100, 1, "0", "0");
        order.volume_unit = VolumeUnit::Quote;
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("BTC", economics.freeze.asset);
        assert_eq!("0.01", economics.freeze.amount.to_string());
    }

    #[test]
    fn freeze_rounds_quote_up_to_the_asset_precision() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Buy, 1, 1, "0", "0");
        order.price = "100.005".parse().unwrap();
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("100.01", economics.freeze.amount.to_string());
    }

    #[test]
    fn base_policy_quote_market_sell_collateralises_the_ceiling_base() {
        let symbol = symbol(FeeAssetPolicy::Base);
        let mut order = order(OrderSide::Sell, 0, 1, "0", "0.002");
        order.live = OrderType::Market;
        order.volume_unit = VolumeUnit::Quote;
        order.stop_price = Fixed::from_units(100);
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("USDT", economics.freeze.asset);
        assert_eq!("1.01", economics.freeze.amount.to_string());
        assert_eq!(
            "0.01",
            economics
                .taker
                .as_ref()
                .unwrap()
                .delivers
                .amount
                .to_string()
        );
    }

    #[test]
    fn quote_limit_sell_with_zero_lot_rounds_the_freeze_up() {
        let mut symbol = symbol(FeeAssetPolicy::Received);
        symbol.base_asset = AssetId::new("BTC", AssetClass::Crypto, 2).unwrap();
        symbol.base_lot = Fixed::ZERO;
        let mut order = order(OrderSide::Sell, 3, 1, "0", "0");
        order.volume_unit = VolumeUnit::Quote;
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("BTC", economics.freeze.asset);
        assert_eq!("0.34", economics.freeze.amount.to_string());
    }

    #[test]
    fn a_zero_lot_still_rounds_to_the_asset_precision() {
        let mut symbol = symbol(FeeAssetPolicy::Received);
        symbol.base_lot = Fixed::ZERO;
        let mut order = order(OrderSide::Sell, 100, 1, "0", "0");
        order.volume = "1.000000001".parse().unwrap();
        let economics = estimate_order_economics(&order, &symbol, Product::Spot, 1).unwrap();
        assert_eq!("1.00000001", economics.freeze.amount.to_string());
    }

    #[test]
    fn ticket_preview_uses_the_worst_case_fee_rate() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let order = order(OrderSide::Buy, 100, 1, "0", "0.01");
        let economics = estimate_ticket_economics(&order, &symbol, None, Product::Spot, 1).unwrap();
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("0.01", economics.freeze.fee_rate.to_string());
        assert_eq!("100", economics.freeze.amount.to_string());
    }

    #[test]
    fn a_market_order_without_any_reference_price_has_no_estimate() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Buy, 0, 1, "0", "0");
        order.live = OrderType::Market;
        assert!(estimate_order_economics(&order, &symbol, Product::Spot, 1).is_none());
    }

    #[test]
    fn a_limit_ticket_does_not_borrow_the_last_trade_price() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let mut order = order(OrderSide::Buy, 0, 1, "0", "0");
        order.live = OrderType::Limit;
        assert!(
            estimate_ticket_economics(
                &order,
                &symbol,
                Some(Fixed::from_units(100)),
                Product::Spot,
                1,
            )
            .is_none()
        );
    }

    #[test]
    fn linear_perpetual_freezes_margin_plus_quote_fee() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Buy, 50_000, 1, "0.001", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 10).unwrap();
        assert_eq!(Product::Perpetual, economics.product);
        assert_eq!("USDT", economics.freeze.asset);
        assert_eq!("5050", economics.freeze.amount.to_string());
        assert_eq!(FreezeBasis::PerpetualMarginPlusFee, economics.freeze.basis);
        assert_eq!(FeeFunding::DeductFromMargin, economics.fee_funding);
        assert!(economics.maker.is_none());
        assert!(economics.taker.is_none());
        let perpetual = economics.perpetual.unwrap();
        assert_eq!(10, perpetual.leverage);
        assert_eq!(PositionSide::Long, perpetual.position);
        assert_eq!("50000", perpetual.notional.to_string());
        assert_eq!("5000", perpetual.initial_margin.to_string());
        assert_eq!("50", perpetual.fee.amount.to_string());
        assert_eq!("USDT", perpetual.fee.asset);
        assert_eq!("0", perpetual.buffer.to_string());
    }

    #[test]
    fn perpetual_sell_opens_a_short_and_rounds_the_freeze_up() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Sell, 100, 1, "0", "0.002");
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 3).unwrap();
        assert_eq!(PositionSide::Short, economics.perpetual.unwrap().position);
        // A resting order uses the maker rate (zero here): 100 / 3 = 33.333...
        // and the quote lot is 0.01, so the initial margin rounds up to 33.34.
        assert_eq!("33.34", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_requires_the_quote_fee_policy() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let order = order(OrderSide::Buy, 100, 1, "0", "0");
        assert!(estimate_order_economics(&order, &symbol, Product::Perpetual, 10).is_none());
        assert!(!perpetual_supported(&symbol));
    }

    #[test]
    fn perpetual_leverage_is_bounded() {
        assert!(validate_leverage(1).is_ok());
        assert!(validate_leverage(100).is_ok());
        assert!(validate_leverage(0).is_err());
        assert!(validate_leverage(101).is_err());
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Buy, 100, 1, "0", "0");
        assert!(estimate_order_economics(&order, &symbol, Product::Perpetual, 0).is_none());
    }

    #[test]
    fn perpetual_ticket_uses_the_worst_case_rate_and_last_trade() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 0, 1, "0.001", "0.002");
        order.live = OrderType::Market;
        let economics = estimate_ticket_economics(
            &order,
            &symbol,
            Some(Fixed::from_units(100)),
            Product::Perpetual,
            10,
        )
        .unwrap();
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("0.002", economics.freeze.fee_rate.to_string());
        assert_eq!("10.2", economics.freeze.amount.to_string());
        assert_eq!("0.2", economics.perpetual.unwrap().fee.amount.to_string());
    }

    #[test]
    fn perpetual_quote_market_sell_collateralises_the_derived_base() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Sell, 0, 1, "0", "0.01");
        order.live = OrderType::Market;
        order.volume_unit = VolumeUnit::Quote;
        order.stop_price = Fixed::from_units(100);
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 10).unwrap();
        // 1 USDT requested derives a 0.01 BTC ceiling at 100; margin 0.1 +
        // fee 0.01 = 0.11 USDT.
        assert_eq!("0.11", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_supported_only_for_the_quote_fee_policy() {
        assert!(perpetual_supported(&symbol(FeeAssetPolicy::Quote)));
        assert!(!perpetual_supported(&symbol(FeeAssetPolicy::Received)));
        assert!(!perpetual_supported(&symbol(FeeAssetPolicy::Base)));
        // The `Base` policy is rejected for the same reason as `Received`: a
        // linear quote-margined contract has no base delivery to fund a fee
        // from, so the estimate must refuse rather than guess.
        let order = order(OrderSide::Buy, 100, 1, "0", "0");
        assert!(
            estimate_order_economics(
                &order,
                &symbol(FeeAssetPolicy::Base),
                Product::Perpetual,
                10
            )
            .is_none()
        );
    }

    #[test]
    fn perpetual_leverage_bounds_are_reachable_in_the_estimate() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Buy, 100, 1, "0.001", "0.001");
        let lev1 = estimate_order_economics(&order, &symbol, Product::Perpetual, 1).unwrap();
        assert_eq!(
            "100",
            lev1.perpetual.as_ref().unwrap().initial_margin.to_string()
        );
        assert_eq!("100.1", lev1.freeze.amount.to_string());
        let lev100 = estimate_order_economics(&order, &symbol, Product::Perpetual, 100).unwrap();
        assert_eq!(
            "1",
            lev100
                .perpetual
                .as_ref()
                .unwrap()
                .initial_margin
                .to_string()
        );
        assert_eq!("1.1", lev100.freeze.amount.to_string());
        assert!(estimate_order_economics(&order, &symbol, Product::Perpetual, 101).is_none());
        assert!(estimate_order_economics(&order, &symbol, Product::Perpetual, u32::MAX).is_none());
    }

    #[test]
    fn perpetual_rebate_rates_never_reduce_the_freeze_below_the_margin() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        // A resting order selects its maker rate, clamped at zero: the freeze
        // is exactly the initial margin, never less.
        let resting = order(OrderSide::Buy, 100, 1, "-0.5", "-0.2");
        let economics =
            estimate_order_economics(&resting, &symbol, Product::Perpetual, 10).unwrap();
        assert_eq!("0", economics.freeze.fee_rate.to_string());
        assert_eq!("10", economics.freeze.amount.to_string());
        assert_eq!(
            "0",
            economics.perpetual.as_ref().unwrap().fee.amount.to_string()
        );
        // A taker-only ticket clamps the negative taker rate the same way.
        let mut taker_only = order(OrderSide::Buy, 0, 1, "-0.2", "-0.5");
        taker_only.live = OrderType::Market;
        taker_only.stop_price = Fixed::from_units(100);
        let economics =
            estimate_order_economics(&taker_only, &symbol, Product::Perpetual, 10).unwrap();
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("0", economics.freeze.fee_rate.to_string());
        assert_eq!("10", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_freeze_rounds_margin_plus_fee_up_to_quote_precision() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let order = order(OrderSide::Sell, 100, 1, "0.001", "0.001");
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 3).unwrap();
        // margin 100/3 = 33.333… + fee 0.1 = 33.4333… → ceil to 0.01 → 33.44.
        assert_eq!("33.44", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_quote_limit_uses_the_normalized_base_notional() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 100, 1, "0", "0.01");
        order.volume_unit = VolumeUnit::Quote;
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 10).unwrap();
        // 1 USDT at 100 normalizes to 0.01 BTC, so the tradable notional is
        // 0.01 × 100 = 1 USDT; margin 0.1 with a zero maker rate.
        let perpetual = economics.perpetual.unwrap();
        assert_eq!("1", perpetual.notional.to_string());
        assert_eq!("0.1", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_stop_market_uses_the_stop_price_as_reference() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Sell, 0, 1, "0", "0.01");
        order.live = OrderType::Market;
        order.stop_price = Fixed::from_units(100);
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 5).unwrap();
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("100", economics.freeze.reference_price.to_string());
        // notional 100, margin 20, taker fee 1 → 21 USDT.
        assert_eq!("21", economics.freeze.amount.to_string());
        assert_eq!(PositionSide::Short, economics.perpetual.unwrap().position);
    }

    #[test]
    fn perpetual_ioc_limit_is_taker_only() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 100, 1, "0.001", "0.002");
        order.tif = TimeInForce::Ioc;
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 10).unwrap();
        assert_eq!(FreezeRole::Taker, economics.freeze.role);
        assert_eq!("0.002", economics.freeze.fee_rate.to_string());
        // margin 10 + taker fee 0.2.
        assert_eq!("10.2", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_without_a_reference_price_has_no_estimate() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut market = order(OrderSide::Buy, 0, 1, "0", "0");
        market.live = OrderType::Market;
        assert!(
            estimate_ticket_economics(&market, &symbol, None, Product::Perpetual, 10).is_none()
        );
        // A limit ticket never borrows the last trade, on the perpetual
        // backend exactly like the spot one.
        let mut limit = order(OrderSide::Buy, 0, 1, "0", "0");
        limit.live = OrderType::Limit;
        assert!(
            estimate_ticket_economics(
                &limit,
                &symbol,
                Some(Fixed::from_units(100)),
                Product::Perpetual,
                10,
            )
            .is_none()
        );
    }

    #[test]
    fn perpetual_math_is_exact_and_rounds_only_at_the_end() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 0, 0, "0", "0");
        order.price = "50000.02".parse().unwrap();
        order.volume = "0.14".parse().unwrap();
        let economics = estimate_order_economics(&order, &symbol, Product::Perpetual, 7).unwrap();
        let perpetual = economics.perpetual.unwrap();
        // 50000.02 × 0.14 = 7000.0028 exactly; / 7 = 1000.0004 exactly; only
        // the freeze rounds, up to the quote asset's 0.01 precision.
        assert_eq!("7000.0028", perpetual.notional.to_string());
        assert_eq!("1000.0004", perpetual.initial_margin.to_string());
        assert_eq!("1000.01", economics.freeze.amount.to_string());
    }

    #[test]
    fn perpetual_overflow_returns_no_estimate_rather_than_panicking() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let mut order = order(OrderSide::Buy, 0, 0, "0", "0");
        order.price = "100000000000".parse().unwrap();
        order.volume = "100000000000".parse().unwrap();
        // price × volume overflows the scale-18 integer; the estimate must be
        // `None`, never a panic or a wrapped amount.
        assert!(estimate_order_economics(&order, &symbol, Product::Perpetual, 10).is_none());
    }

    #[test]
    fn freeze_summary_groups_by_asset() {
        let symbol = symbol(FeeAssetPolicy::Received);
        let buy = estimate_order_economics(
            &order(OrderSide::Buy, 100, 2, "0", "0"),
            &symbol,
            Product::Spot,
            1,
        )
        .unwrap()
        .freeze;
        let sell = estimate_order_economics(
            &order(OrderSide::Sell, 100, 1, "0", "0"),
            &symbol,
            Product::Spot,
            1,
        )
        .unwrap()
        .freeze;
        let summary = summarize_freezes([&buy, &sell]);
        assert_eq!(2, summary.len());
        assert_eq!("BTC", summary[0].asset);
        assert_eq!("1", summary[0].amount.to_string());
        assert_eq!("USDT", summary[1].asset);
        assert_eq!("200", summary[1].amount.to_string());
    }

    #[test]
    fn freeze_summary_aggregates_perpetual_margin_in_the_quote_asset() {
        let symbol = symbol(FeeAssetPolicy::Quote);
        let long = estimate_order_economics(
            &order(OrderSide::Buy, 100, 1, "0", "0"),
            &symbol,
            Product::Perpetual,
            10,
        )
        .unwrap()
        .freeze;
        let short = estimate_order_economics(
            &order(OrderSide::Sell, 100, 2, "0", "0"),
            &symbol,
            Product::Perpetual,
            5,
        )
        .unwrap()
        .freeze;
        // Both perpetual positions margin in the quote asset: 100/10 = 10
        // long plus 200/5 = 40 short.
        let summary = summarize_freezes([&long, &short]);
        assert_eq!(1, summary.len());
        assert_eq!("USDT", summary[0].asset);
        assert_eq!("50", summary[0].amount.to_string());
        assert_eq!(2, summary[0].orders);
    }
}
