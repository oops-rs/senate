//! Who sent a request.
//!
//! Every `/v1` request carries `Authorization: Bearer <token>`, and the token
//! names a **person** (a [`Name`]). Turning a token into a person is the
//! [`Authenticate`] seam: the server holds an `Arc<dyn Authenticate>`, and the
//! default, [`StaticTokens`], reads tokens from configuration. A private layer
//! that has its own identity system implements the trait and hands it to
//! [`ServerBuilder::auth`](crate::ServerBuilder::auth).
//!
//! What the person may then do (act as themselves and as their own agents) is
//! the roster's rule, not the authenticator's; see
//! [`Roster::may_act_as`](senate_core::Roster::may_act_as).
//!
//! # Static tokens
//!
//! Tokens are never compared as strings. Each person's token is held as its
//! SHA-256 (computed from `token`, or decoded from `token_sha256`), the
//! presented token is hashed, and the digest is compared against every entry
//! in constant time without an early exit, so neither the match position nor
//! the token length leaks through timing.

use async_trait::async_trait;
use senate_core::Name;
use sha2::{Digest, Sha256};

use crate::config::{ConfigError, TokenConfig};

/// Turns a bearer token into the person it names.
///
/// An implementation must not reveal through timing which part of a token
/// was wrong: compare secrets in constant time, as [`StaticTokens`] does.
/// `None` is "not a valid token", answered 401; the server never learns why.
#[async_trait]
pub trait Authenticate: Send + Sync + 'static {
    /// The person `token` (the bare token, without the `Bearer ` scheme)
    /// belongs to.
    async fn authenticate(&self, token: &str) -> Option<Name>;
}

pub(crate) fn sha256(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

fn constant_time_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The token inside an `Authorization` header value (`Bearer <token>`, scheme
/// case-insensitive). `None` for another scheme or an empty token.
pub fn bearer_token(header_value: &str) -> Option<&str> {
    let (scheme, token) = header_value.trim().split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    (!token.is_empty()).then_some(token)
}

/// The default [`Authenticate`]: the tokens in configuration, each naming one
/// person. Several tokens may name one person (a laptop and a server); two
/// people may not share a token.
#[derive(Clone)]
pub struct StaticTokens {
    tokens: Vec<(Name, [u8; 32])>,
}

impl StaticTokens {
    pub fn new(tokens: &[TokenConfig]) -> Result<Self, ConfigError> {
        let mut digests: Vec<(Name, [u8; 32])> = Vec::with_capacity(tokens.len());
        for token in tokens {
            let digest = token.digest()?;
            if let Some((other, _)) = digests
                .iter()
                .find(|(person, d)| d == &digest && *person != token.person)
            {
                return Err(ConfigError::Invalid(format!(
                    "people {other} and {} share a token",
                    token.person
                )));
            }
            digests.push((token.person.clone(), digest));
        }
        Ok(Self { tokens: digests })
    }

    /// The person a bearer token names. Synchronous: nothing here waits.
    pub fn lookup(&self, token: &str) -> Option<Name> {
        let presented = sha256(token);
        let mut found = None;
        for (person, digest) in &self.tokens {
            if constant_time_eq(&presented, digest) {
                found = Some(person.clone());
            }
        }
        found
    }

    /// The person an `Authorization` header value names.
    pub fn lookup_header(&self, value: &str) -> Option<Name> {
        self.lookup(bearer_token(value)?)
    }
}

#[async_trait]
impl Authenticate for StaticTokens {
    async fn authenticate(&self, token: &str) -> Option<Name> {
        self.lookup(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn name(h: &str) -> Name {
        Name::new(h).unwrap()
    }

    #[test]
    fn plaintext_and_hashed_tokens_authenticate() {
        let bob = TokenConfig {
            token: None,
            token_sha256: Some(hex::encode(sha256("bob-token")).to_uppercase()),
            ..TokenConfig::with_token(name("bob"), "")
        };
        let auth = StaticTokens::new(&[TokenConfig::with_token(name("alice"), "alice-token"), bob])
            .unwrap();
        assert_eq!(auth.lookup("alice-token"), Some(name("alice")));
        assert_eq!(auth.lookup("bob-token"), Some(name("bob")));
        assert_eq!(auth.lookup("alice-token "), None);
        assert_eq!(auth.lookup(""), None);
        assert_eq!(
            auth.lookup_header("Bearer alice-token"),
            Some(name("alice"))
        );
        assert_eq!(auth.lookup_header("bearer  bob-token"), Some(name("bob")));
        assert_eq!(auth.lookup_header("Basic alice-token"), None);
        assert_eq!(auth.lookup_header("Bearer"), None);
        assert_eq!(auth.lookup_header("alice-token"), None);
    }

    #[test]
    fn shared_tokens_are_rejected() {
        let result = StaticTokens::new(&[
            TokenConfig::with_token(name("alice"), "same"),
            TokenConfig::with_token(name("bob"), "same"),
        ]);
        assert!(matches!(result, Err(ConfigError::Invalid(_))));
    }

    #[test]
    fn one_person_may_hold_several_tokens() {
        let auth = StaticTokens::new(&[
            TokenConfig::with_token(name("alice"), "laptop"),
            TokenConfig::with_token(name("alice"), "desktop"),
        ])
        .unwrap();
        assert_eq!(auth.lookup("laptop"), Some(name("alice")));
        assert_eq!(auth.lookup("desktop"), Some(name("alice")));
    }

    #[test]
    fn the_comparison_has_no_early_exit() {
        // Digests that differ only in their first or only in their last byte
        // are told apart by the same full-width comparison.
        let a = sha256("a");
        let mut first = a;
        first[0] ^= 1;
        let mut last = a;
        last[31] ^= 1;
        assert!(constant_time_eq(&a, &a));
        assert!(!constant_time_eq(&a, &first));
        assert!(!constant_time_eq(&a, &last));
    }

    #[tokio::test]
    async fn the_default_implements_the_seam() {
        let auth: std::sync::Arc<dyn Authenticate> = std::sync::Arc::new(
            StaticTokens::new(&[TokenConfig::with_token(name("alice"), "t")]).unwrap(),
        );
        assert_eq!(auth.authenticate("t").await, Some(name("alice")));
        assert_eq!(auth.authenticate("u").await, None);
    }
}
