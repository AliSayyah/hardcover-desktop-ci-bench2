//! Display text derived from core models: status labels, progress, ratings,
//! relative times and user-facing error copy.
//!
//! Plain functions without GPUI so they can be unit tested directly.

use chrono::{DateTime, Datelike, TimeZone};
use hardcover_core::{Error, LibraryEntry, Rating, ReadingStatus};

/// Sentence-case label for a status, as the app shows it.
pub fn status_label(status: ReadingStatus) -> &'static str {
    match status {
        ReadingStatus::WantToRead => "Want to read",
        ReadingStatus::CurrentlyReading => "Currently reading",
        ReadingStatus::Read => "Read",
        ReadingStatus::Paused => "Paused",
        ReadingStatus::DidNotFinish => "Did not finish",
        ReadingStatus::Ignored => "Ignored",
    }
}

/// The library shelves in sidebar order. Ctrl+1…5 follow this order.
pub const SIDEBAR_SHELVES: [ReadingStatus; 5] = [
    ReadingStatus::CurrentlyReading,
    ReadingStatus::WantToRead,
    ReadingStatus::Read,
    ReadingStatus::Paused,
    ReadingStatus::DidNotFinish,
];

/// Reading progress of an entry's open read, ready for display.
#[derive(Debug, Clone, PartialEq)]
pub struct ProgressSummary {
    /// Current page, when the read records pages.
    pub page: Option<u32>,
    /// Page count of the edition being read.
    pub total_pages: Option<u32>,
    /// 0..=100, from the server or derived from pages.
    pub percent: Option<f32>,
}

impl ProgressSummary {
    /// "Page 120 of 350", "Page 120", "42%", or "Not started".
    pub fn text(&self) -> String {
        match (self.page, self.total_pages, self.percent) {
            (Some(page), Some(total), _) => format!("Page {page} of {total}"),
            (Some(page), None, _) => format!("Page {page}"),
            (None, _, Some(pct)) => format!("{}%", pct.round() as u32),
            (None, _, None) => "Not started".to_owned(),
        }
    }

    /// Compact form for a list row's trailing lane: "42%" or "p. 120".
    pub fn short_text(&self) -> Option<String> {
        match (self.percent, self.page) {
            (Some(pct), _) => Some(format!("{}%", pct.round() as u32)),
            (None, Some(page)) => Some(format!("p. {page}")),
            (None, None) => None,
        }
    }
}

/// Progress of the entry's unfinished read, if it has one.
pub fn progress_summary(entry: &LibraryEntry) -> ProgressSummary {
    let total_pages = entry.total_pages();
    let read = entry.current_read.as_ref().filter(|r| !r.is_finished());
    let page = read.and_then(|r| r.progress_pages).filter(|p| *p > 0);
    let percent = read
        .and_then(|r| r.progress_percent)
        .or_else(|| match (page, total_pages) {
            (Some(p), Some(t)) => Some((f64::from(p) / f64::from(t) * 100.0) as f32),
            _ => None,
        })
        .map(|p| p.clamp(0.0, 100.0));
    ProgressSummary {
        page,
        total_pages,
        percent,
    }
}

/// "4", "3.5".
pub fn rating_text(rating: Rating) -> String {
    let v = rating.value();
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.1}")
    }
}

/// Hardcover's community average, "4.12".
pub fn average_rating_text(avg: f32) -> String {
    format!("{avg:.2}")
}

/// Joined author list, or a placeholder.
pub fn authors_text(authors: &[String]) -> String {
    if authors.is_empty() {
        "Unknown author".to_owned()
    } else {
        authors.join(", ")
    }
}

/// "#3", "#2.5" for a series position.
pub fn series_position_text(position: f32) -> String {
    if position.fract() == 0.0 {
        format!("#{position:.0}")
    } else {
        format!("#{position}")
    }
}

/// "just now", "5 min ago", "3 hours ago", "yesterday", "4 days ago",
/// "on Nov 3" or "on Nov 3, 2024".
pub fn relative_time<Tz: TimeZone>(now: &DateTime<Tz>, then: &DateTime<Tz>) -> String
where
    Tz::Offset: std::fmt::Display,
{
    let secs = (now.clone() - then.clone()).num_seconds().max(0);
    let mins = secs / 60;
    let hours = mins / 60;
    let days = hours / 24;
    match () {
        _ if mins < 1 => "just now".to_owned(),
        _ if mins < 60 => format!("{mins} min ago"),
        _ if hours == 1 => "1 hour ago".to_owned(),
        _ if hours < 24 => format!("{hours} hours ago"),
        _ if days == 1 => "yesterday".to_owned(),
        _ if days < 7 => format!("{days} days ago"),
        _ if then.year() == now.year() => format!("on {}", then.format("%b %-d")),
        _ => format!("on {}", then.format("%b %-d, %Y")),
    }
}

/// What to tell the user when a request fails, with a recovery hint.
pub fn error_message(error: &Error) -> String {
    match error {
        Error::RateLimited { .. } => {
            "Hardcover's rate limit was reached. Try again in a minute.".to_owned()
        }
        Error::Network(_) | Error::Timeout => {
            "Couldn't reach Hardcover. Check your connection and try again.".to_owned()
        }
        Error::Unauthorized => "Your session has expired. Sign out and sign in again.".to_owned(),
        Error::InsufficientScope { .. } => {
            "Hardcover didn't grant this app access to your library. Sign out and sign in again."
                .to_owned()
        }
        Error::InvalidInput(msg) => sentence(msg),
        Error::Storage(msg) => format!("Couldn't save data on this computer: {msg}."),
        Error::Auth(msg) => format!("Sign-in failed: {msg}."),
        Error::Cancelled => "Sign-in was cancelled.".to_owned(),
        Error::Api(msg) => format!("Hardcover reported an error: {msg}."),
        Error::Decode(_) => "Hardcover sent an unexpected response. Try again later.".to_owned(),
        _ => format!("{error}."),
    }
}

/// Upper-cases the first letter and ends with a period.
fn sentence(msg: &str) -> String {
    let msg = msg.trim().trim_end_matches('.');
    let mut chars = msg.chars();
    match chars.next() {
        Some(first) => format!("{}{}.", first.to_uppercase(), chars.as_str()),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use hardcover_core::{
        BookId, BookSummary, EditionId, EditionSummary, ReadId, ReadSession, UserBookId, UserId,
    };

    fn entry_with_read(
        pages: Option<u32>,
        percent: Option<f32>,
        total: Option<u32>,
    ) -> LibraryEntry {
        let mut book = BookSummary::new(BookId::new(1), "Book");
        book.pages = total;
        let mut e = LibraryEntry::new(
            UserBookId::new(1),
            UserId::new(1),
            book,
            ReadingStatus::CurrentlyReading,
        );
        let mut read = ReadSession::new(ReadId::new(9));
        read.progress_pages = pages;
        read.progress_percent = percent;
        e.current_read = Some(read);
        e
    }

    #[test]
    fn progress_with_pages_and_total() {
        let p = progress_summary(&entry_with_read(Some(120), None, Some(400)));
        assert_eq!(p.text(), "Page 120 of 400");
        assert_eq!(p.percent, Some(30.0));
        assert_eq!(p.short_text().as_deref(), Some("30%"));
    }

    #[test]
    fn progress_prefers_edition_pages_and_server_percent() {
        let mut e = entry_with_read(Some(50), Some(25.0), Some(400));
        let mut edition = EditionSummary::new(EditionId::new(3));
        edition.pages = Some(200);
        e.edition = Some(edition);
        let p = progress_summary(&e);
        assert_eq!(p.text(), "Page 50 of 200");
        assert_eq!(p.percent, Some(25.0));
    }

    #[test]
    fn progress_fallbacks() {
        assert_eq!(
            progress_summary(&entry_with_read(Some(12), None, None)).text(),
            "Page 12"
        );
        assert_eq!(
            progress_summary(&entry_with_read(Some(12), None, None))
                .short_text()
                .as_deref(),
            Some("p. 12")
        );
        assert_eq!(
            progress_summary(&entry_with_read(None, Some(41.6), None)).text(),
            "42%"
        );
        let none = progress_summary(&entry_with_read(None, None, Some(300)));
        assert_eq!(none.text(), "Not started");
        assert_eq!(none.short_text(), None);
    }

    #[test]
    fn finished_read_has_no_progress() {
        let mut e = entry_with_read(Some(300), Some(100.0), Some(300));
        e.current_read.as_mut().unwrap().finished_at = chrono::NaiveDate::from_ymd_opt(2024, 1, 1);
        let p = progress_summary(&e);
        assert_eq!(p.page, None);
        assert_eq!(p.percent, None);
    }

    #[test]
    fn ratings_and_positions() {
        assert_eq!(rating_text(Rating::new(4.0).unwrap()), "4");
        assert_eq!(rating_text(Rating::new(3.5).unwrap()), "3.5");
        assert_eq!(series_position_text(3.0), "#3");
        assert_eq!(series_position_text(2.5), "#2.5");
        assert_eq!(average_rating_text(4.123), "4.12");
        assert_eq!(authors_text(&[]), "Unknown author");
        assert_eq!(authors_text(&["A".into(), "B".into()]), "A, B");
    }

    #[test]
    fn relative_times() {
        let now = Utc.with_ymd_and_hms(2026, 11, 19, 12, 0, 0).unwrap();
        let ago = |d: Duration| relative_time(&now, &(now - d));
        assert_eq!(ago(Duration::seconds(20)), "just now");
        assert_eq!(ago(Duration::minutes(5)), "5 min ago");
        assert_eq!(ago(Duration::minutes(61)), "1 hour ago");
        assert_eq!(ago(Duration::hours(5)), "5 hours ago");
        assert_eq!(ago(Duration::hours(30)), "yesterday");
        assert_eq!(ago(Duration::days(4)), "4 days ago");
        assert_eq!(ago(Duration::days(16)), "on Nov 3");
        assert_eq!(ago(Duration::days(400)), "on Oct 15, 2025");
        // Clock skew never yields a negative time.
        assert_eq!(
            relative_time(&now, &(now + Duration::minutes(3))),
            "just now"
        );
    }

    #[test]
    fn error_copy() {
        assert_eq!(
            error_message(&Error::RateLimited { retry_after: None }),
            "Hardcover's rate limit was reached. Try again in a minute."
        );
        assert_eq!(
            error_message(&Error::Timeout),
            "Couldn't reach Hardcover. Check your connection and try again."
        );
        assert_eq!(
            error_message(&Error::InvalidInput(
                "finish date is before the start date".into()
            )),
            "Finish date is before the start date."
        );
    }

    #[test]
    fn labels_are_sentence_case() {
        for s in SIDEBAR_SHELVES {
            let label = status_label(s);
            assert!(label.chars().skip(1).all(|c| !c.is_uppercase()), "{label}");
        }
    }
}
