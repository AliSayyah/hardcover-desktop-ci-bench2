//! Token sources for the API client.

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use super::oauth::{self, TokenError};
use super::{OAuthConfig, TokenStore, Tokens};
use crate::api::TokenSource;
use crate::http::{HttpTransport, UreqTransport};
use crate::{Error, Result};

/// Refresh this long before the access token expires (they last a week).
const REFRESH_MARGIN_MINUTES: i64 = 10;

/// A signed-in OAuth session.
///
/// Refresh tokens rotate on every use and replaying one makes Hardcover revoke
/// the whole chain, so a refresh happens under a lock (one at a time), is never
/// retried automatically, and the rotated pair is saved before it is used.
pub struct Session {
    cfg: OAuthConfig,
    store: Arc<dyn TokenStore>,
    http: Arc<dyn HttpTransport>,
    tokens: Mutex<Option<Tokens>>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("client_id", &self.cfg.client_id())
            .field("signed_in", &self.is_signed_in())
            .finish_non_exhaustive()
    }
}

impl Session {
    /// A session for freshly obtained tokens (from [`super::PendingSignIn::wait`]);
    /// saves them to `store`.
    pub fn start(cfg: OAuthConfig, store: Arc<dyn TokenStore>, tokens: Tokens) -> Result<Self> {
        Self::start_with(cfg, store, tokens, Arc::new(UreqTransport::new()))
    }

    /// The session saved by a previous run, if any.
    pub fn restore(cfg: OAuthConfig, store: Arc<dyn TokenStore>) -> Result<Option<Self>> {
        Self::restore_with(cfg, store, Arc::new(UreqTransport::new()))
    }

    pub(crate) fn start_with(
        cfg: OAuthConfig,
        store: Arc<dyn TokenStore>,
        tokens: Tokens,
        http: Arc<dyn HttpTransport>,
    ) -> Result<Self> {
        store.save(&tokens)?;
        Ok(Self {
            cfg,
            store,
            http,
            tokens: Mutex::new(Some(tokens)),
        })
    }

    pub(crate) fn restore_with(
        cfg: OAuthConfig,
        store: Arc<dyn TokenStore>,
        http: Arc<dyn HttpTransport>,
    ) -> Result<Option<Self>> {
        let Some(tokens) = store.load()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            cfg,
            store,
            http,
            tokens: Mutex::new(Some(tokens)),
        }))
    }

    fn lock(&self) -> MutexGuard<'_, Option<Tokens>> {
        self.tokens.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// False after [`Session::sign_out`] or after the refresh token was rejected.
    pub fn is_signed_in(&self) -> bool {
        self.lock().is_some()
    }

    pub fn expires_at(&self) -> Option<crate::api::Timestamp> {
        self.lock().as_ref().and_then(Tokens::expires_at)
    }

    /// Scopes granted at sign-in, when the server reported them.
    pub fn scope(&self) -> Option<String> {
        self.lock()
            .as_ref()
            .and_then(|t| t.scope().map(str::to_owned))
    }

    /// Revokes the tokens at Hardcover, then forgets them locally (store and memory).
    ///
    /// The local sign-out always happens. An `Err` means the server-side revoke
    /// (or clearing the store) failed; the user is signed out of this app either way.
    pub fn sign_out(&self) -> Result<()> {
        let tokens = self.lock().take();
        let revoked = match &tokens {
            Some(t) => oauth::revoke(&*self.http, &self.cfg, t),
            None => Ok(()),
        };
        self.store.clear()?;
        revoked
    }

    /// Refreshes while holding the lock. On rejection the session is dropped.
    fn refresh_locked(&self, guard: &mut MutexGuard<'_, Option<Tokens>>) -> Result<()> {
        let refresh_token = guard
            .as_ref()
            .and_then(|t| t.refresh_token().map(str::to_owned))
            .ok_or(Error::Unauthorized)?;
        match oauth::refresh(&*self.http, &self.cfg, &refresh_token) {
            Ok(new) => {
                // Hardcover always rotates, but keep the old refresh token if a
                // server ever omits it.
                let new = if new.refresh_token().is_none() {
                    Tokens::new(new.access_token(), Some(refresh_token), new.expires_at())
                        .with_scope(new.scope().map(str::to_owned))
                } else {
                    new
                };
                **guard = Some(new.clone());
                // In memory first: the old refresh token is spent either way.
                self.store.save(&new)
            }
            Err(TokenError::Rejected(_)) => {
                **guard = None;
                let _ = self.store.clear();
                Err(Error::Unauthorized)
            }
            Err(TokenError::Transient(e)) => Err(e),
        }
    }
}

impl TokenSource for Session {
    fn access_token(&self) -> Result<String> {
        let mut guard = self.lock();
        let tokens = guard.as_ref().ok_or(Error::Unauthorized)?;
        let now = chrono::Utc::now();
        let margin = chrono::Duration::minutes(REFRESH_MARGIN_MINUTES);
        if tokens.expires_within(now, margin) && tokens.refresh_token().is_some() {
            let still_valid = !tokens.expires_within(now, chrono::Duration::zero());
            let current = tokens.access_token().to_owned();
            match self.refresh_locked(&mut guard) {
                Ok(()) => {}
                // A network blip during a proactive refresh: keep using the old
                // token while it is still valid.
                Err(Error::Network(_) | Error::Timeout) if still_valid => return Ok(current),
                Err(e) => return Err(e),
            }
        }
        guard
            .as_ref()
            .map(|t| t.access_token().to_owned())
            .ok_or(Error::Unauthorized)
    }

    fn on_unauthorized(&self, rejected: &str) -> Result<bool> {
        let mut guard = self.lock();
        match guard.as_ref() {
            None => Ok(false),
            // Someone else refreshed since that request was sent.
            Some(t) if t.access_token() != rejected => Ok(true),
            Some(t) if t.refresh_token().is_none() => Ok(false),
            Some(_) => match self.refresh_locked(&mut guard) {
                Ok(()) => Ok(true),
                Err(Error::Unauthorized) => Ok(false),
                Err(e) => Err(e),
            },
        }
    }
}

/// A fixed bearer token, e.g. a personal access token for development. No refresh.
#[derive(Clone)]
pub struct StaticToken {
    token: String,
}

impl StaticToken {
    /// Environment variable read by [`StaticToken::from_env`].
    pub const ENV: &'static str = "HARDCOVER_TOKEN";

    /// Accepts the token with or without a `Bearer ` prefix or surrounding quotes.
    pub fn new(token: impl AsRef<str>) -> Option<Self> {
        let t = token
            .as_ref()
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        let t = t
            .strip_prefix("Bearer ")
            .or_else(|| t.strip_prefix("bearer "))
            .unwrap_or(t)
            .trim();
        (!t.is_empty()).then(|| Self {
            token: t.to_owned(),
        })
    }

    /// `HARDCOVER_TOKEN`, if set.
    pub fn from_env() -> Option<Self> {
        std::env::var(Self::ENV).ok().and_then(Self::new)
    }
}

impl fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("StaticToken(<redacted>)")
    }
}

impl TokenSource for StaticToken {
    fn access_token(&self) -> Result<String> {
        Ok(self.token.clone())
    }

    fn on_unauthorized(&self, _rejected: &str) -> Result<bool> {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::MemoryTokenStore;
    use crate::http::fake::FakeTransport;

    fn soon(minutes: i64) -> Option<crate::api::Timestamp> {
        Some(chrono::Utc::now() + chrono::Duration::minutes(minutes))
    }

    fn session(tokens: Tokens) -> (Session, Arc<FakeTransport>, Arc<MemoryTokenStore>) {
        let http = Arc::new(FakeTransport::new());
        let store = Arc::new(MemoryTokenStore::new());
        let s = Session::start_with(OAuthConfig::new("cid"), store.clone(), tokens, http.clone())
            .unwrap();
        (s, http, store)
    }

    const ROTATED: &str = r#"{"access_token":"at2","refresh_token":"rt2","expires_in":604800}"#;

    #[test]
    fn fresh_token_is_used_as_is() {
        let (s, http, _) = session(Tokens::new("at1", Some("rt1".into()), soon(60 * 24)));
        assert_eq!(s.access_token().unwrap(), "at1");
        assert!(http.requests().is_empty());
    }

    #[test]
    fn refreshes_near_expiry_and_persists_rotation() {
        let (s, http, store) = session(Tokens::new("at1", Some("rt1".into()), soon(2)));
        http.push(200, ROTATED);
        assert_eq!(s.access_token().unwrap(), "at2");
        let req = &http.requests()[0];
        assert_eq!(req.form_value("grant_type"), Some("refresh_token"));
        assert_eq!(req.form_value("refresh_token"), Some("rt1"));
        assert_eq!(req.form_value("client_id"), Some("cid"));
        let saved = store.load().unwrap().unwrap();
        assert_eq!(
            (saved.access_token(), saved.refresh_token()),
            ("at2", Some("rt2"))
        );
        // Next call uses the new token without another refresh.
        assert_eq!(s.access_token().unwrap(), "at2");
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn proactive_refresh_network_error_falls_back_while_valid() {
        let (s, http, _) = session(Tokens::new("at1", Some("rt1".into()), soon(2)));
        http.push_err(Error::Network("down".into()));
        assert_eq!(s.access_token().unwrap(), "at1");
        let (s, http, _) = session(Tokens::new("at1", Some("rt1".into()), soon(-1)));
        http.push_err(Error::Network("down".into()));
        assert!(matches!(s.access_token(), Err(Error::Network(_))));
    }

    #[test]
    fn unauthorized_refreshes_once_per_token() {
        let (s, http, _) = session(Tokens::new("at1", Some("rt1".into()), soon(600)));
        http.push(200, ROTATED);
        assert!(s.on_unauthorized("at1").unwrap());
        // A second 401 for the old token (a concurrent request) does not refresh again.
        assert!(s.on_unauthorized("at1").unwrap());
        assert_eq!(http.requests().len(), 1);
        assert_eq!(s.access_token().unwrap(), "at2");
    }

    #[test]
    fn rejected_refresh_signs_out() {
        let (s, http, store) = session(Tokens::new("at1", Some("rt1".into()), soon(600)));
        http.push(400, r#"{"error":"invalid_grant"}"#);
        assert!(!s.on_unauthorized("at1").unwrap());
        assert!(!s.is_signed_in());
        assert!(store.load().unwrap().is_none());
        assert_eq!(s.access_token(), Err(Error::Unauthorized));
    }

    #[test]
    fn concurrent_callers_refresh_once() {
        let (s, http, _) = session(Tokens::new("at1", Some("rt1".into()), soon(1)));
        http.push(200, ROTATED);
        let s = Arc::new(s);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || s.access_token().unwrap())
            })
            .collect();
        for h in handles {
            assert_eq!(h.join().unwrap(), "at2");
        }
        assert_eq!(http.requests().len(), 1);
    }

    #[test]
    fn sign_out_revokes_and_clears() {
        let (s, http, store) = session(Tokens::new("at1", Some("rt1".into()), soon(600)));
        http.push(200, "");
        s.sign_out().unwrap();
        assert!(!s.is_signed_in());
        assert!(store.load().unwrap().is_none());
        assert_eq!(http.requests()[0].form_value("token"), Some("rt1"));
        // Revoke failure still signs out locally.
        let (s, http, store) = session(Tokens::new("at1", Some("rt1".into()), soon(600)));
        http.push(503, "");
        assert!(s.sign_out().is_err());
        assert!(!s.is_signed_in());
        assert!(store.load().unwrap().is_none());
    }

    #[test]
    fn restore_reads_store() {
        let store = Arc::new(MemoryTokenStore::new());
        let http = Arc::new(FakeTransport::new());
        assert!(
            Session::restore_with(OAuthConfig::new("c"), store.clone(), http.clone())
                .unwrap()
                .is_none()
        );
        store.save(&Tokens::new("at", None, None)).unwrap();
        let s = Session::restore_with(OAuthConfig::new("c"), store, http)
            .unwrap()
            .unwrap();
        assert_eq!(s.access_token().unwrap(), "at");
        assert!(!s.on_unauthorized("at").unwrap());
    }

    #[test]
    fn static_token_normalizes_and_redacts() {
        assert_eq!(
            StaticToken::new(" \"hc_pat_x\" \n")
                .unwrap()
                .access_token()
                .unwrap(),
            "hc_pat_x"
        );
        assert_eq!(
            StaticToken::new("Bearer abc")
                .unwrap()
                .access_token()
                .unwrap(),
            "abc"
        );
        assert!(StaticToken::new("  ").is_none());
        let t = StaticToken::new("secret-value").unwrap();
        assert!(!format!("{t:?}").contains("secret"));
        assert!(!t.on_unauthorized("secret-value").unwrap());
    }
}
