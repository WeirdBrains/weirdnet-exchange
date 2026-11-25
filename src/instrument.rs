//! Venues, assets, products and markets.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::num::{Decimal, Price, Qty, Rounding, is_multiple};
use crate::order::{OrderRequest, OrderSize, OrderType};
use crate::{ExchangeError, RejectReason};

macro_rules! id_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self {
                Self(s.into())
            }

            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl From<&str> for $name {
            fn from(s: &str) -> Self {
                Self(s.to_string())
            }
        }
    };
}

id_newtype!(
    /// A venue (exchange) id, such as `example-exchange`.
    VenueId
);
id_newtype!(
    /// A venue's product id, such as `BTC-USD`.
    ProductId
);
id_newtype!(
    /// An asset or currency code, such as `BTC` or `USD`.
    Asset
);
id_newtype!(
    /// A trading account id, chosen by the app.
    AccountId
);

/// One product on one venue.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Market {
    pub venue: VenueId,
    pub product: ProductId,
}

impl Market {
    pub fn new(venue: impl Into<String>, product: impl Into<String>) -> Self {
        Self { venue: VenueId::new(venue), product: ProductId::new(product) }
    }
}

impl fmt::Display for Market {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.venue, self.product)
    }
}

/// What a product is.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InstrumentKind {
    Spot,
    /// A future that settles at `expiry_ms`; one contract is `contract_size` of the base.
    DatedFuture {
        expiry_ms: i64,
        contract_size: Decimal,
    },
    /// A perpetual future with funding every `funding_interval_ms`.
    Perpetual {
        contract_size: Decimal,
        funding_interval_ms: i64,
    },
}

impl InstrumentKind {
    pub fn is_derivative(&self) -> bool {
        !matches!(self, InstrumentKind::Spot)
    }
}

/// Whether a product trades right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProductStatus {
    #[default]
    Online,
    /// Only post-only limit orders.
    PostOnly,
    /// Only limit orders.
    LimitOnly,
    /// Only cancels.
    CancelOnly,
    Halted,
    Delisted,
}

/// A tradable product and its rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Product {
    pub market: Market,
    pub base: Asset,
    pub quote: Asset,
    pub kind: InstrumentKind,
    /// Prices are multiples of this.
    pub price_increment: Decimal,
    /// Base sizes are multiples of this.
    pub size_increment: Decimal,
    pub min_size: Qty,
    pub max_size: Option<Qty>,
    /// Smallest price × size, in the quote asset.
    pub min_notional: Decimal,
    pub status: ProductStatus,
}

impl Product {
    /// A spot product with no increments or limits (set them with the `with_` methods).
    pub fn spot(market: Market, base: impl Into<String>, quote: impl Into<String>) -> Self {
        Self {
            market,
            base: Asset::new(base),
            quote: Asset::new(quote),
            kind: InstrumentKind::Spot,
            price_increment: Decimal::ZERO,
            size_increment: Decimal::ZERO,
            min_size: Qty::ZERO,
            max_size: None,
            min_notional: Decimal::ZERO,
            status: ProductStatus::Online,
        }
    }

    pub fn with_increments(mut self, price: Decimal, size: Decimal) -> Self {
        self.price_increment = price;
        self.size_increment = size;
        self
    }

    pub fn with_limits(mut self, min_size: Qty, max_size: Option<Qty>, min_notional: Decimal) -> Self {
        self.min_size = min_size;
        self.max_size = max_size;
        self.min_notional = min_notional;
        self
    }

    pub fn with_kind(mut self, kind: InstrumentKind) -> Self {
        self.kind = kind;
        self
    }

    pub fn with_status(mut self, status: ProductStatus) -> Self {
        self.status = status;
        self
    }

    /// A price rounded to the price increment.
    pub fn round_price(&self, p: Price, mode: Rounding) -> Price {
        p.round_to(self.price_increment, mode)
    }

    /// A size rounded down to the size increment.
    pub fn round_size(&self, q: Qty) -> Qty {
        q.round_to(self.size_increment, Rounding::Down)
    }

    /// Checks an order against the product's status, increments and limits, as a venue would.
    pub fn check_order(&self, r: &OrderRequest) -> Result<(), ExchangeError> {
        let reject = |reason, msg: String| Err(ExchangeError::rejected(reason, msg));
        if r.market != self.market {
            return reject(RejectReason::UnknownProduct, format!("order for {} checked against {}", r.market, self.market));
        }
        let limit = r.limit_price();
        match self.status {
            ProductStatus::Online => {}
            ProductStatus::LimitOnly if limit.is_some() => {}
            ProductStatus::PostOnly if matches!(r.order_type, OrderType::Limit { post_only: true, .. }) => {}
            s => return reject(RejectReason::TradingDisabled, format!("{} is {s:?}", self.market)),
        }
        for p in [limit, r.stop_price()].into_iter().flatten() {
            if !p.is_positive() || !is_multiple(p.0, self.price_increment) {
                return reject(RejectReason::BadIncrement, format!("price {p} is not a positive multiple of {}", self.price_increment));
            }
        }
        match r.size {
            OrderSize::Base(q) => {
                if !q.is_positive() || !is_multiple(q.0, self.size_increment) {
                    return reject(RejectReason::BadIncrement, format!("size {q} is not a positive multiple of {}", self.size_increment));
                }
                if q < self.min_size {
                    return reject(RejectReason::BelowMinSize, format!("size {q} is below {}", self.min_size));
                }
                if self.max_size.is_some_and(|m| q > m) {
                    return reject(RejectReason::AboveMaxSize, format!("size {q} is above the maximum"));
                }
                if let Some(p) = limit
                    && p * q < self.min_notional
                {
                    return reject(RejectReason::BelowMinNotional, format!("notional {} is below {}", p * q, self.min_notional));
                }
            }
            OrderSize::Quote(amount) => {
                if !matches!(r.order_type, OrderType::Market) {
                    return Err(ExchangeError::invalid("a quote-sized order must be a market order"));
                }
                if amount <= Decimal::ZERO {
                    return reject(RejectReason::BelowMinNotional, "quote amount must be positive".into());
                }
                if amount < self.min_notional {
                    return reject(RejectReason::BelowMinNotional, format!("notional {amount} is below {}", self.min_notional));
                }
            }
        }
        Ok(())
    }
}
