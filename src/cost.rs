//! What a trade costs: fees, spread, slippage and carry.

use serde::{Deserialize, Serialize};

use crate::account::{FeeTier, Liquidity};
use crate::market::OrderBook;
use crate::num::{Decimal, Qty};
use crate::order::Side;
use crate::{ExchangeError, Result};

/// The cost of a trade against the mid price, in the quote asset. Positive is a cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Cost {
    pub fee: Decimal,
    /// Half the spread, times the size (negative for a maker earning it).
    pub half_spread: Decimal,
    /// Walking past the best level, times the size.
    pub slippage: Decimal,
    /// Funding or borrow per day held.
    pub carry_per_day: Decimal,
}

impl Cost {
    /// The round cost of holding for `days`.
    pub fn total(&self, days: Decimal) -> Decimal {
        self.fee + self.half_spread + self.slippage + self.carry_per_day * days
    }
}

/// Estimates what a trade would cost.
pub trait CostModel: Send + Sync {
    fn estimate(&self, book: &OrderBook, side: Side, size: Qty, liquidity: Liquidity) -> Result<Cost>;
}

/// Costs from a live book and a fee tier. A taker walks the book; a maker fills at the best
/// level on its own side. `carry_rate_per_day` is a fraction of notional per day.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct BookCostModel {
    pub fees: FeeTier,
    pub carry_rate_per_day: Decimal,
}

impl CostModel for BookCostModel {
    fn estimate(&self, book: &OrderBook, side: Side, size: Qty, liquidity: Liquidity) -> Result<Cost> {
        let (best_bid, best_ask) = match (book.best_bid(), book.best_ask()) {
            (Some(b), Some(a)) => (b.price.0, a.price.0),
            _ => return Err(ExchangeError::invalid("the book has no two-sided quote")),
        };
        let half = (best_ask - best_bid) / Decimal::TWO;
        let (notional, half_spread, slippage) = match liquidity {
            Liquidity::Maker => {
                let touch = if side == Side::Buy { best_bid } else { best_ask };
                (touch * size.0, -half * size.0, Decimal::ZERO)
            }
            Liquidity::Taker => {
                let (filled, notional) = book.walk(side, size, None);
                if filled < size {
                    return Err(ExchangeError::invalid(format!("the book holds {filled} of {size}")));
                }
                let touch = if side == Side::Buy { best_ask } else { best_bid };
                let beyond = (notional - touch * size.0) * side.sign();
                (notional, half * size.0, beyond)
            }
        };
        Ok(Cost {
            fee: self.fees.fee(liquidity, notional),
            half_spread: half_spread.normalize(),
            slippage: slippage.normalize(),
            carry_per_day: (notional * self.carry_rate_per_day).normalize(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::Market;
    use crate::market::{BookSnapshot, Level};
    use crate::num::Price;

    #[test]
    fn taker_and_maker_costs() {
        let l = |p: &str, q: &str| Level::new(Price::parse(p).unwrap(), Qty::parse(q).unwrap());
        let book = OrderBook::from_snapshot(&BookSnapshot {
            market: Market::new("v", "X"),
            bids: vec![l("99", "1")],
            asks: vec![l("101", "1"), l("103", "1")],
            sequence: 1,
            at_ms: 0,
        });
        let m = BookCostModel { fees: FeeTier::from_bps(0, 10), carry_rate_per_day: Decimal::new(1, 4) };
        let c = m.estimate(&book, Side::Buy, Qty::parse("2").unwrap(), Liquidity::Taker).unwrap();
        // 204 notional: 0.204 fee, 1 × 2 half spread, 2 beyond the touch.
        assert_eq!((c.fee, c.half_spread, c.slippage, c.carry_per_day), (Decimal::new(204, 3), Decimal::TWO, Decimal::TWO, Decimal::new(204, 4)));
        assert_eq!(c.total(Decimal::ZERO), Decimal::new(4204, 3));
        let c = m.estimate(&book, Side::Sell, Qty::parse("1").unwrap(), Liquidity::Maker).unwrap();
        assert_eq!((c.fee, c.half_spread, c.slippage), (Decimal::ZERO, Decimal::NEGATIVE_ONE, Decimal::ZERO));
        assert!(m.estimate(&book, Side::Buy, Qty::parse("3").unwrap(), Liquidity::Taker).is_err());
    }
}
