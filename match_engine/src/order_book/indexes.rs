//! Derived indexes over parked stops and expiring orders.
//!
//! Nothing in this module is snapshotted: [`super::OrderBook`] owns the
//! authoritative bids/asks/stops maps and rebuilds these indexes after a
//! restore with [`super::OrderBook::rebuild_derived_indexes`]. They exist so
//! that a trade print only visits trailing stops, a cancel/amend finds a
//! parked stop by sequence, a cascade finds the next elected stop without
//! walking every parked order, and an expiry purge only touches deadlines that
//! are actually due.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::Bound;
use std::ops::RangeBounds;

use super::Fixed;
use super::Order;
use super::OrderSide;
use super::TimeInForce;

/// Trigger-ordered index over parked stops plus the elected set.
///
/// Buy stops are keyed by trigger in ascending order and fire when the last
/// trade is at or above the trigger; sell stops fire at or below it. Both maps
/// are ascending because the elected set is maintained incrementally: a price
/// move only visits the trigger range it crossed.
#[derive(Clone, Debug, Default)]
pub(crate) struct StopIndex {
    /// Buy stops: trigger -> parked sequences.
    buys: BTreeMap<Fixed, BTreeSet<u64>>,
    /// Sell stops: trigger -> parked sequences.
    sells: BTreeMap<Fixed, BTreeSet<u64>>,
    /// Parked trailing stops by sequence, so a print does not walk fixed stops.
    trailing: BTreeSet<u64>,
    /// Sequences whose trigger is crossed by the current last trade price.
    /// `first()` is the next stop to elect in admission (sequence) order.
    elected: BTreeSet<u64>,
}

impl StopIndex {
    /// Clears every index. The caller rebuilds it from the authoritative maps.
    pub(crate) fn clear(&mut self) {
        self.buys.clear();
        self.sells.clear();
        self.trailing.clear();
        self.elected.clear();
    }

    /// Indexes one parked stop. Uninitialised trailing stops have no trigger
    /// yet and are not indexed; the first print inserts them.
    pub(crate) fn insert(&mut self, stop: &Order, last: Option<Fixed>) {
        if let Some(trigger) = indexed_trigger(stop) {
            self.levels_mut(stop.side)
                .entry(trigger)
                .or_default()
                .insert(stop.sequence);
        }
        if stop.is_trailing_stop() {
            self.trailing.insert(stop.sequence);
        }
        self.set_membership(stop, last);
    }

    /// Removes one parked stop from every index.
    pub(crate) fn remove(&mut self, stop: &Order) {
        if let Some(trigger) = indexed_trigger(stop) {
            self.remove_level(stop.side, trigger, stop.sequence);
        }
        self.trailing.remove(&stop.sequence);
        self.elected.remove(&stop.sequence);
    }

    /// Moves a stop to a new trigger after an amendment or a trailing update.
    ///
    /// `old_trigger` is the stop's previous indexed trigger (or `None` when it
    /// was uninitialised); `stop` carries the new trigger. Membership in the
    /// elected set is recomputed against `last`.
    pub(crate) fn retrigger(
        &mut self,
        stop: &Order,
        old_trigger: Option<Fixed>,
        last: Option<Fixed>,
    ) {
        if let Some(trigger) = old_trigger {
            self.remove_level(stop.side, trigger, stop.sequence);
        }
        if let Some(trigger) = indexed_trigger(stop) {
            self.levels_mut(stop.side)
                .entry(trigger)
                .or_default()
                .insert(stop.sequence);
        }
        if stop.is_trailing_stop() {
            self.trailing.insert(stop.sequence);
        } else {
            self.trailing.remove(&stop.sequence);
        }
        self.set_membership(stop, last);
    }

    /// Updates the elected set for a new last trade price.
    ///
    /// Buy stops become elected as the price rises through their trigger and
    /// unelected as it falls back; sell stops are the mirror image. Only the
    /// trigger range between the two prices is visited.
    pub(crate) fn on_last_trade(&mut self, old: Option<Fixed>, new: Fixed) {
        match old {
            None => {
                set_elected(&self.buys, &mut self.elected, ..=new, true);
                set_elected(&self.sells, &mut self.elected, new.., true);
            }
            Some(old) if old < new => {
                // A rising print elects buy stops it crosses and un-elects
                // sell stops whose trigger it leaves behind.
                set_elected(
                    &self.buys,
                    &mut self.elected,
                    (Bound::Excluded(old), Bound::Included(new)),
                    true,
                );
                set_elected(&self.sells, &mut self.elected, old..new, false);
            }
            Some(old) if new < old => {
                // A falling print elects sell stops it crosses and un-elects
                // buy stops whose trigger it leaves behind.
                set_elected(
                    &self.buys,
                    &mut self.elected,
                    (Bound::Excluded(new), Bound::Included(old)),
                    false,
                );
                set_elected(&self.sells, &mut self.elected, new..old, true);
            }
            Some(_) => {}
        }
    }

    /// Next stop to elect, in admission (sequence) order.
    pub(crate) fn first_elected(&self) -> Option<u64> {
        self.elected.first().copied()
    }

    /// Parked trailing stop sequences, ascending.
    pub(crate) fn trailing_sequences(&self) -> impl Iterator<Item = u64> + '_ {
        self.trailing.iter().copied()
    }

    /// Whether any trailing stop is parked.
    pub(crate) fn has_trailing(&self) -> bool {
        !self.trailing.is_empty()
    }

    /// Recomputes membership for one stop against `last`.
    fn set_membership(&mut self, stop: &Order, last: Option<Fixed>) {
        let elected = indexed_trigger(stop)
            .zip(last)
            .is_some_and(|(trigger, last)| elected_at(stop.side, trigger, last));
        if elected {
            self.elected.insert(stop.sequence);
        } else {
            self.elected.remove(&stop.sequence);
        }
    }

    fn levels_mut(&mut self, side: OrderSide) -> &mut BTreeMap<Fixed, BTreeSet<u64>> {
        match side {
            OrderSide::Buy => &mut self.buys,
            OrderSide::Sell => &mut self.sells,
        }
    }

    fn remove_level(&mut self, side: OrderSide, trigger: Fixed, sequence: u64) {
        let levels = self.levels_mut(side);
        if let Some(sequences) = levels.get_mut(&trigger) {
            sequences.remove(&sequence);
            if sequences.is_empty() {
                levels.remove(&trigger);
            }
        }
    }
    /// Recomputes every index from scratch. Test-only guard against a missed
    /// removal/update in one of the incremental paths.
    #[cfg(test)]
    pub(crate) fn assert_matches(&self, stops: &BTreeMap<u64, Order>, last: Option<Fixed>) {
        let mut buys: BTreeMap<Fixed, BTreeSet<u64>> = BTreeMap::new();
        let mut sells: BTreeMap<Fixed, BTreeSet<u64>> = BTreeMap::new();
        let mut trailing = BTreeSet::new();
        let mut elected = BTreeSet::new();
        for stop in stops.values() {
            if let Some(trigger) = indexed_trigger(stop) {
                let level = match stop.side {
                    OrderSide::Buy => &mut buys,
                    OrderSide::Sell => &mut sells,
                };
                level.entry(trigger).or_default().insert(stop.sequence);
                if last.is_some_and(|last| elected_at(stop.side, trigger, last)) {
                    elected.insert(stop.sequence);
                }
            }
            if stop.is_trailing_stop() {
                trailing.insert(stop.sequence);
            }
        }
        assert_eq!(self.buys, buys, "buy trigger index diverged");
        assert_eq!(self.sells, sells, "sell trigger index diverged");
        assert_eq!(self.trailing, trailing, "trailing index diverged");
        assert_eq!(self.elected, elected, "elected set diverged");
    }
}

/// Adds or removes every sequence in `range` from `elected`.
fn set_elected(
    levels: &BTreeMap<Fixed, BTreeSet<u64>>,
    elected: &mut BTreeSet<u64>,
    range: impl RangeBounds<Fixed>,
    value: bool,
) {
    for sequences in levels.range(range).map(|(_, sequences)| sequences) {
        for sequence in sequences {
            if value {
                elected.insert(*sequence);
            } else {
                elected.remove(sequence);
            }
        }
    }
}

/// The trigger a parked stop contributes to the index, if it can be elected.
///
/// An uninitialised trailing stop has no reference (and therefore no trigger)
/// until the first print; `stop_elected` refuses it too.
pub(crate) fn indexed_trigger(stop: &Order) -> Option<Fixed> {
    if stop.is_trailing_stop() {
        (stop.trail_reference > Fixed::ZERO).then_some(stop.stop_price)
    } else {
        (stop.stop_price > Fixed::ZERO).then_some(stop.stop_price)
    }
}

fn elected_at(side: OrderSide, trigger: Fixed, last: Fixed) -> bool {
    match side {
        OrderSide::Buy => last >= trigger,
        OrderSide::Sell => last <= trigger,
    }
}

/// Where an expiring order lives, so a purge can remove it without a scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ExpiryLocation {
    pub(crate) side: OrderSide,
    pub(crate) price: Fixed,
    /// `true` for a parked stop (keyed by sequence alone).
    pub(crate) parked: bool,
}

/// Deadlines of resting and parked GTD/Day orders.
///
/// GTD entries are keyed by `(expiry, sequence)` so the earliest deadline is
/// the first key and a purge can split off every due entry in one operation.
/// Day orders have no fixed deadline (they expire at a close supplied by the
/// request) and live in their own map.
#[derive(Clone, Debug, Default)]
pub(crate) struct ExpiryIndex {
    gtd: BTreeMap<(u64, u64), ExpiryLocation>,
    day: BTreeMap<u64, ExpiryLocation>,
}

impl ExpiryIndex {
    /// Indexes one order. `parked` distinguishes a stop from a book order.
    pub(crate) fn insert(&mut self, order: &Order, parked: bool) {
        let location = ExpiryLocation {
            side: order.side,
            price: order.price,
            parked,
        };
        match order.tif {
            TimeInForce::Gtd(expiry) => {
                self.gtd.insert((expiry, order.sequence), location);
            }
            TimeInForce::Day => {
                self.day.insert(order.sequence, location);
            }
            TimeInForce::Gtc | TimeInForce::Ioc | TimeInForce::Fok => {}
        }
    }

    /// Removes one order after a cancel, fill, amendment or election.
    pub(crate) fn remove(&mut self, order: &Order) {
        match order.tif {
            TimeInForce::Gtd(expiry) => {
                self.gtd.remove(&(expiry, order.sequence));
            }
            TimeInForce::Day => {
                self.day.remove(&order.sequence);
            }
            TimeInForce::Gtc | TimeInForce::Ioc | TimeInForce::Fok => {}
        }
    }

    /// Moves a re-inserted order (new sequence) to its new identity.
    pub(crate) fn reinsert(&mut self, old: &Order, new: &Order, parked: bool) {
        self.remove(old);
        self.insert(new, parked);
    }

    /// Clears every entry.
    pub(crate) fn clear(&mut self) {
        self.gtd.clear();
        self.day.clear();
    }

    /// Whether any GTD deadline is tracked.
    pub(crate) fn has_gtd(&self) -> bool {
        !self.gtd.is_empty()
    }

    /// Whether any GTD or Day order is tracked.
    pub(crate) fn has_any(&self) -> bool {
        !self.gtd.is_empty() || !self.day.is_empty()
    }

    /// Earliest tracked GTD deadline.
    pub(crate) fn earliest_gtd(&self) -> Option<u64> {
        self.gtd.first_key_value().map(|(&(expiry, _), _)| expiry)
    }

    /// Removes and returns every deadline due at `now_ms`.
    ///
    /// Day orders are due only when the request supplies a close at or before
    /// `now_ms`, mirroring [`TimeInForce::is_expired`].
    pub(crate) fn take_due(
        &mut self,
        now_ms: u64,
        day_close_ms: Option<u64>,
    ) -> Vec<(u64, ExpiryLocation)> {
        let mut due = Vec::new();
        if self.earliest_gtd().is_some_and(|expiry| now_ms >= expiry) {
            // `split_off` keeps everything strictly after `(now, MAX)`, which
            // is exactly "expiry > now"; the replaced map is the due prefix.
            let future = self.gtd.split_off(&(now_ms, u64::MAX));
            let due_gtd = std::mem::replace(&mut self.gtd, future);
            due.extend(
                due_gtd
                    .into_iter()
                    .map(|((_, sequence), location)| (sequence, location)),
            );
        }
        if day_close_ms.is_some_and(|close| now_ms >= close) {
            due.extend(std::mem::take(&mut self.day));
        }
        due
    }

    /// Recomputes both deadline maps from the authoritative book. Test-only.
    #[cfg(test)]
    pub(crate) fn assert_matches(
        &self,
        resting: impl Iterator<Item = Order>,
        stops: &BTreeMap<u64, Order>,
    ) {
        let mut gtd = BTreeMap::new();
        let mut day = BTreeMap::new();
        let mut index = |order: &Order, parked: bool| {
            let location = ExpiryLocation {
                side: order.side,
                price: order.price,
                parked,
            };
            match order.tif {
                TimeInForce::Gtd(expiry) => {
                    gtd.insert((expiry, order.sequence), location);
                }
                TimeInForce::Day => {
                    day.insert(order.sequence, location);
                }
                TimeInForce::Gtc | TimeInForce::Ioc | TimeInForce::Fok => {}
            }
        };
        for order in resting {
            index(&order, false);
        }
        for stop in stops.values() {
            index(stop, true);
        }
        assert_eq!(self.gtd, gtd, "GTD deadline index diverged");
        assert_eq!(self.day, day, "Day deadline index diverged");
    }
}
