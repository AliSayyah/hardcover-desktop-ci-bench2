//! Signs in with OAuth from the command line and stores the session in the
//! system keyring, where the desktop app restores it on launch.
//!
//! Useful when the browser that approves the sign-in is on another device:
//! approve there, then deliver the redirect URL the browser failed to load to
//! the printed loopback address on this machine (for example with `curl`).
//!
//! ```sh
//! cargo run -p hardcover-core --example oauth_login
//! ```

use std::{sync::Arc, time::Duration};

use hardcover_core::auth::{KeyringTokenStore, OAuthConfig, Session, begin_sign_in};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = OAuthConfig::default();
    let pending = begin_sign_in(&cfg)?;
    println!(
        "Open this URL and approve access:\n{}\n",
        pending.authorize_url()
    );
    println!("Listening for the redirect on {}", pending.redirect_uri());
    let tokens = pending.wait(Duration::from_secs(30 * 60))?;
    let session = Session::start(cfg, Arc::new(KeyringTokenStore::new()?), tokens)?;
    println!(
        "Signed in. Session stored in the keyring (scope: {:?}).",
        session.scope()
    );
    Ok(())
}
