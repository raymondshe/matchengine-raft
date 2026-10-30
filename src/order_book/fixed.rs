//! Fixed-point decimal values for order prices and quantities.
//!
//! Values are stored as `i128` units of `1e-18`. This keeps the matching hot
//! path in integer arithmetic, is `Copy`, allocation-free and deterministic,
//! and covers 18-decimal BTC values (about 20 integer digits of range).
//!
//! JSON emits decimal strings so all 18 digits round-trip exactly; legacy
//! numeric input is still accepted. Postcard (snapshots and Raft entries)
//! serializes the exact scaled integer.

use std::fmt;
use std::ops::Add;
use std::ops::Div;
use std::ops::Mul;
use std::ops::Neg;
use std::ops::Sub;
use std::str::FromStr;

use serde::Deserialize;
use serde::Deserializer;
use serde::Serialize;
use serde::Serializer;
use serde::de::Visitor;

/// Number of fractional decimal digits.
pub const DECIMALS: u32 = 18;
/// One unit in the fixed-point representation (`10^18`).
pub const SCALE: i128 = 1_000_000_000_000_000_000;

/// A signed fixed-point decimal with 18 fractional digits.
#[derive(Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fixed(i128);

/// Lowest accepted fee rate: a 100% rebate.
pub const MIN_FEE_RATE: Fixed = Fixed::from_units(-1);
/// Highest accepted fee rate: a 100% charge.
pub const MAX_FEE_RATE: Fixed = Fixed::from_units(1);

impl Fixed {
    /// Zero.
    pub const ZERO: Self = Self(0);

    /// Creates a value from whole units.
    pub const fn from_units(units: i128) -> Self {
        Self(units * SCALE)
    }

    /// Returns the scaled integer representation.
    pub const fn raw(self) -> i128 {
        self.0
    }

    /// Rebuilds a value from its scaled integer representation.
    ///
    /// This is the inverse of [`Fixed::raw`] and the constructor used by
    /// wire decoders.
    pub const fn from_raw(raw: i128) -> Self {
        Self(raw)
    }

    /// Converts to `f64` for legacy callers and tests. Public financial outputs do not use it.
    pub fn to_f64(self) -> f64 {
        self.0 as f64 / SCALE as f64
    }

    /// Returns the absolute value.
    pub fn abs(self) -> Self {
        Self(self.0.abs())
    }

    /// Returns the larger of `self` and `other`.
    pub fn max(self, other: impl Into<Self>) -> Self {
        std::cmp::max(self, other.into())
    }

    /// Returns the smaller of `self` and `other`.
    pub fn min(self, other: impl Into<Self>) -> Self {
        std::cmp::min(self, other.into())
    }

    /// Subtracts two values, returning `None` on integer overflow.
    pub fn checked_sub(self, rhs: Self) -> Option<Self> {
        self.0.checked_sub(rhs.0).map(Self)
    }

    /// Subtracts two values, saturating at the `i128` bounds.
    pub fn saturating_sub(self, rhs: Self) -> Self {
        Self(self.0.saturating_sub(rhs.0))
    }

    /// Multiplies two fixed-point values through a 256-bit intermediate.
    ///
    /// The product is truncated toward zero to the common 18-digit scale.
    /// Returns `None` when the unscaled product or the scaled result does not
    /// fit `i128`; this is the correct operation for price × quantity, whose
    /// unscaled intermediate can exceed `i128` even when the result fits.
    pub fn checked_mul(self, rhs: Self) -> Option<Self> {
        // Fast path: most fills fit the unscaled product in i128.
        if let Some(product) = self.0.checked_mul(rhs.0) {
            return Some(Self(product / SCALE));
        }

        let product = i256::I256::from_i128(self.0).checked_mul(i256::I256::from_i128(rhs.0))?;
        let scaled = product.checked_div(i256::I256::from_i128(SCALE))?;
        if scaled < i256::I256::from_i128(i128::MIN) || scaled > i256::I256::from_i128(i128::MAX) {
            return None;
        }
        Some(Self(scaled.as_i128()))
    }

    /// Divides two fixed-point values through a 256-bit numerator.
    ///
    /// The result is truncated toward zero to 18 fractional digits. Returns
    /// `None` on division by zero or when the result does not fit `i128`.
    pub fn checked_div(self, rhs: Self) -> Option<Self> {
        let numerator = i256::I256::from_i128(self.0).checked_mul(i256::I256::from_i128(SCALE))?;
        let quotient = numerator.checked_div(i256::I256::from_i128(rhs.0))?;
        if quotient < i256::I256::from_i128(i128::MIN)
            || quotient > i256::I256::from_i128(i128::MAX)
        {
            return None;
        }
        Some(Self(quotient.as_i128()))
    }

    /// Price × quantity, rounded toward zero to 18 fractional digits.
    ///
    /// This is an alias for [`Fixed::checked_mul`] with the notional use case
    /// spelled out; callers must decide how to handle an overflow.
    pub fn notional(price: Self, quantity: Self) -> Option<Self> {
        price.checked_mul(quantity)
    }

    /// Whether `self` is an exact multiple of `step`.
    ///
    /// A zero or negative `step` disables the check (legacy configurations).
    pub fn is_multiple_of(self, step: Self) -> bool {
        step.0 <= 0 || self.0 % step.0 == 0
    }

    /// Rounds down to the nearest multiple of `step`.
    ///
    /// A zero or negative `step` returns `self` unchanged.
    pub fn floor_to_multiple(self, step: Self) -> Self {
        if step.0 <= 0 {
            return self;
        }
        Self(self.0.div_euclid(step.0) * step.0)
    }

    /// Rounds up to the nearest multiple of `step`.
    ///
    /// A zero or negative `step` returns `self` unchanged.
    pub fn ceil_to_multiple(self, step: Self) -> Self {
        if step.0 <= 0 {
            return self;
        }
        let quotient = self.0.div_euclid(step.0) + i128::from(self.0.rem_euclid(step.0) != 0);
        Self(quotient * step.0)
    }

    /// The midpoint between two values, rounded toward zero to 18 places.
    ///
    /// Returns `None` if the integer addition overflows.
    pub fn midpoint(self, other: Self) -> Option<Self> {
        self.0.checked_add(other.0).map(|sum| Self(sum / 2))
    }

    /// Fixed-point values are always finite.
    pub const fn is_finite(self) -> bool {
        true
    }

    /// Whether the value fits a symbol's declared number of decimal places.
    pub fn has_at_most_decimals(self, decimals: u8) -> bool {
        if decimals >= DECIMALS as u8 {
            return true;
        }
        let step = 10i128.pow(DECIMALS - u32::from(decimals));
        self.0 % step == 0
    }

    /// Rounds half away from zero to `decimals` places.
    pub fn quantize(self, decimals: u8) -> Result<Self, ParseFixedError> {
        if decimals >= DECIMALS as u8 {
            return Ok(self);
        }
        let step = 10i128.pow(DECIMALS - u32::from(decimals));
        let magnitude = self.0.unsigned_abs();
        let rounded = ((magnitude + (step as u128) / 2) / step as u128) * step as u128;
        let rounded = rounded as i128;
        Ok(Self(if self.0 < 0 { -rounded } else { rounded }))
    }

    /// Parses a value and enforces a symbol's decimal precision.
    pub fn parse_with_decimals(value: &str, decimals: u8) -> Result<Self, ParseFixedError> {
        let value = value.parse::<Self>()?;
        if value.has_at_most_decimals(decimals) {
            Ok(value)
        } else {
            Err(ParseFixedError)
        }
    }
}

impl From<i64> for Fixed {
    fn from(value: i64) -> Self {
        Self(i128::from(value) * SCALE)
    }
}

impl From<u64> for Fixed {
    fn from(value: u64) -> Self {
        Self(i128::from(value) * SCALE)
    }
}

impl From<i32> for Fixed {
    fn from(value: i32) -> Self {
        Self(i128::from(value) * SCALE)
    }
}

impl From<u32> for Fixed {
    fn from(value: u32) -> Self {
        Self(i128::from(value) * SCALE)
    }
}

impl From<usize> for Fixed {
    fn from(value: usize) -> Self {
        Self((value as i128) * SCALE)
    }
}

impl From<f64> for Fixed {
    fn from(value: f64) -> Self {
        if !value.is_finite() {
            return Self::ZERO;
        }
        Self((value * SCALE as f64).round() as i128)
    }
}

impl From<Fixed> for f64 {
    fn from(value: Fixed) -> Self {
        value.to_f64()
    }
}

impl PartialEq<f64> for Fixed {
    fn eq(&self, other: &f64) -> bool {
        self.to_f64() == *other
    }
}

impl PartialOrd<f64> for Fixed {
    fn partial_cmp(&self, other: &f64) -> Option<std::cmp::Ordering> {
        self.to_f64().partial_cmp(other)
    }
}

impl PartialEq<Fixed> for f64 {
    fn eq(&self, other: &Fixed) -> bool {
        *self == other.to_f64()
    }
}

impl PartialOrd<Fixed> for f64 {
    fn partial_cmp(&self, other: &Fixed) -> Option<std::cmp::Ordering> {
        self.partial_cmp(&other.to_f64())
    }
}

impl Add for Fixed {
    type Output = Self;

    fn add(self, rhs: Self) -> Self::Output {
        Self(self.0 + rhs.0)
    }
}

impl std::ops::AddAssign for Fixed {
    fn add_assign(&mut self, rhs: Self) {
        self.0 += rhs.0;
    }
}

impl std::ops::SubAssign for Fixed {
    fn sub_assign(&mut self, rhs: Self) {
        self.0 -= rhs.0;
    }
}

impl Sub for Fixed {
    type Output = Self;

    fn sub(self, rhs: Self) -> Self::Output {
        Self(self.0 - rhs.0)
    }
}

impl Mul for Fixed {
    type Output = Self;

    fn mul(self, rhs: Self) -> Self::Output {
        self.checked_mul(rhs)
            .expect("fixed-point multiplication overflow")
    }
}

impl Div for Fixed {
    type Output = Self;

    fn div(self, rhs: Self) -> Self::Output {
        self.checked_div(rhs)
            .expect("fixed-point division overflow")
    }
}

impl Div<f64> for Fixed {
    type Output = Self;

    fn div(self, rhs: f64) -> Self::Output {
        self / Self::from(rhs)
    }
}

impl Neg for Fixed {
    type Output = Self;

    fn neg(self) -> Self::Output {
        Self(-self.0)
    }
}

impl Add<f64> for Fixed {
    type Output = Self;

    fn add(self, rhs: f64) -> Self::Output {
        self + Self::from(rhs)
    }
}

impl Sub<f64> for Fixed {
    type Output = Self;

    fn sub(self, rhs: f64) -> Self::Output {
        self - Self::from(rhs)
    }
}

impl Mul<f64> for Fixed {
    type Output = Self;

    fn mul(self, rhs: f64) -> Self::Output {
        self * Self::from(rhs)
    }
}

impl fmt::Display for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sign = if self.0 < 0 { "-" } else { "" };
        let abs = self.0.unsigned_abs();
        let integer = abs / SCALE as u128;
        let fraction = abs % SCALE as u128;
        if fraction == 0 {
            write!(f, "{sign}{integer}")
        } else {
            let mut fraction = format!("{fraction:018}");
            while fraction.ends_with('0') {
                fraction.pop();
            }
            write!(f, "{sign}{integer}.{fraction}")
        }
    }
}

impl fmt::Debug for Fixed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// Asset class carried by a symbol's base/quote metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AssetClass {
    /// National currency, e.g. USD or GBP.
    Fiat,
    /// Tokenised fiat, e.g. USDT or USDC.
    Stablecoin,
    /// Native crypto asset, e.g. BTC or ETH.
    Crypto,
    /// Listed equity, e.g. GOOGL.
    Equity,
    /// Exchange-traded fund, e.g. QQQ or VOO.
    Etf,
    /// Unknown/legacy asset metadata.
    #[default]
    Unknown,
}

impl AssetClass {
    /// Whether this base asset uses cash (quote-asset) fees by default.
    ///
    /// Brokerage-style ETF/equity fees are charged in the quote currency, not
    /// in shares. Crypto/spot pairs may use the received asset instead.
    pub fn is_equity_like(self) -> bool {
        matches!(self, Self::Equity | Self::Etf)
    }
}

/// Identity and settlement precision of one asset.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssetId {
    /// Human-readable asset code, e.g. `USD`, `USDT`, `QQQ` or `BTC`.
    pub code: String,
    /// Asset class; fiat and stablecoin are deliberately distinct.
    pub class: AssetClass,
    /// Settlement precision in fractional digits (0..=18).
    pub decimals: u8,
}

impl AssetId {
    /// Creates an asset id, rejecting unsupported settlement precision.
    pub fn new(
        code: impl Into<String>,
        class: AssetClass,
        decimals: u8,
    ) -> Result<Self, ParseFixedError> {
        if decimals > DECIMALS as u8 {
            return Err(ParseFixedError);
        }
        Ok(Self {
            code: code.into(),
            class,
            decimals,
        })
    }
}

/// Which side of an order's volume is specified by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum VolumeUnit {
    /// Gross base quantity.
    #[default]
    Base,
    /// Gross quote amount.
    Quote,
}

/// Where a fill fee is charged/rebated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeeAsset {
    /// The pair's base asset.
    Base,
    /// The pair's quote asset.
    Quote,
}

/// Pair-level fee deduction policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeeAssetPolicy {
    /// Buyer fees in base, seller fees in quote. Suitable for crypto/spot
    /// pairs; ETF/equity bases are forced to [`Self::Quote`].
    #[default]
    Received,
    /// Both sides in quote.
    Quote,
    /// Both sides in base.
    Base,
}

impl FeeAssetPolicy {
    /// Asset used for a fee on `side`.
    pub fn fee_asset(self, side: super::OrderSide) -> FeeAsset {
        match (self, side) {
            (Self::Received, super::OrderSide::Buy) => FeeAsset::Base,
            (Self::Received, super::OrderSide::Sell) => FeeAsset::Quote,
            (Self::Quote, _) => FeeAsset::Quote,
            (Self::Base, _) => FeeAsset::Base,
        }
    }
}

/// Per-symbol configuration: one base asset, one quote asset, precision,
/// tick/lot increments and the fee asset policy.
///
/// The internal `Fixed` scale is always 18 so the matching engine can compare
/// and add integers directly; this configuration validates and formats values
/// at the API boundary. A symbol may use fewer decimals (for example 2 for an
/// equity or 8 for native BTC), but never more than 18.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolConfig {
    /// Instrument id (the process/cluster trades one symbol).
    pub symbol_id: u32,
    /// Base asset and its settlement precision.
    pub base_asset: AssetId,
    /// Quote asset and its settlement precision.
    pub quote_asset: AssetId,
    /// Accepted fractional digits for prices (0..=18).
    pub price_decimals: u8,
    /// Minimum price increment; zero disables tick validation.
    pub price_tick: Fixed,
    /// Minimum base quantity increment; zero disables lot validation.
    pub base_lot: Fixed,
    /// Fee asset policy for this pair.
    pub fee_asset_policy: FeeAssetPolicy,
}

fn default_symbol_assets(decimals: u8) -> (AssetId, AssetId) {
    (
        AssetId {
            code: "BTC".to_string(),
            class: AssetClass::Crypto,
            decimals,
        },
        AssetId {
            code: "USDT".to_string(),
            class: AssetClass::Stablecoin,
            decimals,
        },
    )
}

fn decimal_step(decimals: u8) -> Fixed {
    Fixed(10i128.pow(DECIMALS - u32::from(decimals)))
}

impl Default for SymbolConfig {
    /// The single-symbol default: BTC/USDT, symbol `0`, 18 fractional digits.
    fn default() -> Self {
        let (base_asset, quote_asset) = default_symbol_assets(DECIMALS as u8);
        Self {
            symbol_id: 0,
            base_asset,
            quote_asset,
            price_decimals: DECIMALS as u8,
            price_tick: Fixed(1),
            base_lot: Fixed(1),
            fee_asset_policy: FeeAssetPolicy::Received,
        }
    }
}

impl SymbolConfig {
    /// Creates a configuration with default BTC/USDT asset metadata.
    pub fn new(
        symbol_id: u32,
        price_decimals: u8,
        quantity_decimals: u8,
    ) -> Result<Self, ParseFixedError> {
        if price_decimals > DECIMALS as u8 || quantity_decimals > DECIMALS as u8 {
            return Err(ParseFixedError);
        }
        let (base_asset, quote_asset) = default_symbol_assets(quantity_decimals);
        Ok(Self {
            symbol_id,
            base_asset,
            quote_asset,
            price_decimals,
            price_tick: decimal_step(price_decimals),
            base_lot: decimal_step(quantity_decimals),
            fee_asset_policy: FeeAssetPolicy::Received,
        })
    }

    /// Replaces base/quote asset metadata.
    pub fn with_assets(
        mut self,
        base_asset: AssetId,
        quote_asset: AssetId,
    ) -> Result<Self, ParseFixedError> {
        if base_asset.decimals > DECIMALS as u8 || quote_asset.decimals > DECIMALS as u8 {
            return Err(ParseFixedError);
        }
        if base_asset.class.is_equity_like() {
            // ETF/equity fees are cash fees: both sides settle in quote.
            self.fee_asset_policy = FeeAssetPolicy::Quote;
        }
        self.base_asset = base_asset;
        self.quote_asset = quote_asset;
        self.base_lot = decimal_step(self.base_asset.decimals);
        Ok(self)
    }

    /// Overrides tick/lot increments.
    pub fn with_tick_and_lot(mut self, price_tick: Fixed, base_lot: Fixed) -> Self {
        self.price_tick = price_tick;
        self.base_lot = base_lot;
        self
    }

    /// Overrides the fee asset policy.
    pub fn with_fee_asset_policy(mut self, policy: FeeAssetPolicy) -> Self {
        self.fee_asset_policy = policy;
        self
    }

    /// Base asset precision.
    pub fn base_decimals(&self) -> u8 {
        self.base_asset.decimals
    }

    /// Quote asset precision.
    pub fn quote_decimals(&self) -> u8 {
        self.quote_asset.decimals
    }

    /// Validates this configuration.
    pub fn validate(&self) -> Result<Self, ParseFixedError> {
        if self.price_decimals > DECIMALS as u8
            || self.base_asset.decimals > DECIMALS as u8
            || self.quote_asset.decimals > DECIMALS as u8
            || self.price_tick < Fixed::ZERO
            || self.base_lot < Fixed::ZERO
            || (self.base_asset.class.is_equity_like()
                && self.fee_asset_policy != FeeAssetPolicy::Quote)
        {
            return Err(ParseFixedError);
        }
        Ok(self.clone())
    }

    /// Parses a price at this symbol's precision.
    pub fn parse_price(&self, value: &str) -> Result<Fixed, ParseFixedError> {
        Fixed::parse_with_decimals(value, self.price_decimals)
    }

    /// Parses a base quantity at this symbol's precision.
    pub fn parse_quantity(&self, value: &str) -> Result<Fixed, ParseFixedError> {
        Fixed::parse_with_decimals(value, self.base_asset.decimals)
    }
}

/// Parses a decimal string exactly, rejecting more than 18 fractional digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseFixedError;

impl fmt::Display for ParseFixedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("invalid fixed-point decimal")
    }
}

impl std::error::Error for ParseFixedError {}

impl FromStr for Fixed {
    type Err = ParseFixedError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let value = value.trim();
        let (negative, value) = match value.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, value.strip_prefix('+').unwrap_or(value)),
        };
        let (integer, fraction) = match value.split_once('.') {
            Some((integer, fraction)) => (integer, fraction),
            None => (value, ""),
        };
        if integer.is_empty() && fraction.is_empty() {
            return Err(ParseFixedError);
        }
        if fraction.len() > DECIMALS as usize {
            return Err(ParseFixedError);
        }
        let integer: i128 = if integer.is_empty() {
            0
        } else {
            integer.parse().map_err(|_| ParseFixedError)?
        };
        let fraction_digits = if fraction.is_empty() {
            0
        } else {
            fraction.parse::<i128>().map_err(|_| ParseFixedError)?
        };
        let fraction = fraction_digits * 10i128.pow(DECIMALS - fraction.len() as u32);
        let value = integer
            .checked_mul(SCALE)
            .and_then(|value| value.checked_add(fraction))
            .ok_or(ParseFixedError)?;
        Ok(Self(if negative { -value } else { value }))
    }
}

impl Serialize for Fixed {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            // A decimal string preserves all 18 digits; JSON numbers do not.
            serializer.serialize_str(&self.to_string())
        } else {
            serializer.serialize_i128(self.0)
        }
    }
}

impl<'de> Deserialize<'de> for Fixed {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if !deserializer.is_human_readable() {
            return i128::deserialize(deserializer).map(Self);
        }

        struct FixedVisitor;

        impl Visitor<'_> for FixedVisitor {
            type Value = Fixed;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a decimal number or numeric string")
            }

            fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
                Ok(Fixed::from(value))
            }

            fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(Fixed::from(value))
            }

            fn visit_f64<E: serde::de::Error>(self, value: f64) -> Result<Self::Value, E> {
                if value.is_finite() {
                    Ok(Fixed::from(value))
                } else {
                    Err(E::custom("non-finite decimal"))
                }
            }

            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                Fixed::from_str(value).map_err(E::custom)
            }
        }

        deserializer.deserialize_any(FixedVisitor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats_exact_18_decimal_values() {
        let value: Fixed = "123456.123456789012345678".parse().unwrap();
        assert_eq!("123456.123456789012345678", value.to_string());
        assert_eq!(123_456_123_456_789_012_345_678, value.raw());
        assert_eq!(
            "-0.000000000000000001",
            "-0.000000000000000001"
                .parse::<Fixed>()
                .unwrap()
                .to_string()
        );
        assert!("1.0000000000000000001".parse::<Fixed>().is_err());
    }

    #[test]
    fn per_symbol_decimals_are_enforced() {
        let btc = SymbolConfig::new(0, 18, 18).unwrap();
        assert_eq!(
            "123456.123456789012345678",
            btc.parse_price("123456.123456789012345678")
                .unwrap()
                .to_string()
        );

        let equity = SymbolConfig::new(1, 2, 2).unwrap();
        assert!(equity.parse_price("100.25").is_ok());
        assert!(equity.parse_price("100.255").is_err());
        assert!(SymbolConfig::new(2, 19, 2).is_err());
    }

    #[test]
    fn equity_and_etf_force_quote_fees_while_fx_keeps_received() {
        let qqq = SymbolConfig::new(0, 2, 4)
            .unwrap()
            .with_assets(
                AssetId::new("QQQ", AssetClass::Etf, 4).unwrap(),
                AssetId::new("USD", AssetClass::Fiat, 2).unwrap(),
            )
            .unwrap();
        assert_eq!(FeeAssetPolicy::Quote, qqq.fee_asset_policy);
        assert!(qqq.validate().is_ok());

        let invalid_equity = qqq.clone().with_fee_asset_policy(FeeAssetPolicy::Received);
        assert!(
            invalid_equity.validate().is_err(),
            "ETF/equity fees must settle in quote"
        );

        let eurusd = SymbolConfig::new(1, 4, 4)
            .unwrap()
            .with_assets(
                AssetId::new("EUR", AssetClass::Fiat, 4).unwrap(),
                AssetId::new("USD", AssetClass::Fiat, 4).unwrap(),
            )
            .unwrap();
        assert_eq!(FeeAssetPolicy::Received, eurusd.fee_asset_policy);
        assert!(eurusd.validate().is_ok());

        let btcusdt = SymbolConfig::new(2, 8, 8)
            .unwrap()
            .with_assets(
                AssetId::new("BTC", AssetClass::Crypto, 8).unwrap(),
                AssetId::new("USDT", AssetClass::Stablecoin, 8).unwrap(),
            )
            .unwrap();
        assert_eq!(FeeAssetPolicy::Received, btcusdt.fee_asset_policy);
        assert!(btcusdt.validate().is_ok());
    }

    #[test]
    fn json_uses_decimal_strings() {
        let value: Fixed = "123456.123456789012345678".parse().unwrap();
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!("\"123456.123456789012345678\"", json);
        let back: Fixed = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
        // Legacy numeric payloads remain accepted.
        let legacy: Fixed = serde_json::from_str("100.25").unwrap();
        assert_eq!("100.25", legacy.to_string());
    }

    #[test]
    fn arithmetic_is_exact() {
        let a: Fixed = "0.1".parse().unwrap();
        let b: Fixed = "0.2".parse().unwrap();
        assert_eq!("0.3", (a + b).to_string());
        assert_eq!("0.1", (a * 1.0).to_string());
    }

    #[test]
    fn notional_uses_a_256_bit_intermediate() {
        // Both operands fit i128, but their unscaled product does not.
        let price = Fixed::from_units(1_000_000_000_000);
        let quantity = Fixed::from_units(100_000_000);
        let notional = Fixed::notional(price, quantity).expect("the scaled result fits");
        assert_eq!("100000000000000000000", notional.to_string());
        assert_eq!(
            Some(Fixed::from_units(100_000_000_000_000_000_000)),
            Some(notional)
        );

        let very_large = Fixed::from_units(i128::MAX / SCALE);
        assert_eq!(None, very_large.checked_mul(very_large));
    }

    #[test]
    fn midpoint_is_exact_at_the_scale() {
        let a: Fixed = "100.000000000000000001".parse().unwrap();
        let b: Fixed = "100.000000000000000002".parse().unwrap();
        assert_eq!(
            "100.000000000000000001",
            a.midpoint(b).expect("no overflow").to_string()
        );
        assert_eq!(
            None,
            Fixed::from_units(i128::MAX / SCALE).midpoint(Fixed::from_units(i128::MAX / SCALE))
        );
    }
}
