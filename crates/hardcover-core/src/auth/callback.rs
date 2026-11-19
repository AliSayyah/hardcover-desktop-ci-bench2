//! Loopback redirect listener for the authorization code flow (RFC 8252).

use std::fmt;
use std::io::{ErrorKind, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use url::Url;

use super::oauth::{self, TokenError};
use super::{OAuthConfig, Tokens, pkce};
use crate::http::{HttpTransport, UreqTransport};
use crate::{Error, Result};

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_REQUEST_HEAD: usize = 16 * 1024;

/// Starts a sign-in: binds `127.0.0.1:<random port>`, makes a PKCE verifier and a
/// random `state`, and builds the authorize URL. Nothing is sent yet.
pub fn begin_sign_in(cfg: &OAuthConfig) -> Result<PendingSignIn> {
    begin_sign_in_with(cfg, Arc::new(UreqTransport::new()))
}

pub(crate) fn begin_sign_in_with(
    cfg: &OAuthConfig,
    http: Arc<dyn HttpTransport>,
) -> Result<PendingSignIn> {
    if !cfg.redirect_path().starts_with('/') {
        return Err(Error::InvalidInput(
            "redirect path must start with '/'".into(),
        ));
    }
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .map_err(|e| Error::Auth(format!("could not listen on 127.0.0.1: {e}")))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| Error::Auth(format!("could not configure the listener: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| Error::Auth(e.to_string()))?
        .port();
    // Literal 127.0.0.1 (not "localhost"), matching the registered URI; the same
    // string, port included, is sent again at the token exchange.
    let redirect_uri = format!("http://127.0.0.1:{port}{}", cfg.redirect_path());
    let state = pkce::random_urlsafe(32)?;
    let verifier = pkce::new_verifier()?;
    let mut url = Url::parse(&cfg.endpoints().authorize)
        .map_err(|e| Error::InvalidInput(format!("authorize endpoint: {e}")))?;
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", cfg.client_id())
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("scope", &cfg.scopes().join(" "))
        .append_pair("state", &state)
        .append_pair("code_challenge", &pkce::challenge_s256(&verifier))
        .append_pair("code_challenge_method", "S256");
    Ok(PendingSignIn {
        listener,
        redirect_uri,
        authorize_url: url.into(),
        state,
        verifier,
        cfg: cfg.clone(),
        cancel: Arc::new(AtomicBool::new(false)),
        http,
    })
}

/// A sign-in waiting for the browser to come back.
pub struct PendingSignIn {
    listener: TcpListener,
    redirect_uri: String,
    authorize_url: String,
    state: String,
    verifier: String,
    cfg: OAuthConfig,
    cancel: Arc<AtomicBool>,
    http: Arc<dyn HttpTransport>,
}

impl fmt::Debug for PendingSignIn {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingSignIn")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// Cancels a [`PendingSignIn::wait`] from another thread. Cheap to clone.
#[derive(Debug, Clone)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

impl PendingSignIn {
    /// Open this in the browser, and also show it so the user can copy it if
    /// opening fails. Contains `state` and the PKCE challenge, never the verifier.
    pub fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle(self.cancel.clone())
    }

    #[cfg(test)]
    pub(crate) fn state(&self) -> &str {
        &self.state
    }

    /// Blocks until the browser hits the redirect URI, then exchanges the code.
    ///
    /// Errors: [`Error::Cancelled`] (cancel handle), [`Error::Timeout`],
    /// [`Error::Auth`] (state or issuer mismatch, the user declined, or the code
    /// exchange failed). Requests to other paths (e.g. `/favicon.ico`) are
    /// answered with 404 and ignored.
    pub fn wait(self, timeout: Duration) -> Result<Tokens> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.cancel.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout);
            }
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if let Some(result) = self.handle_connection(stream) {
                        return result;
                    }
                }
                Err(e)
                    if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::Interrupted =>
                {
                    std::thread::sleep(POLL_INTERVAL);
                }
                Err(e) => return Err(Error::Auth(format!("loopback listener failed: {e}"))),
            }
        }
    }

    /// `None` if the connection was not the decisive callback.
    fn handle_connection(&self, mut stream: TcpStream) -> Option<Result<Tokens>> {
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(CONNECTION_TIMEOUT));
        let _ = stream.set_write_timeout(Some(CONNECTION_TIMEOUT));
        let head = read_request_head(&mut stream)?;
        let Some((method, target)) = parse_request_line(&head) else {
            respond(
                &mut stream,
                400,
                "Bad request",
                "This address only handles the Hardcover sign-in.",
            );
            return None;
        };
        let Ok(url) = Url::parse("http://127.0.0.1").and_then(|base| base.join(target)) else {
            respond(
                &mut stream,
                400,
                "Bad request",
                "This address only handles the Hardcover sign-in.",
            );
            return None;
        };
        if url.path() != self.cfg.redirect_path() {
            respond(
                &mut stream,
                404,
                "Not found",
                "This address only handles the Hardcover sign-in.",
            );
            return None;
        }
        if method != "GET" {
            respond(
                &mut stream,
                405,
                "Method not allowed",
                "Expected a GET request.",
            );
            return None;
        }
        let params: Vec<(String, String)> = url.query_pairs().into_owned().collect();
        let outcome = match evaluate_callback(&params, &self.state, &self.cfg.endpoints().issuer) {
            Ok(code) => oauth::exchange_code(
                &*self.http,
                &self.cfg,
                &code,
                &self.redirect_uri,
                &self.verifier,
            )
            .map_err(|e| match e {
                TokenError::Rejected(m) => Error::Auth(format!("code exchange rejected ({m})")),
                TokenError::Transient(e) => e,
            }),
            Err(e) => Err(e),
        };
        match &outcome {
            Ok(_) => respond(
                &mut stream,
                200,
                "Signed in to Hardcover",
                "You can close this tab and return to Hardcover Desktop.",
            ),
            Err(e) => respond(
                &mut stream,
                400,
                "Sign-in failed",
                &format!("{e}. Close this tab and try again from Hardcover Desktop."),
            ),
        }
        Some(outcome)
    }
}

/// Checks the redirect's query: `state` first (CSRF), then `error`, then `iss`
/// (mix-up attacks, RFC 9207) and `code`. Returns the code.
pub(crate) fn evaluate_callback(
    params: &[(String, String)],
    expected_state: &str,
    issuer: &str,
) -> Result<String> {
    let get = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    if !get("state").is_some_and(|s| constant_time_eq(s.as_bytes(), expected_state.as_bytes())) {
        return Err(Error::Auth(
            "the response did not match this sign-in attempt (state mismatch)".into(),
        ));
    }
    if let Some(iss) = get("iss")
        && iss != issuer
    {
        return Err(Error::Auth(format!("unexpected issuer {iss}")));
    }
    if let Some(err) = get("error") {
        return Err(match err {
            "access_denied" => Error::Auth("access was denied".into()),
            other => {
                let desc = get("error_description")
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default();
                Error::Auth(format!("{other}{desc}"))
            }
        });
    }
    if get("iss").is_none() {
        // Hardcover advertises authorization_response_iss_parameter_supported.
        return Err(Error::Auth("the response did not name its issuer".into()));
    }
    get("code")
        .filter(|c| !c.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| Error::Auth("the response had no authorization code".into()))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Reads up to the end of the request head. `None` for empty/slow/oversized requests.
fn read_request_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        if buf.len() > MAX_REQUEST_HEAD {
            return None;
        }
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(_) => return None,
        }
    }
    if buf.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

fn parse_request_line(head: &str) -> Option<(&str, &str)> {
    let line = head.lines().next()?;
    let mut parts = line.split_whitespace();
    let method = parts.next()?;
    let target = parts.next()?;
    parts
        .next()?
        .starts_with("HTTP/")
        .then_some((method, target))
}

fn respond(stream: &mut TcpStream, status: u16, title: &str, message: &str) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Bad Request",
    };
    let body = format!(
        "<!doctype html><html><head><meta charset=\"utf-8\"><title>{t}</title>\
<style>body{{font-family:system-ui,sans-serif;max-width:32rem;margin:4rem auto;padding:0 1rem;color:#222}}</style>\
</head><body><h1>{t}</h1><p>{m}</p></body></html>",
        t = html_escape(title),
        m = html_escape(message)
    );
    let response = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
    let _ = stream.flush();
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::fake::FakeTransport;

    const ISS: &str = "https://api.hardcover.app";

    fn p(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).into(), (*v).into()))
            .collect()
    }

    #[test]
    fn callback_validation() {
        assert_eq!(
            evaluate_callback(&p(&[("code", "c"), ("state", "s"), ("iss", ISS)]), "s", ISS)
                .unwrap(),
            "c"
        );
        // state is checked before anything else, including errors
        assert!(matches!(
            evaluate_callback(&p(&[("error", "access_denied"), ("state", "other")]), "s", ISS),
            Err(Error::Auth(m)) if m.contains("state")
        ));
        assert!(evaluate_callback(&p(&[("code", "c"), ("iss", ISS)]), "s", ISS).is_err());
        assert!(matches!(
            evaluate_callback(&p(&[("code", "c"), ("state", "s"), ("iss", "https://evil.example")]), "s", ISS),
            Err(Error::Auth(m)) if m.contains("issuer")
        ));
        assert!(evaluate_callback(&p(&[("code", "c"), ("state", "s")]), "s", ISS).is_err());
        assert_eq!(
            evaluate_callback(
                &p(&[("error", "access_denied"), ("state", "s"), ("iss", ISS)]),
                "s",
                ISS
            ),
            Err(Error::Auth("access was denied".into()))
        );
        assert!(evaluate_callback(&p(&[("state", "s"), ("iss", ISS)]), "s", ISS).is_err());
    }

    #[test]
    fn authorize_url_has_pkce_and_loopback_redirect() {
        let cfg = OAuthConfig::new("client-123");
        let pending = begin_sign_in_with(&cfg, Arc::new(FakeTransport::new())).unwrap();
        let url = Url::parse(pending.authorize_url()).unwrap();
        assert_eq!(url.host_str(), Some("hardcover.app"));
        assert_eq!(url.path(), "/oauth2/authorize");
        let q: std::collections::HashMap<_, _> = url.query_pairs().into_owned().collect();
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], "client-123");
        assert_eq!(
            q["scope"],
            "read:me read:catalog read:library write:library"
        );
        assert_eq!(q["code_challenge_method"], "S256");
        assert_eq!(q["code_challenge"], pkce::challenge_s256(&pending.verifier));
        assert_eq!(q["state"], pending.state());
        assert_eq!(q["redirect_uri"], pending.redirect_uri());
        assert!(pending.redirect_uri().starts_with("http://127.0.0.1:"));
        assert!(pending.redirect_uri().ends_with("/callback"));
        assert!(!pending.authorize_url().contains(&pending.verifier));
        assert!(!format!("{pending:?}").contains(&pending.verifier));
    }

    fn send(addr: &str, request: &str) -> String {
        let mut s = TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        s.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn start(
        http: Arc<FakeTransport>,
    ) -> (
        String,
        String,
        String,
        std::thread::JoinHandle<Result<Tokens>>,
        CancelHandle,
    ) {
        let cfg = OAuthConfig::new("client-123");
        let pending = begin_sign_in_with(&cfg, http).unwrap();
        let addr = pending
            .redirect_uri()
            .trim_start_matches("http://")
            .trim_end_matches("/callback")
            .to_owned();
        let state = pending.state().to_owned();
        let redirect = pending.redirect_uri().to_owned();
        let cancel = pending.cancel_handle();
        let handle = std::thread::spawn(move || pending.wait(Duration::from_secs(10)));
        (addr, state, redirect, handle, cancel)
    }

    #[test]
    fn full_callback_exchanges_code() {
        let http = Arc::new(FakeTransport::new());
        http.push(200, r#"{"access_token":"hc_at_new","refresh_token":"hc_rt_new","expires_in":3600,"token_type":"Bearer"}"#);
        let (addr, state, redirect, handle, _) = start(http.clone());

        // Unrelated requests and empty connections are ignored.
        let r = send(&addr, "GET /favicon.ico HTTP/1.1\r\nHost: x\r\n\r\n");
        assert!(r.starts_with("HTTP/1.1 404"));
        drop(TcpStream::connect(&addr).unwrap());

        let iss = "https%3A%2F%2Fapi.hardcover.app";
        let r = send(
            &addr,
            &format!(
                "GET /callback?code=the-code&state={state}&iss={iss} HTTP/1.1\r\nHost: x\r\n\r\n"
            ),
        );
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("You can close this tab"));

        let tokens = handle.join().unwrap().unwrap();
        assert_eq!(tokens.access_token(), "hc_at_new");
        assert_eq!(tokens.refresh_token(), Some("hc_rt_new"));
        let req = &http.requests()[0];
        assert_eq!(req.url, "https://api.hardcover.app/oauth2/token");
        assert_eq!(req.form_value("grant_type"), Some("authorization_code"));
        assert_eq!(req.form_value("code"), Some("the-code"));
        assert_eq!(req.form_value("client_id"), Some("client-123"));
        assert_eq!(req.form_value("redirect_uri"), Some(redirect.as_str()));
        assert_eq!(req.form_value("code_verifier").map(str::len), Some(43));
        assert!(req.form_value("client_secret").is_none());
    }

    #[test]
    fn denied_callback_fails_without_exchange() {
        let http = Arc::new(FakeTransport::new());
        let (addr, state, _, handle, _) = start(http.clone());
        let r = send(
            &addr,
            &format!(
                "GET /callback?error=access_denied&state={state}&iss=https%3A%2F%2Fapi.hardcover.app HTTP/1.1\r\n\r\n"
            ),
        );
        assert!(r.starts_with("HTTP/1.1 400"));
        assert_eq!(
            handle.join().unwrap(),
            Err(Error::Auth("access was denied".into()))
        );
        assert!(http.requests().is_empty());
    }

    #[test]
    fn wrong_state_aborts() {
        let http = Arc::new(FakeTransport::new());
        let (addr, _, _, handle, _) = start(http.clone());
        send(
            &addr,
            "GET /callback?code=x&state=forged&iss=https%3A%2F%2Fapi.hardcover.app HTTP/1.1\r\n\r\n",
        );
        assert!(matches!(handle.join().unwrap(), Err(Error::Auth(m)) if m.contains("state")));
        assert!(http.requests().is_empty());
    }

    #[test]
    fn rejected_exchange_is_reported() {
        let http = Arc::new(FakeTransport::new());
        http.push(
            400,
            r#"{"error":"invalid_grant","error_description":"code expired"}"#,
        );
        let (addr, state, _, handle, _) = start(http);
        let r = send(
            &addr,
            &format!(
                "GET /callback?code=c&state={state}&iss=https%3A%2F%2Fapi.hardcover.app HTTP/1.1\r\n\r\n"
            ),
        );
        assert!(r.contains("Sign-in failed"));
        assert!(
            matches!(handle.join().unwrap(), Err(Error::Auth(m)) if m.contains("code expired"))
        );
    }

    #[test]
    fn cancel_and_timeout() {
        let (_, _, _, handle, cancel) = start(Arc::new(FakeTransport::new()));
        cancel.cancel();
        assert_eq!(handle.join().unwrap(), Err(Error::Cancelled));

        let pending =
            begin_sign_in_with(&OAuthConfig::new("c"), Arc::new(FakeTransport::new())).unwrap();
        assert_eq!(
            pending.wait(Duration::from_millis(100)),
            Err(Error::Timeout)
        );
    }
}
