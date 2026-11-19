//! Token endpoint calls: code exchange, refresh, revoke, discovery.

use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::{OAuthConfig, OAuthEndpoints, Tokens};
use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method};
use crate::{Error, Result};

const TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<Value>,
    scope: Option<String>,
}

/// Why a token request failed.
pub(crate) enum TokenError {
    /// The server rejected the grant (`invalid_grant`, `invalid_client`, ...): the
    /// code or refresh token is dead and retrying cannot help.
    Rejected(String),
    /// Network trouble or a 5xx: the grant may or may not have been used.
    Transient(Error),
}

pub(crate) fn exchange_code(
    http: &dyn HttpTransport,
    cfg: &OAuthConfig,
    code: &str,
    redirect_uri: &str,
    verifier: &str,
) -> std::result::Result<Tokens, TokenError> {
    token_request(
        http,
        cfg,
        &[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", verifier),
            ("client_id", cfg.client_id()),
        ],
    )
}

/// One attempt only: refresh tokens rotate, and replaying one revokes the chain.
pub(crate) fn refresh(
    http: &dyn HttpTransport,
    cfg: &OAuthConfig,
    refresh_token: &str,
) -> std::result::Result<Tokens, TokenError> {
    token_request(
        http,
        cfg,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", cfg.client_id()),
        ],
    )
}

fn token_request(
    http: &dyn HttpTransport,
    cfg: &OAuthConfig,
    form: &[(&str, &str)],
) -> std::result::Result<Tokens, TokenError> {
    let req = HttpRequest::new(Method::Post, &cfg.endpoints().token)
        .header("accept", "application/json")
        .form(form)
        .timeout(TIMEOUT);
    let resp = http.send(req).map_err(TokenError::Transient)?;
    parse_token_response(&resp, chrono::Utc::now())
}

pub(crate) fn parse_token_response(
    resp: &HttpResponse,
    now: chrono::DateTime<chrono::Utc>,
) -> std::result::Result<Tokens, TokenError> {
    if (200..300).contains(&resp.status) {
        let t: TokenResponse = serde_json::from_slice(&resp.body)
            .map_err(|e| TokenError::Transient(Error::Decode(format!("token response: {e}"))))?;
        let expires_in = t.expires_in.as_ref().and_then(crate::api::wire_i64);
        let expires_at = expires_in.map(|s| now + chrono::Duration::seconds(s));
        return Ok(Tokens::new(t.access_token, t.refresh_token, expires_at).with_scope(t.scope));
    }
    let body: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    let field = |k: &str| body.get(k).and_then(Value::as_str).map(str::to_owned);
    let code = field("error");
    let desc = field("error_description");
    let msg = match (&code, &desc) {
        (Some(c), Some(d)) => format!("{c}: {d}"),
        (Some(c), None) => c.clone(),
        (None, Some(d)) => d.clone(),
        (None, None) => format!("HTTP {}", resp.status),
    };
    if resp.status >= 500 || resp.status == 429 {
        Err(TokenError::Transient(Error::Network(format!(
            "token endpoint: {msg}"
        ))))
    } else {
        Err(TokenError::Rejected(msg))
    }
}

/// RFC 7009 revocation. Revoking either token revokes both.
pub(crate) fn revoke(http: &dyn HttpTransport, cfg: &OAuthConfig, tokens: &Tokens) -> Result<()> {
    let (token, hint) = match tokens.refresh_token() {
        Some(rt) => (rt, "refresh_token"),
        None => (tokens.access_token(), "access_token"),
    };
    let req = HttpRequest::new(Method::Post, &cfg.endpoints().revoke)
        .form(&[
            ("token", token),
            ("token_type_hint", hint),
            ("client_id", cfg.client_id()),
        ])
        .timeout(TIMEOUT);
    let resp = http.send(req)?;
    if (200..300).contains(&resp.status) {
        Ok(())
    } else {
        Err(Error::Auth(format!(
            "revoking the token failed with HTTP {}",
            resp.status
        )))
    }
}

pub(crate) fn discover(http: &dyn HttpTransport, expected_issuer: &str) -> Result<OAuthEndpoints> {
    let url = format!("{expected_issuer}/.well-known/oauth-authorization-server");
    let resp = http.send(HttpRequest::new(Method::Get, url).timeout(TIMEOUT))?;
    if resp.status != 200 {
        return Err(Error::Auth(format!(
            "discovery failed with HTTP {}",
            resp.status
        )));
    }
    let v: Value = serde_json::from_slice(&resp.body).map_err(|e| Error::Decode(e.to_string()))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| Error::Decode(format!("discovery document has no `{k}`")))
    };
    let issuer = s("issuer")?;
    if issuer != expected_issuer {
        return Err(Error::Auth(format!(
            "discovery returned unexpected issuer {issuer}"
        )));
    }
    Ok(OAuthEndpoints {
        issuer,
        authorize: s("authorization_endpoint")?,
        token: s("token_endpoint")?,
        revoke: s("revocation_endpoint")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }

    #[test]
    fn parses_success() {
        let now = chrono::Utc::now();
        let t = parse_token_response(
            &resp(200, r#"{"access_token":"hc_at_x","refresh_token":"hc_rt_y","expires_in":604800,"token_type":"Bearer","scope":"read:me"}"#),
            now,
        )
        .ok()
        .unwrap();
        assert_eq!(t.access_token(), "hc_at_x");
        assert_eq!(t.refresh_token(), Some("hc_rt_y"));
        assert_eq!(
            t.expires_at(),
            Some(now + chrono::Duration::seconds(604_800))
        );
        assert_eq!(t.scope(), Some("read:me"));
    }

    #[test]
    fn classifies_failures() {
        let now = chrono::Utc::now();
        assert!(matches!(
            parse_token_response(&resp(400, r#"{"error":"invalid_grant","error_description":"used"}"#), now),
            Err(TokenError::Rejected(m)) if m == "invalid_grant: used"
        ));
        assert!(matches!(
            parse_token_response(&resp(503, "oops"), now),
            Err(TokenError::Transient(_))
        ));
        assert!(matches!(
            parse_token_response(&resp(200, "not json"), now),
            Err(TokenError::Transient(_))
        ));
    }

    #[test]
    fn revoke_prefers_refresh_token() {
        let http = FakeTransport::new();
        http.push(200, "");
        let cfg = OAuthConfig::new("cid");
        revoke(&http, &cfg, &Tokens::new("at", Some("rt".into()), None)).unwrap();
        let r = &http.requests()[0];
        assert_eq!(r.url, "https://api.hardcover.app/oauth2/revoke");
        assert_eq!(r.form_value("token"), Some("rt"));
        assert_eq!(r.form_value("token_type_hint"), Some("refresh_token"));
        assert_eq!(r.form_value("client_id"), Some("cid"));
    }

    #[test]
    fn discovery_checks_issuer() {
        let http = FakeTransport::new();
        http.push(200, r#"{"issuer":"https://api.hardcover.app","authorization_endpoint":"https://hardcover.app/oauth2/authorize","token_endpoint":"https://api.hardcover.app/oauth2/token","revocation_endpoint":"https://api.hardcover.app/oauth2/revoke"}"#);
        assert_eq!(
            discover(&http, "https://api.hardcover.app").unwrap(),
            OAuthEndpoints::default()
        );
        http.push(200, r#"{"issuer":"https://evil.example","authorization_endpoint":"a","token_endpoint":"b","revocation_endpoint":"c"}"#);
        assert!(matches!(
            discover(&http, "https://api.hardcover.app"),
            Err(Error::Auth(_))
        ));
    }
}
