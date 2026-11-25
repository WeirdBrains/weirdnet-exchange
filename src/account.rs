//! Fills, balances, fees, positions and margin.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::instrument::{Asset, Market};
use crate::num::{Decimal, Price, Qty};
use crate::order::{ClientOrderId, OrderId, PositionSide, Side};

/// Whether a fill added liquidity (maker) or took it (taker).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Liquidity {
    Maker,
    Taker,
}

/// One execution of (part of) an order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fill {
    /// The venue's trade id; unique per venue and product.
    pub trade_id: String,
    pub order_id: OrderId,
    pub client_id: Option<ClientOrderId>,
    pub market: Market,
    pub side: Side,
    pub price: Price,
    pub size: Qty,
    pub fee: Decimal,
    pub fee_asset: Asset,
    pub liquidity: Liquidity,
    pub at_ms: i64,
}

impl Fill {
    /// price × size, in the quote asset.
    pub fn notional(&self) -> Decimal {
        self.price * self.size
    }
}

/// How much of an asset an account holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    pub asset: Asset,
    /// Free to use.
    pub available: Decimal,
    /// Held by open orders.
    pub hold: Decimal,
}

impl Balance {
    pub fn new(asset: impl Into<String>, available: Decimal) -> Self {
        Self { asset: Asset::new(asset), available, hold: Decimal::ZERO }
    }

    pub fn total(&self) -> Decimal {
        self.available + self.hold
    }
}

/// Fee rates as fractions of notional (0.001 is 0.1%, 10 basis points).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FeeTier {
    pub maker: Decimal,
    pub taker: Decimal,
}

impl FeeTier {
    /// Rates from basis points.
    pub fn from_bps(maker: i64, taker: i64) -> Self {
        Self { maker: Decimal::new(maker, 4), taker: Decimal::new(taker, 4) }
    }

    pub fn rate(&self, l: Liquidity) -> Decimal {
        match l {
            Liquidity::Maker => self.maker,
            Liquidity::Taker => self.taker,
        }
    }

    /// The fee on `notional`.
    pub fn fee(&self, l: Liquidity, notional: Decimal) -> Decimal {
        (notional.abs() * self.rate(l)).normalize()
    }
}

/// A net position in one market. `size` is signed: positive long, negative short.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub market: Market,
    pub size: Decimal,
    /// Weighted-average entry price of the open size.
    pub entry_price: Option<Price>,
    /// Profit or loss already closed, in the quote asset, before fees.
    pub realized_pnl: Decimal,
    /// Fees paid on fills, in the quote asset.
    pub fees: Decimal,
    pub liquidation_price: Option<Price>,
    pub updated_ms: i64,
}

impl Position {
    pub fn flat(market: Market) -> Self {
        Self {
            market,
            size: Decimal::ZERO,
            entry_price: None,
            realized_pnl: Decimal::ZERO,
            fees: Decimal::ZERO,
            liquidation_price: None,
            updated_ms: 0,
        }
    }

    pub fn side(&self) -> Option<PositionSide> {
        if self.size > Decimal::ZERO {
            Some(PositionSide::Long)
        } else if self.size < Decimal::ZERO {
            Some(PositionSide::Short)
        } else {
            None
        }
    }

    /// Profit or loss on the open size at `mark`, in the quote asset.
    pub fn unrealized_pnl(&self, mark: Price) -> Decimal {
        self.entry_price.map(|e| (mark.0 - e.0) * self.size).unwrap_or_default()
    }

    /// Applies a fill: grows, reduces, closes or flips the position. Fees are counted only
    /// when the fee asset is `quote`.
    pub fn apply(&mut self, fill: &Fill, quote: &Asset) {
        let delta = fill.side.sign() * fill.size.0;
        let price = fill.price.0;
        if fill.fee_asset == *quote {
            self.fees += fill.fee;
        }
        self.updated_ms = fill.at_ms;
        let same_way = self.size.is_zero() || (self.size > Decimal::ZERO) == (delta > Decimal::ZERO);
        if same_way {
            let old = self.size.abs();
            let entry = self.entry_price.map(|p| p.0).unwrap_or_default();
            let new = old + delta.abs();
            if !new.is_zero() {
                self.entry_price = Some(Price(((old * entry + delta.abs() * price) / new).normalize()));
            }
            self.size += delta;
            return;
        }
        let entry = self.entry_price.map(|p| p.0).unwrap_or(price);
        let closed = self.size.abs().min(delta.abs());
        let dir = if self.size > Decimal::ZERO { Decimal::ONE } else { Decimal::NEGATIVE_ONE };
        self.realized_pnl += closed * (price - entry) * dir;
        self.size += delta;
        if self.size.is_zero() {
            self.entry_price = None;
        } else if (self.size > Decimal::ZERO) != (dir > Decimal::ZERO) {
            // Flipped: the rest opened at this fill's price.
            self.entry_price = Some(fill.price);
        }
    }
}

/// Positions built from fills, one per market.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PositionBook {
    positions: BTreeMap<Market, Position>,
}

impl PositionBook {
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies a fill to its market's position and returns the position.
    pub fn apply(&mut self, fill: &Fill, quote: &Asset) -> &Position {
        let p = self.positions.entry(fill.market.clone()).or_insert_with(|| Position::flat(fill.market.clone()));
        p.apply(fill, quote);
        p
    }

    pub fn get(&self, market: &Market) -> Option<&Position> {
        self.positions.get(market)
    }

    /// Positions that are not flat.
    pub fn open(&self) -> impl Iterator<Item = &Position> {
        self.positions.values().filter(|p| !p.size.is_zero())
    }

    pub fn all(&self) -> impl Iterator<Item = &Position> {
        self.positions.values()
    }

    /// Realized PnL less fees, over all markets.
    pub fn net_realized(&self) -> Decimal {
        self.positions.values().map(|p| p.realized_pnl - p.fees).sum()
    }
}

/// An account's margin, for venues with derivatives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarginSummary {
    pub asset: Asset,
    /// Collateral plus unrealized PnL.
    pub equity: Decimal,
    pub initial_margin: Decimal,
    pub maintenance_margin: Decimal,
    pub available: Decimal,
    pub unrealized_pnl: Decimal,
}

impl MarginSummary {
    /// Equity over maintenance margin; below 1 means liquidation.
    pub fn health(&self) -> Option<Decimal> {
        (!self.maintenance_margin.is_zero()).then(|| self.equity / self.maintenance_margin)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    fn fill(side: Side, price: &str, size: &str) -> Fill {
        Fill {
            trade_id: format!("{side:?}{price}{size}"),
            order_id: OrderId::new("o"),
            client_id: None,
            market: Market::new("v", "BTC-PERP"),
            side,
            price: Price::parse(price).unwrap(),
            size: Qty::parse(size).unwrap(),
            fee: d("1"),
            fee_asset: Asset::new("USD"),
            liquidity: Liquidity::Taker,
            at_ms: 1,
        }
    }

    #[test]
    fn grow_reduce_flip_close() {
        let usd = Asset::new("USD");
        let mut b = PositionBook::new();
        b.apply(&fill(Side::Buy, "100", "1"), &usd);
        let p = b.apply(&fill(Side::Buy, "110", "1"), &usd).clone();
        assert_eq!((p.size, p.entry_price), (d("2"), Some(Price::parse("105").unwrap())));
        assert_eq!(p.unrealized_pnl(Price::parse("115").unwrap()), d("20"));
        let p = b.apply(&fill(Side::Sell, "120", "1"), &usd).clone();
        assert_eq!((p.size, p.realized_pnl, p.entry_price), (d("1"), d("15"), Some(Price::parse("105").unwrap())));
        let p = b.apply(&fill(Side::Sell, "100", "3"), &usd).clone();
        assert_eq!(
            (p.size, p.realized_pnl, p.entry_price, p.side()),
            (d("-2"), d("10"), Some(Price::parse("100").unwrap()), Some(PositionSide::Short))
        );
        let p = b.apply(&fill(Side::Buy, "90", "2"), &usd).clone();
        assert_eq!((p.size, p.realized_pnl, p.entry_price, p.fees), (d("0"), d("30"), None, d("5")));
        assert_eq!(b.open().count(), 0);
        assert_eq!(b.net_realized(), d("25"));
    }

    #[test]
    fn fees_and_balances() {
        let t = FeeTier::from_bps(10, 25);
        assert_eq!((t.maker, t.taker), (d("0.001"), d("0.0025")));
        assert_eq!(t.fee(Liquidity::Taker, d("-1000")), d("2.5"));
        let mut bal = Balance::new("USD", d("90"));
        bal.hold = d("10");
        assert_eq!(bal.total(), d("100"));
        let m = MarginSummary {
            asset: Asset::new("USD"),
            equity: d("150"),
            initial_margin: d("200"),
            maintenance_margin: d("100"),
            available: d("0"),
            unrealized_pnl: d("0"),
        };
        assert_eq!(m.health(), Some(d("1.5")));
    }
}
