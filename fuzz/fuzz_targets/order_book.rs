#![no_main]

use arbitrary::Arbitrary;
use arbitrary::Unstructured;
use libfuzzer_sys::fuzz_target;
use matchengine_raft::order_book::Fixed;
use matchengine_raft::order_book::Order;
use matchengine_raft::order_book::OrderBook;
use matchengine_raft::order_book::OrderSide;
use matchengine_raft::order_book::OrderType;
use matchengine_raft::order_book::TimeInForce;
use matchengine_raft::order_book::VolumeUnit;
use matchengine_raft::order_book::fixed::SCALE;

/// Structured order input. The field ranges are deliberately small enough that
/// a useful fraction of generated orders are admissible and reach the matching
/// loop instead of being rejected by shape validation.
#[derive(Arbitrary, Debug)]
enum FuzzTif {
    Gtc,
    Ioc,
    Fok,
    Gtd(u32),
    Day,
}

#[derive(Arbitrary, Debug)]
struct FuzzOrder {
    side: bool,
    live: bool,
    price: i8,
    volume: i8,
    volume_unit: bool,
    requested_volume: i8,
    maker_fee_bps: i16,
    taker_fee_bps: i16,
    id: u32,
    tif: FuzzTif,
    post_only: bool,
    stop_price: u8,
    display_volume: u8,
    hidden_volume: u8,
    timestamp: u32,
}

impl FuzzOrder {
    fn into_order(self) -> Order {
        let tif = match self.tif {
            FuzzTif::Gtc => TimeInForce::Gtc,
            FuzzTif::Ioc => TimeInForce::Ioc,
            FuzzTif::Fok => TimeInForce::Fok,
            FuzzTif::Gtd(expiry) => TimeInForce::Gtd(u64::from(expiry)),
            FuzzTif::Day => TimeInForce::Day,
        };
        Order {
            live: if self.live {
                OrderType::Market
            } else {
                OrderType::Limit
            },
            side: if self.side {
                OrderSide::Buy
            } else {
                OrderSide::Sell
            },
            symbol_id: 0,
            price: Fixed::from(i32::from(self.price)),
            volume: Fixed::from(i32::from(self.volume)),
            volume_unit: if self.volume_unit {
                VolumeUnit::Quote
            } else {
                VolumeUnit::Base
            },
            requested_volume: Fixed::from(i32::from(self.requested_volume)),
            requested_volume_unit: VolumeUnit::Base,
            maker_fee_rate: fee_rate(self.maker_fee_bps),
            taker_fee_rate: fee_rate(self.taker_fee_bps),
            id: u64::from(self.id),
            sequence: 0,
            tif,
            post_only: self.post_only,
            stop_price: Fixed::from(u32::from(self.stop_price)),
            display_volume: Fixed::from(u32::from(self.display_volume)),
            hidden_volume: Fixed::from(u32::from(self.hidden_volume)),
            timestamp: u64::from(self.timestamp),
        }
    }
}

fn fee_rate(bps: i16) -> Fixed {
    let bps = i128::from(bps.clamp(-10_000, 10_000));
    Fixed::from_raw(bps * (SCALE / 10_000))
}

fuzz_target!(|data: &[u8]| {
    let mut input = Unstructured::new(data);
    let mut book = OrderBook::new();

    // Bound the number of operations so one input cannot monopolise a worker.
    for expected_sequence in 0..64u64 {
        let Ok(fuzz) = FuzzOrder::arbitrary(&mut input) else {
            break;
        };
        let mut order = fuzz.into_order();
        let outcome = book.execute_order(&mut order);

        assert_eq!(outcome.market_seq, expected_sequence);

        // A resting book must never cross, including after a stop cascade.
        if let (Some(bid), Some(ask)) = (book.best_bid(), book.best_ask()) {
            assert!(
                bid.price < ask.price,
                "crossed book: bid={:?} ask={:?}",
                bid.price,
                ask.price
            );
        }
    }
});
