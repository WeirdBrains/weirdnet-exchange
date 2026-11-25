//! Credentials from weirdnet-vault.
#![cfg(feature = "vault")]

use std::sync::Arc;

use weirdnet_exchange::vault::{VaultSecret, scope_for};
use weirdnet_exchange::{AccountId, Credential, ExchangeError, Live, Test, VenueId};
use weirdnet_vault::{AccessRequest, Caller, Decision, Environment, GrantAction, GrantTarget, NewSecret, Vault, VaultOptions};

/// What an adapter's live constructor looks like: it can only take live credentials.
struct LiveAdapter {
    credential: Credential<Live>,
}

impl LiveAdapter {
    fn new(credential: Credential<Live>) -> Self {
        Self { credential }
    }
}

#[test]
fn vault_credentials_keep_test_and_live_apart() {
    let allow = |_: &AccessRequest<'_>| Decision::Allow;
    let vault = Arc::new(Vault::open_in_memory(Arc::new(allow), VaultOptions::default()).unwrap());
    let admin = Caller::live("admin");
    let (acct, venue) = (AccountId::new("acct-1"), VenueId::new("example"));
    let scope = scope_for(&acct, &venue).unwrap();
    let t = vault.create(&admin, NewSecret::new(scope.clone(), "api", Environment::Test).kind("api-key"), b"sandbox-key").unwrap();
    let l = vault.create(&admin, NewSecret::new(scope, "api", Environment::Live).kind("api-key"), b"live-key").unwrap();
    for id in [t.id, l.id.clone()] {
        vault.grant(&admin, GrantTarget::Secret(id), "bot", &[GrantAction::Use], None).unwrap();
    }

    // A test secret can't become a live credential, so it can't build the live adapter.
    let test_secret = VaultSecret::find(vault.clone(), Caller::test("bot"), &acct, &venue, "api", weirdnet_exchange::Environment::Test).unwrap();
    assert!(matches!(Credential::<Live>::new(test_secret), Err(ExchangeError::Environment { .. })));
    let test = VaultSecret::credential::<Test>(vault.clone(), Caller::test("bot"), &acct, &venue, "api").unwrap();
    assert_eq!(test.with_secret(|s| s.to_vec()).unwrap(), b"sandbox-key");
    assert!(!format!("{test:?}").contains("sandbox-key"));

    // A test-bound caller can't reach the live secret at all.
    assert!(matches!(
        VaultSecret::credential::<Live>(vault.clone(), Caller::test("bot"), &acct, &venue, "api"),
        Err(ExchangeError::Environment { .. })
    ));

    let live = VaultSecret::credential::<Live>(vault.clone(), Caller::live("bot"), &acct, &venue, "api").unwrap();
    let adapter = LiveAdapter::new(live);
    assert_eq!(adapter.credential.with_secret(|s| s.len()).unwrap(), 8);
    assert!(adapter.credential.label().contains("acct-1/example"));
    assert!(VaultSecret::credential::<Live>(vault.clone(), Caller::live("bot"), &acct, &venue, "missing").is_err());

    // Without a use grant the vault refuses, even for a live caller.
    let other = VaultSecret::credential::<Live>(vault, Caller::live("someone-else"), &acct, &venue, "api").unwrap();
    assert!(matches!(other.with_secret(|_| ()), Err(ExchangeError::Auth { .. })));
}
