//! GPUI-free core of hardcover-desktop.
//!
//! Everything here is synchronous and blocking: call it from a background
//! thread pool, never from the UI thread. Nothing depends on an async runtime.
//!
//! - [`api`]: GraphQL client for <https://api.hardcover.app/v1/graphql> plus the data models.
//! - [`auth`]: OAuth (authorization code + PKCE over a loopback redirect), token storage and refresh.
//! - [`store`]: SQLite cache of the signed-in user's library.
//! - [`covers`]: on-disk cache of cover images.
//! - [`update`]: self-update from signed GitHub releases.

pub mod api;
pub mod auth;
pub mod covers;
pub mod error;
pub mod store;
pub mod update;

mod http;
mod ratelimit;

pub use api::{
    BookDetails, BookId, BookSummary, Client, Cover, Date, EditionId, EditionSummary, LibraryEntry,
    Privacy, ProgressUpdate, Rating, ReadId, ReadSession, ReadingStatus, SearchPage, SeriesRef,
    Timestamp, TokenSource, User, UserBookId, UserId,
};
pub use error::{Error, Result};

/// `User-Agent` sent with every request (API, OAuth and cover downloads).
pub const USER_AGENT: &str = concat!(
    "hardcover-desktop/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/AliSayyah/hardcover-desktop)"
);

/// Directory name used under the XDG data and cache directories.
pub const APP_DIR_NAME: &str = "hardcover-desktop";

pub(crate) fn project_dirs() -> Result<directories::ProjectDirs> {
    directories::ProjectDirs::from("", "", APP_DIR_NAME)
        .ok_or_else(|| Error::Storage("could not determine the home directory".into()))
}
