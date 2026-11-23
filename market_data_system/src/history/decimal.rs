//! Exact decimal handling for the TDengine bar path.
//!
//! TDengine stores prices and volumes in `DECIMAL(38,18)`. The database is
//! exact, but a 96-bit client decimal is not: the `taos` connector maps DECIMAL
//! to `rust_decimal`, which silently loses the low digits of a 38-digit value.
//! This module therefore does two things only:
//!
//! * format an engine `Fixed` as a canonical 18-fractional-digit SQL literal;
//! * parse a `CAST(column AS VARCHAR)` result back into `Fixed` strictly.
//!
//! There is deliberately no `f64` conversion anywhere in the historical path.

use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::fixed::SCALE;

/// Formats a fixed value as `[-]<integer>.<exactly 18 digits>`.
///
/// `Fixed::to_string()` trims trailing zeros, which is fine for JSON but not
/// for a canonical SQL literal or a wire string that promises 18 decimals.
pub fn to_sql(value: Fixed) -> String {
    let raw = value.raw();
    let sign = if raw < 0 { "-" } else { "" };
    let abs = raw.unsigned_abs();
    let whole = abs / SCALE as u128;
    let fraction = abs % SCALE as u128;
    format!("{sign}{whole}.{fraction:018}")
}

/// Alias used by the serving path: the wire carries the same canonical text.
pub fn to_wire(value: Fixed) -> String {
    to_sql(value)
}

/// Parses decimal text into an 18-scale `Fixed`, rejecting anything the
/// database could only round or overflow.
pub fn parse(text: &str) -> Option<Fixed> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (negative, digits) = match text.as_bytes()[0] {
        b'-' => (true, &text[1..]),
        b'+' => (false, &text[1..]),
        _ => (false, text),
    };
    if digits.is_empty() {
        return None;
    }
    let (whole, fraction) = match digits.split_once('.') {
        Some((whole, fraction)) => (whole, fraction),
        None => (digits, ""),
    };
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 18
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let whole: i128 = whole.parse().ok()?;
    let fraction: i128 = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<i128>().ok()? * 10i128.pow((18 - fraction.len()) as u32)
    };
    let raw = whole.checked_mul(SCALE)?.checked_add(fraction)?;
    // DECIMAL(38,18) has 20 integer digits; 10^20 is already out of range.
    const MAX_RAW: i128 = 100_000_000_000_000_000_000_000_000_000_000_000_000;
    if raw >= MAX_RAW {
        return None;
    }
    Some(Fixed::from_raw(if negative { -raw } else { raw }))
}

/// A canonical zero, used for flat/synthetic fields.
pub fn zero_wire() -> String {
    to_sql(Fixed::ZERO)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_text_round_trips() {
        let value = Fixed::from_raw(123_456_789_123_456_789_123);
        let text = to_sql(value);
        assert_eq!(text, "123.456789123456789123");
        assert_eq!(parse(&text), Some(value));
    }

    #[test]
    fn max_decimal_value_is_exact() {
        let text = "99999999999999999999.999999999999999999";
        let value = parse(text).expect("38-digit value");
        assert_eq!(to_sql(value), text);
    }

    #[test]
    fn rejects_rounding_and_overflow() {
        assert_eq!(parse("1.1234567890123456789"), None);
        assert_eq!(parse("100000000000000000000"), None);
        assert_eq!(parse("1e5"), None);
        assert_eq!(parse(""), None);
    }
}
