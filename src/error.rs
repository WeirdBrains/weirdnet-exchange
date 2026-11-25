//! Errors adapters and helpers return.

use serde::{Deserialize, Serialize};

use crate::order::OrderStatus;

/// Shorthand for results with [`ExchangeError`].
pub type Result<T, E = ExchangeError> = std::result::Result<T, E>;

/// Why a venue refused an order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum RejectReason {
    InsufficientFunds,
    BelowMinSize,
    AboveMaxSize,
    BelowMinNotional,
    /// The price or size is not a multiple of the product's increment.
    BadIncrement,
    /// A post-only order would have taken liquidity.
    PostOnlyWouldTake,
    /// A fill-or-kill order could not fill in full.
    WouldNotFill,
    UnknownProduct,
    /// The client order id was already used for a different order.
    DuplicateClientId,
    /// The product is halted, delisted, cancel-only, or limit-only for a market order.
    TradingDisabled,
    /// The order type or time in force is not supported by this venue.
    UnsupportedOrder,
    /// A reduce-only order would open or grow a position.
    ReduceOnly,
    Other,
}

/// What went wrong talking to a venue, or in a helper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case", tag = "kind")]
#[non_exhaustive]
pub enum ExchangeError {
    /// Too many requests; wait `retry_after_ms` when the venue said how long.
    #[error("rate limited")]
    RateLimited { retry_after_ms: Option<u64> },
    /// The venue refused the request.
    #[error("rejected ({reason:?}): {message}")]
    Rejected { reason: RejectReason, message: String },
    /// A timeout or server error. With `outcome_unknown` (an order that may or may not have
    /// been placed), look the order up by its client id before retrying.
    #[error("transient failure: {message}")]
    Transient { message: String, outcome_unknown: bool },
    /// The credentials were refused.
    #[error("authentication failed: {message}")]
    Auth { message: String },
    /// The venue or product is down for maintenance.
    #[error("unavailable: {message}")]
    Unavailable { message: String },
    /// The adapter or venue does not support this.
    #[error("unsupported: {message}")]
    Unsupported { message: String },
    #[error("not found: {message}")]
    NotFound { message: String },
    /// The request or value is not valid.
    #[error("invalid: {message}")]
    Invalid { message: String },
    /// An order book delta did not follow the last sequence number; fetch a new snapshot.
    #[error("book gap: expected sequence {expected}, got {got}")]
    BookGap { expected: u64, got: u64 },
    /// An order event that is not possible in the order's state.
    #[error("order in state {from:?} cannot take {event}")]
    InvalidTransition { from: OrderStatus, event: String },
    /// Credentials for the wrong environment (test credentials for a live adapter).
    #[error("environment: {message}")]
    Environment { message: String },
}

impl ExchangeError {
    pub fn rejected(reason: RejectReason, message: impl Into<String>) -> Self {
        Self::Rejected { reason, message: message.into() }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid { message: message.into() }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound { message: message.into() }
    }

    pub fn unsupported(message: impl Into<String>) -> Self {
        Self::Unsupported { message: message.into() }
    }

    pub fn transient(message: impl Into<String>, outcome_unknown: bool) -> Self {
        Self::Transient { message: message.into(), outcome_unknown }
    }

    /// The same request may succeed later (rate limits, transient failures, maintenance).
    /// A transient failure with an unknown outcome is retryable only after checking.
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::RateLimited { .. } | Self::Unavailable { .. }) || matches!(self, Self::Transient { outcome_unknown: false, .. })
    }

    /// The reject reason, if the venue rejected the request.
    pub fn reject_reason(&self) -> Option<RejectReason> {
        match self {
            Self::Rejected { reason, .. } => Some(*reason),
            _ => None,
        }
    }
}
