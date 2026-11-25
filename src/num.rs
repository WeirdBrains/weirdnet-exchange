//! Exact decimal prices and quantities.
//!
//! [`Price`] and [`Qty`] wrap [`Decimal`] so a size can't be passed where a price belongs.
//! They serialize as strings (`"64250.10"`), never as floats. `Price * Qty` is the notional,
//! a plain [`Decimal`] in the quote asset.

use std::fmt;
use std::ops::{Add, Mul, Sub};
use std::str::FromStr;

pub use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::ExchangeError;

/// How to round to an increment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rounding {
    /// Toward negative infinity (sizes, buy prices).
    Down,
    /// Toward positive infinity (sell prices).
    Up,
    /// To the nearest multiple, halves away from zero.
    Nearest,
}

/// `value` rounded to a multiple of `increment`. A zero or negative increment leaves it as is.
pub fn round_to(value: Decimal, increment: Decimal, mode: Rounding) -> Decimal {
    if increment <= Decimal::ZERO {
        return value;
    }
    let n = value / increment;
    let n = match mode {
        Rounding::Down => n.floor(),
        Rounding::Up => n.ceil(),
        Rounding::Nearest => n.round_dp_with_strategy(0, rust_decimal::RoundingStrategy::MidpointAwayFromZero),
    };
    (n * increment).normalize()
}

/// `value` is a whole multiple of `increment` (always true for a zero increment).
pub fn is_multiple(value: Decimal, increment: Decimal) -> bool {
    increment <= Decimal::ZERO || (value % increment).is_zero()
}

macro_rules! decimal_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Decimal);

        impl $name {
            pub const ZERO: Self = Self(Decimal::ZERO);

            pub fn new(value: Decimal) -> Self {
                Self(value)
            }

            /// Parses an exact decimal string such as `"0.00012"`.
            pub fn parse(s: &str) -> Result<Self, ExchangeError> {
                Decimal::from_str_exact(s.trim()).map(Self).map_err(|e| ExchangeError::invalid(format!("{s:?}: {e}")))
            }

            pub fn value(self) -> Decimal {
                self.0
            }

            pub fn is_zero(self) -> bool {
                self.0.is_zero()
            }

            pub fn is_positive(self) -> bool {
                self.0 > Decimal::ZERO
            }

            /// Rounded to a multiple of `increment`.
            pub fn round_to(self, increment: Decimal, mode: Rounding) -> Self {
                Self(round_to(self.0, increment, mode))
            }

            pub fn min(self, other: Self) -> Self {
                if other < self { other } else { self }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::Display::fmt(&self.0.normalize(), f)
            }
        }

        impl FromStr for $name {
            type Err = ExchangeError;
            fn from_str(s: &str) -> Result<Self, ExchangeError> {
                Self::parse(s)
            }
        }

        impl From<Decimal> for $name {
            fn from(d: Decimal) -> Self {
                Self(d)
            }
        }

        impl Add for $name {
            type Output = Self;
            fn add(self, o: Self) -> Self {
                Self(self.0 + o.0)
            }
        }

        impl Sub for $name {
            type Output = Self;
            fn sub(self, o: Self) -> Self {
                Self(self.0 - o.0)
            }
        }
    };
}

decimal_newtype!(
    /// A price in the quote asset per unit of the base asset.
    Price
);
decimal_newtype!(
    /// A quantity of the base asset (or contracts).
    Qty
);

impl Mul<Qty> for Price {
    type Output = Decimal;
    fn mul(self, q: Qty) -> Decimal {
        self.0 * q.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(s: &str) -> Decimal {
        Decimal::from_str_exact(s).unwrap()
    }

    #[test]
    fn exact_parsing_rounding_and_strings() {
        let p = Price::parse("64250.105").unwrap();
        assert_eq!(p.round_to(d("0.01"), Rounding::Down), Price::parse("64250.10").unwrap());
        assert_eq!(p.round_to(d("0.01"), Rounding::Up).to_string(), "64250.11");
        assert_eq!(p.round_to(d("0.01"), Rounding::Nearest).to_string(), "64250.11");
        assert_eq!(Qty::parse("0.000123456").unwrap().round_to(d("0.00001"), Rounding::Down).to_string(), "0.00012");
        assert!(Price::parse("abc").is_err());
        assert!(is_multiple(d("0.30"), d("0.1")) && !is_multiple(d("0.35"), d("0.1")) && is_multiple(d("7"), Decimal::ZERO));
        // 0.1 + 0.2 is exactly 0.3.
        assert_eq!(Price::parse("0.1").unwrap() + Price::parse("0.2").unwrap(), Price::parse("0.3").unwrap());
        assert_eq!(Price::parse("2.5").unwrap() * Qty::parse("4").unwrap(), d("10"));
        let json = serde_json::to_string(&Price::parse("64250.10").unwrap()).unwrap();
        assert_eq!(json, "\"64250.10\"");
        assert_eq!(serde_json::from_str::<Qty>("\"0.5\"").unwrap(), Qty::parse("0.5").unwrap());
    }
}
