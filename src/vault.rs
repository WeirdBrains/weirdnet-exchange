//! Credentials from `weirdnet-vault` (feature `vault`).
//!
//! Secrets live in the vault under the scope `<account>/<venue>`, one per name and
//! environment. A [`VaultSecret`] names one and uses it through [`Vault::use_secret`], so the
//! plaintext is opened only for the call and never kept. Its environment is the secret's own,
//! fixed in the vault when it was created, so wrapping it in [`Credential<Live>`] fails for a
//! test secret. The vault also refuses a test-bound caller any live secret.
//!
//! [`Credential<Live>`]: crate::Credential

use std::sync::Arc;

use weirdnet_vault::{Caller, Scope, SecretMeta, Vault, VaultError};

use crate::credentials::{Credential, EnvKind, SecretSource};
use crate::exchange::Environment;
use crate::instrument::{AccountId, VenueId};
use crate::{ExchangeError, Result};

/// This crate's environment as the vault's.
pub fn to_vault_env(e: Environment) -> weirdnet_vault::Environment {
    match e {
        Environment::Test => weirdnet_vault::Environment::Test,
        Environment::Live => weirdnet_vault::Environment::Live,
    }
}

/// The vault's environment as this crate's.
pub fn from_vault_env(e: weirdnet_vault::Environment) -> Environment {
    match e {
        weirdnet_vault::Environment::Test => Environment::Test,
        weirdnet_vault::Environment::Live => Environment::Live,
    }
}

/// The vault scope for an account's credentials on a venue: `<account>/<venue>`.
pub fn scope_for(account: &AccountId, venue: &VenueId) -> Result<Scope> {
    Scope::new([account.as_str(), venue.as_str()]).map_err(vault_err)
}

fn vault_err(e: VaultError) -> ExchangeError {
    match e {
        VaultError::NotFound(m) => ExchangeError::NotFound { message: m },
        e @ VaultError::EnvironmentMismatch { .. } => ExchangeError::Environment { message: e.to_string() },
        other => ExchangeError::Auth { message: other.to_string() },
    }
}

/// One secret in a vault, used as `caller`.
pub struct VaultSecret {
    vault: Arc<Vault>,
    caller: Caller,
    meta: SecretMeta,
}

impl VaultSecret {
    /// Looks up the `environment` secret `name` for `account` on `venue`. A test-bound caller
    /// is refused a live secret here, before the vault is asked.
    pub fn find(vault: Arc<Vault>, caller: Caller, account: &AccountId, venue: &VenueId, name: &str, environment: Environment) -> Result<Self> {
        let env = to_vault_env(environment);
        if !caller.environment.may_touch(env) {
            return Err(ExchangeError::Environment { message: format!("a {} caller may not use a {env} credential", caller.environment) });
        }
        let scope = scope_for(account, venue)?;
        let meta = vault.find(&caller, &scope, name, env).map_err(vault_err)?;
        Ok(Self { vault, caller, meta })
    }

    /// [`find`](Self::find), then a [`Credential`] for environment `E`.
    pub fn credential<E: EnvKind>(vault: Arc<Vault>, caller: Caller, account: &AccountId, venue: &VenueId, name: &str) -> Result<Credential<E>> {
        Credential::new(Self::find(vault, caller, account, venue, name, E::ENVIRONMENT)?)
    }

    pub fn meta(&self) -> &SecretMeta {
        &self.meta
    }
}

impl SecretSource for VaultSecret {
    fn environment(&self) -> Environment {
        from_vault_env(self.meta.environment)
    }

    fn label(&self) -> String {
        format!("{}/{} ({})", self.meta.scope, self.meta.name, self.meta.environment)
    }

    fn with_secret(&self, f: &mut dyn FnMut(&[u8])) -> Result<()> {
        self.vault.use_secret(&self.caller, &self.meta.id, |p| f(p.as_bytes())).map_err(vault_err)
    }
}
