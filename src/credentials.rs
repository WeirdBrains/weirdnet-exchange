//! Credentials typed by environment.
//!
//! A [`SecretSource`] holds an API secret somewhere (in memory, in `weirdnet-vault` with the
//! `vault` feature) and says which [`Environment`] it belongs to. A [`Credential<Live>`] can
//! only be made from a live source and a [`Credential<Test>`] only from a test one, so an
//! adapter whose live constructor takes `Credential<Live>` cannot be built from test
//! credentials, and a test (sandbox) constructor cannot be handed live ones. The environment is
//! checked again on every use.
//!
//! ```
//! use weirdnet_exchange::{Credential, Environment, Live, StaticSecret, Test};
//!
//! let test = StaticSecret::new(Environment::Test, "sandbox key", b"key:secret".to_vec());
//! assert!(Credential::<Live>::new(test.clone()).is_err());
//! let cred = Credential::<Test>::new(test).unwrap();
//! assert_eq!(cred.with_secret(|s| s.len()).unwrap(), 10);
//! ```

use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use zeroize::Zeroizing;

use crate::exchange::Environment;
use crate::{ExchangeError, Result};

/// Somewhere a secret is kept.
pub trait SecretSource: Send + Sync {
    /// The secret's environment.
    fn environment(&self) -> Environment;

    /// A name that is safe to log (never the secret).
    fn label(&self) -> String;

    /// Calls `f` with the secret bytes. Implementations should not keep copies.
    fn with_secret(&self, f: &mut dyn FnMut(&[u8])) -> Result<()>;
}

mod sealed {
    pub trait Sealed {}
}

/// [`Live`] or [`Test`].
pub trait EnvKind: sealed::Sealed + Send + Sync + 'static {
    const ENVIRONMENT: Environment;
}

/// Marks live credentials.
#[derive(Debug, Clone, Copy)]
pub enum Live {}

/// Marks test (sandbox, paper) credentials.
#[derive(Debug, Clone, Copy)]
pub enum Test {}

impl sealed::Sealed for Live {}
impl sealed::Sealed for Test {}
impl EnvKind for Live {
    const ENVIRONMENT: Environment = Environment::Live;
}
impl EnvKind for Test {
    const ENVIRONMENT: Environment = Environment::Test;
}

/// A secret source checked to be in environment `E`.
pub struct Credential<E: EnvKind> {
    source: Arc<dyn SecretSource>,
    _env: PhantomData<E>,
}

impl<E: EnvKind> Clone for Credential<E> {
    fn clone(&self) -> Self {
        Self { source: self.source.clone(), _env: PhantomData }
    }
}

impl<E: EnvKind> Credential<E> {
    /// Refuses a source from the other environment.
    pub fn new(source: impl SecretSource + 'static) -> Result<Self> {
        Self::from_arc(Arc::new(source))
    }

    pub fn from_arc(source: Arc<dyn SecretSource>) -> Result<Self> {
        check::<E>(&*source)?;
        Ok(Self { source, _env: PhantomData })
    }

    pub fn environment(&self) -> Environment {
        E::ENVIRONMENT
    }

    pub fn label(&self) -> String {
        self.source.label()
    }

    /// Calls `f` with the secret, after checking the source's environment again.
    pub fn with_secret<R>(&self, f: impl FnOnce(&[u8]) -> R) -> Result<R> {
        check::<E>(&*self.source)?;
        let mut f = Some(f);
        let mut out = None;
        self.source.with_secret(&mut |s| {
            if let Some(f) = f.take() {
                out = Some(f(s));
            }
        })?;
        out.ok_or_else(|| ExchangeError::Auth { message: format!("{} gave no secret", self.source.label()) })
    }
}

fn check<E: EnvKind>(s: &dyn SecretSource) -> Result<()> {
    let got = s.environment();
    if got == E::ENVIRONMENT {
        Ok(())
    } else {
        Err(ExchangeError::Environment { message: format!("{} is a {} credential; {} needed", s.label(), got.as_str(), E::ENVIRONMENT.as_str()) })
    }
}

impl<E: EnvKind> fmt::Debug for Credential<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credential").field("environment", &E::ENVIRONMENT).field("label", &self.source.label()).finish_non_exhaustive()
    }
}

/// A secret held in memory (zeroed on drop), for tests and simple setups.
#[derive(Clone)]
pub struct StaticSecret {
    environment: Environment,
    label: String,
    bytes: Zeroizing<Vec<u8>>,
}

impl StaticSecret {
    pub fn new(environment: Environment, label: impl Into<String>, bytes: Vec<u8>) -> Self {
        Self { environment, label: label.into(), bytes: Zeroizing::new(bytes) }
    }
}

impl fmt::Debug for StaticSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StaticSecret").field("environment", &self.environment).field("label", &self.label).finish_non_exhaustive()
    }
}

impl SecretSource for StaticSecret {
    fn environment(&self) -> Environment {
        self.environment
    }

    fn label(&self) -> String {
        self.label.clone()
    }

    fn with_secret(&self, f: &mut dyn FnMut(&[u8])) -> Result<()> {
        f(&self.bytes);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    struct Flip(AtomicBool);
    impl SecretSource for Flip {
        fn environment(&self) -> Environment {
            if self.0.load(Ordering::SeqCst) { Environment::Live } else { Environment::Test }
        }
        fn label(&self) -> String {
            "flip".into()
        }
        fn with_secret(&self, f: &mut dyn FnMut(&[u8])) -> Result<()> {
            f(b"s");
            Ok(())
        }
    }

    #[test]
    fn environments_are_enforced_and_debug_is_redacted() {
        let live = StaticSecret::new(Environment::Live, "prod", b"hunter2".to_vec());
        assert!(matches!(Credential::<Test>::new(live.clone()), Err(ExchangeError::Environment { .. })));
        let c = Credential::<Live>::new(live.clone()).unwrap();
        assert_eq!(c.with_secret(|s| s.to_vec()).unwrap(), b"hunter2");
        assert!(!format!("{c:?}{live:?}").contains("hunter2"));
        let flip = Arc::new(Flip(AtomicBool::new(false)));
        let c = Credential::<Test>::from_arc(flip.clone()).unwrap();
        flip.0.store(true, Ordering::SeqCst);
        assert!(matches!(c.with_secret(|_| ()), Err(ExchangeError::Environment { .. })), "re-checked on use");
    }
}
