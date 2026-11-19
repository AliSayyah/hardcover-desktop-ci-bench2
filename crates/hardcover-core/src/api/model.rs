//! Plain data records returned by [`crate::api::Client`] and stored by [`crate::store`].
//!
//! Records have public fields but are `#[non_exhaustive]`: build them with their
//! `new` constructors and then set the optional fields you need.

use std::fmt;

use serde::{Deserialize, Serialize};

/// Calendar date (`date` in the schema), e.g. a read's start date.
pub type Date = chrono::NaiveDate;
/// Point in time (`timestamptz` in the schema), always UTC.
pub type Timestamp = chrono::DateTime<chrono::Utc>;

macro_rules! id_type {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(i64);

        impl $name {
            pub const fn new(id: i64) -> Self {
                Self(id)
            }
            pub const fn get(self) -> i64 {
                self.0
            }
        }

        impl From<i64> for $name {
            fn from(id: i64) -> Self {
                Self(id)
            }
        }

        impl From<$name> for i64 {
            fn from(id: $name) -> i64 {
                id.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

id_type!(
    /// `books.id`
    BookId
);
id_type!(
    /// `user_books.id`: one book in one user's library.
    UserBookId
);
id_type!(
    /// `user_book_reads.id`: one read-through of a library entry.
    ReadId
);
id_type!(
    /// `editions.id`
    EditionId
);
id_type!(
    /// `users.id`
    UserId
);

/// A library entry's status (`user_books.status_id`).
///
/// Ids verified against the live `user_book_statuses` table (2026-11):
/// 1 want-to-read, 2 currently-reading, 3 read, 4 paused, 5 did-not-finish, 6 ignored.
///
/// [`Ignored`](Self::Ignored) is a real status (a book the user hid from
/// recommendations). [`crate::api::Client::library`] leaves those entries out, but
/// single-entry lookups return them so the UI never mistakes an ignored book for
/// one that is not in the library. Rows with an id this enum does not know are skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ReadingStatus {
    WantToRead,
    CurrentlyReading,
    Read,
    Paused,
    DidNotFinish,
    Ignored,
}

impl ReadingStatus {
    /// The five statuses that are shelves in the library, in Hardcover's order.
    pub const SHELVES: [ReadingStatus; 5] = [
        Self::WantToRead,
        Self::CurrentlyReading,
        Self::Read,
        Self::Paused,
        Self::DidNotFinish,
    ];

    /// `status_id` used by the API.
    pub const fn id(self) -> i64 {
        match self {
            Self::WantToRead => 1,
            Self::CurrentlyReading => 2,
            Self::Read => 3,
            Self::Paused => 4,
            Self::DidNotFinish => 5,
            Self::Ignored => 6,
        }
    }

    pub const fn from_id(id: i64) -> Option<Self> {
        Some(match id {
            1 => Self::WantToRead,
            2 => Self::CurrentlyReading,
            3 => Self::Read,
            4 => Self::Paused,
            5 => Self::DidNotFinish,
            6 => Self::Ignored,
            _ => return None,
        })
    }

    /// Hardcover's English label (`user_book_statuses.status`).
    pub const fn label(self) -> &'static str {
        match self {
            Self::WantToRead => "Want to Read",
            Self::CurrentlyReading => "Currently Reading",
            Self::Read => "Read",
            Self::Paused => "Paused",
            Self::DidNotFinish => "Did Not Finish",
            Self::Ignored => "Ignored",
        }
    }

    /// Hardcover's slug (`user_book_statuses.slug`).
    pub const fn slug(self) -> &'static str {
        match self {
            Self::WantToRead => "want-to-read",
            Self::CurrentlyReading => "currently-reading",
            Self::Read => "read",
            Self::Paused => "paused",
            Self::DidNotFinish => "did-not-finish",
            Self::Ignored => "ignored",
        }
    }

    pub const fn is_shelf(self) -> bool {
        !matches!(self, Self::Ignored)
    }
}

impl fmt::Display for ReadingStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.pad(self.label())
    }
}

/// Who can see a library entry and the activity it generates
/// (`privacy_settings`, ids verified live: 1 Public, 2 Followers only, 3 Private).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Privacy {
    Public,
    FollowersOnly,
    Private,
}

impl Privacy {
    pub const fn id(self) -> i64 {
        match self {
            Self::Public => 1,
            Self::FollowersOnly => 2,
            Self::Private => 3,
        }
    }

    pub const fn from_id(id: i64) -> Option<Self> {
        Some(match id {
            1 => Self::Public,
            2 => Self::FollowersOnly,
            3 => Self::Private,
            _ => return None,
        })
    }
}

/// A star rating from 0.5 to 5.0 in half steps.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f32", into = "f32")]
pub struct Rating(f32);

impl Rating {
    pub const MIN: f32 = 0.5;
    pub const MAX: f32 = 5.0;

    /// `None` unless `value` is within 0.5..=5.0 and a multiple of 0.5.
    pub fn new(value: f32) -> Option<Self> {
        let doubled = value * 2.0;
        ((Self::MIN..=Self::MAX).contains(&value) && doubled == doubled.round())
            .then_some(Self(value))
    }

    /// Lenient conversion for values coming from the API: rounds to the nearest half
    /// star and treats anything below a quarter star as "no rating".
    pub fn from_api(value: f64) -> Option<Self> {
        if !value.is_finite() || value < 0.25 {
            return None;
        }
        let rounded =
            ((value * 2.0).round() / 2.0).clamp(f64::from(Self::MIN), f64::from(Self::MAX));
        Some(Self(rounded as f32))
    }

    pub fn value(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for Rating {
    type Error = String;
    fn try_from(v: f32) -> Result<Self, Self::Error> {
        Self::new(v).ok_or_else(|| format!("invalid rating {v}: expected 0.5..=5.0 in 0.5 steps"))
    }
}

impl From<Rating> for f32 {
    fn from(r: Rating) -> f32 {
        r.0
    }
}

impl fmt::Display for Rating {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.fract() == 0.0 {
            write!(f, "{:.0}", self.0)
        } else {
            write!(f, "{:.1}", self.0)
        }
    }
}

/// An image hosted by Hardcover (book cover, edition cover or avatar).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Cover {
    pub url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

impl Cover {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            width: None,
            height: None,
        }
    }
}

/// The signed-in user (`me`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct User {
    pub id: UserId,
    pub username: String,
    pub name: Option<String>,
    pub avatar: Option<Cover>,
}

impl User {
    pub fn new(id: UserId, username: impl Into<String>) -> Self {
        Self {
            id,
            username: username.into(),
            name: None,
            avatar: None,
        }
    }
}

/// What lists and search results show for a book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BookSummary {
    pub id: BookId,
    pub title: String,
    pub subtitle: Option<String>,
    /// Primary authors (contributions with no role or the "Author" role), in credit order.
    pub authors: Vec<String>,
    pub cover: Option<Cover>,
    /// Page count of the default physical edition.
    pub pages: Option<u32>,
    pub release_year: Option<i32>,
    /// URL slug: `https://hardcover.app/books/<slug>`.
    pub slug: Option<String>,
    /// Hardcover average rating (0..=5).
    pub average_rating: Option<f32>,
    /// Number of users who have the book in their library.
    pub users_count: Option<u32>,
}

impl BookSummary {
    pub fn new(id: BookId, title: impl Into<String>) -> Self {
        Self {
            id,
            title: title.into(),
            subtitle: None,
            authors: Vec::new(),
            cover: None,
            pages: None,
            release_year: None,
            slug: None,
            average_rating: None,
            users_count: None,
        }
    }

    /// `https://hardcover.app/books/<slug>`, when the book has a slug.
    pub fn web_url(&self) -> Option<String> {
        self.slug
            .as_deref()
            .map(|s| format!("https://hardcover.app/books/{s}"))
    }
}

/// The edition a library entry is tied to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EditionSummary {
    pub id: EditionId,
    pub title: Option<String>,
    pub pages: Option<u32>,
    /// Length of an audiobook edition.
    pub audio_seconds: Option<u32>,
    pub cover: Option<Cover>,
    /// `editions.edition_format`, free text such as "Paperback", "Hardcover", "Kindle Edition".
    pub format: Option<String>,
}

impl EditionSummary {
    pub fn new(id: EditionId) -> Self {
        Self {
            id,
            title: None,
            pages: None,
            audio_seconds: None,
            cover: None,
            format: None,
        }
    }
}

/// One read-through of a book (`user_book_reads`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReadSession {
    pub id: ReadId,
    pub started_at: Option<Date>,
    pub finished_at: Option<Date>,
    pub progress_pages: Option<u32>,
    pub progress_seconds: Option<u32>,
    /// `user_book_reads.progress`, a percentage 0..=100 computed by the server.
    pub progress_percent: Option<f32>,
    pub edition_id: Option<EditionId>,
}

impl ReadSession {
    pub fn new(id: ReadId) -> Self {
        Self {
            id,
            started_at: None,
            finished_at: None,
            progress_pages: None,
            progress_seconds: None,
            progress_percent: None,
            edition_id: None,
        }
    }

    pub fn is_finished(&self) -> bool {
        self.finished_at.is_some()
    }
}

/// One book in the user's library (`user_books`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct LibraryEntry {
    pub user_book_id: UserBookId,
    /// Owner of the entry. Lets the cache file entries per user.
    pub user_id: UserId,
    pub book: BookSummary,
    pub status: ReadingStatus,
    pub rating: Option<Rating>,
    pub edition: Option<EditionSummary>,
    /// The most recent read (highest id), finished or not.
    pub current_read: Option<ReadSession>,
    pub date_added: Option<Date>,
    pub updated_at: Option<Timestamp>,
}

impl LibraryEntry {
    pub fn new(
        user_book_id: UserBookId,
        user_id: UserId,
        book: BookSummary,
        status: ReadingStatus,
    ) -> Self {
        Self {
            user_book_id,
            user_id,
            book,
            status,
            rating: None,
            edition: None,
            current_read: None,
            date_added: None,
            updated_at: None,
        }
    }

    /// The cover to show: the edition's, falling back to the book's.
    pub fn cover(&self) -> Option<&Cover> {
        self.edition
            .as_ref()
            .and_then(|e| e.cover.as_ref())
            .or(self.book.cover.as_ref())
    }

    /// Page count of the edition being read, falling back to the book's default.
    pub fn total_pages(&self) -> Option<u32> {
        self.edition
            .as_ref()
            .and_then(|e| e.pages)
            .or(self.book.pages)
            .filter(|p| *p > 0)
    }
}

/// A series the book belongs to.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeriesRef {
    pub name: String,
    pub slug: Option<String>,
    /// Position in the series; may be fractional (1.5 for a novella).
    pub position: Option<f32>,
    /// Hardcover's featured series for this book.
    pub featured: bool,
}

impl SeriesRef {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            slug: None,
            position: None,
            featured: false,
        }
    }
}

/// Everything the detail page shows for a book.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BookDetails {
    pub summary: BookSummary,
    pub description: Option<String>,
    pub release_date: Option<Date>,
    /// Featured series first.
    pub series: Vec<SeriesRef>,
    /// Top community genres (`cached_tags.Genre`), most-voted first.
    pub genres: Vec<String>,
    /// Top community moods (`cached_tags.Mood`).
    pub moods: Vec<String>,
    /// Top community tags (`cached_tags.Tag`).
    pub tags: Vec<String>,
    /// Top content warnings (`cached_tags["Content Warning"]`).
    pub content_warnings: Vec<String>,
    pub editions_count: Option<u32>,
    pub ratings_count: Option<u32>,
}

impl BookDetails {
    pub fn new(summary: BookSummary) -> Self {
        Self {
            summary,
            description: None,
            release_date: None,
            series: Vec::new(),
            genres: Vec::new(),
            moods: Vec::new(),
            tags: Vec::new(),
            content_warnings: Vec::new(),
            editions_count: None,
            ratings_count: None,
        }
    }
}

/// One page of book search results.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SearchPage {
    pub books: Vec<BookSummary>,
    /// 1-based.
    pub page: u32,
    pub per_page: u32,
    /// Total matches (`found`), when Typesense reports it.
    pub total: Option<u32>,
}

impl SearchPage {
    pub fn new(books: Vec<BookSummary>, page: u32, per_page: u32) -> Self {
        Self {
            books,
            page,
            per_page,
            total: None,
        }
    }

    pub fn has_more(&self) -> bool {
        match self.total {
            Some(total) => u64::from(self.page) * u64::from(self.per_page) < u64::from(total),
            None => self.books.len() as u32 >= self.per_page,
        }
    }
}

/// How far the reader is, for [`crate::api::Client::update_progress`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProgressUpdate {
    /// Current page.
    Pages(u32),
    /// Percentage 0..=100. Hardcover stores pages (or seconds), so this is converted
    /// with the read's edition length, like the official SDK does.
    Percent(f32),
    /// Position in an audiobook.
    Seconds(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_ids_round_trip() {
        for id in 1..=6 {
            assert_eq!(ReadingStatus::from_id(id).unwrap().id(), id);
        }
        assert_eq!(ReadingStatus::from_id(0), None);
        assert_eq!(ReadingStatus::from_id(7), None);
        assert_eq!(ReadingStatus::CurrentlyReading.id(), 2);
        assert!(ReadingStatus::SHELVES.iter().all(|s| s.is_shelf()));
        assert!(!ReadingStatus::Ignored.is_shelf());
    }

    #[test]
    fn rating_validation() {
        assert!(Rating::new(0.0).is_none());
        assert!(Rating::new(0.5).is_some());
        assert!(Rating::new(3.5).is_some());
        assert!(Rating::new(3.25).is_none());
        assert!(Rating::new(5.0).is_some());
        assert!(Rating::new(5.5).is_none());
        assert!(Rating::new(f32::NAN).is_none());
        assert_eq!(Rating::from_api(4.0).unwrap().value(), 4.0);
        assert_eq!(Rating::from_api(3.8).unwrap().value(), 4.0);
        assert_eq!(Rating::from_api(7.0).unwrap().value(), 5.0);
        assert!(Rating::from_api(0.0).is_none());
        assert_eq!(Rating::new(4.0).unwrap().to_string(), "4");
        assert_eq!(Rating::new(4.5).unwrap().to_string(), "4.5");
    }

    #[test]
    fn rating_serde_validates() {
        let r: Rating = serde_json::from_str("4.5").unwrap();
        assert_eq!(r.value(), 4.5);
        assert!(serde_json::from_str::<Rating>("4.3").is_err());
        assert_eq!(serde_json::to_string(&r).unwrap(), "4.5");
    }

    #[test]
    fn ids_are_transparent_in_json() {
        assert_eq!(serde_json::to_string(&BookId::new(42)).unwrap(), "42");
        assert_eq!(
            serde_json::from_str::<UserBookId>("7").unwrap(),
            UserBookId::new(7)
        );
        assert_eq!(ReadId::new(9).to_string(), "9");
    }

    #[test]
    fn search_page_has_more() {
        let mut p = SearchPage::new(vec![], 1, 25);
        p.total = Some(30);
        assert!(p.has_more());
        p.page = 2;
        assert!(!p.has_more());
    }
}
