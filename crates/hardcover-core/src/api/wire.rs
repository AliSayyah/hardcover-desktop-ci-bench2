//! Raw response shapes and their lenient conversion into the public models.
//!
//! Hardcover's `cached_*` columns are JSON blobs with no schema, and search results
//! are a raw Typesense response, so those are read field by field from
//! `serde_json::Value` and anything unexpected is dropped rather than failing the
//! whole response.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use super::model::*;

// ---------- scalar helpers ----------

pub(crate) fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|f| f.is_finite())
}

fn as_u32(v: &Value) -> Option<u32> {
    as_i64(v).and_then(|n| u32::try_from(n).ok())
}

fn as_string(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn opt_u32(n: Option<i64>) -> Option<u32> {
    n.and_then(|n| u32::try_from(n).ok())
}

fn non_empty(s: Option<String>) -> Option<String> {
    s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

pub(crate) fn parse_date(s: &str) -> Option<Date> {
    let s = s.trim();
    NaiveDate::parse_from_str(s.get(..10)?, "%Y-%m-%d").ok()
}

pub(crate) fn parse_timestamp(s: &str) -> Option<Timestamp> {
    let s = s.trim();
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(t.with_timezone(&Utc));
    }
    // Hasura sometimes renders "+00:00" as "+00", and `timestamp` columns have no offset.
    let trimmed = s.strip_suffix("+00").unwrap_or(s);
    NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f")
        .ok()
        .map(|n| n.and_utc())
}

// ---------- shared blobs ----------

/// An `images` row or a `cached_image` blob. An image-less book has `{}`.
pub(crate) fn cover_from_value(v: &Value) -> Option<Cover> {
    let url = v.get("url").and_then(as_string)?;
    let mut c = Cover::new(url);
    c.width = v.get("width").and_then(as_u32);
    c.height = v.get("height").and_then(as_u32);
    Some(c)
}

/// `cached_contributors` (and search `contributions`): primary authors are the
/// entries with no role or the "Author" role. Falls back to everyone credited.
pub(crate) fn authors_from_contributors(v: &Value) -> Vec<String> {
    let Some(list) = v.as_array() else {
        return Vec::new();
    };
    let named: Vec<(Option<String>, String)> = list
        .iter()
        .filter_map(|c| {
            let name = c
                .get("author")
                .and_then(|a| a.get("name"))
                .and_then(as_string)?;
            Some((c.get("contribution").and_then(as_string), name))
        })
        .collect();
    let mut authors: Vec<String> = named
        .iter()
        .filter(|(role, _)| {
            role.as_deref()
                .is_none_or(|r| r.eq_ignore_ascii_case("author"))
        })
        .map(|(_, n)| n.clone())
        .collect();
    if authors.is_empty() {
        authors = named.into_iter().map(|(_, n)| n).collect();
    }
    dedup(authors)
}

fn dedup(items: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(items.len());
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array)
        .map(|a| a.iter().filter_map(as_string).collect())
        .unwrap_or_default()
}

/// `cached_tags` is `{"Genre": [{"tag": "...", "count": n, ...}], "Mood": [...], ...}`.
fn tags_in(cached_tags: &Value, category: &str) -> Vec<String> {
    let Some(list) = cached_tags.get(category).and_then(Value::as_array) else {
        return Vec::new();
    };
    dedup(
        list.iter()
            .filter_map(|t| t.get("tag").and_then(as_string))
            .collect(),
    )
}

// ---------- books ----------

#[derive(Debug, Deserialize)]
pub(crate) struct RawBook {
    pub id: i64,
    pub title: Option<String>,
    pub subtitle: Option<String>,
    pub slug: Option<String>,
    pub pages: Option<i64>,
    pub release_year: Option<i64>,
    pub rating: Option<Value>,
    pub users_count: Option<i64>,
    #[serde(default)]
    pub cached_image: Value,
    #[serde(default)]
    pub cached_contributors: Value,
}

impl RawBook {
    pub fn into_summary(self) -> BookSummary {
        let mut b = BookSummary::new(
            BookId::new(self.id),
            non_empty(self.title).unwrap_or_else(|| "Untitled".to_owned()),
        );
        b.subtitle = non_empty(self.subtitle);
        b.authors = authors_from_contributors(&self.cached_contributors);
        b.cover = cover_from_value(&self.cached_image);
        b.pages = opt_u32(self.pages).filter(|p| *p > 0);
        b.release_year = self.release_year.and_then(|y| i32::try_from(y).ok());
        b.slug = non_empty(self.slug);
        b.average_rating = self.rating.as_ref().and_then(as_f64).map(|r| r as f32);
        b.users_count = opt_u32(self.users_count);
        b
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawBookDetails {
    #[serde(flatten)]
    pub book: RawBook,
    pub description: Option<String>,
    pub release_date: Option<String>,
    pub editions_count: Option<i64>,
    pub ratings_count: Option<i64>,
    #[serde(default)]
    pub cached_tags: Value,
    #[serde(default)]
    pub book_series: Vec<RawBookSeries>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawBookSeries {
    pub position: Option<f64>,
    #[serde(default)]
    pub featured: bool,
    pub series: Option<RawSeries>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSeries {
    pub name: Option<String>,
    pub slug: Option<String>,
}

impl RawBookDetails {
    pub fn into_details(self) -> BookDetails {
        let tags = &self.cached_tags;
        let mut d = BookDetails::new(self.book.into_summary());
        d.description = non_empty(self.description);
        d.release_date = self.release_date.as_deref().and_then(parse_date);
        d.genres = tags_in(tags, "Genre");
        d.moods = tags_in(tags, "Mood");
        d.tags = tags_in(tags, "Tag");
        d.content_warnings = tags_in(tags, "Content Warning");
        d.editions_count = opt_u32(self.editions_count);
        d.ratings_count = opt_u32(self.ratings_count);
        let mut seen = Vec::new();
        for bs in self.book_series {
            let Some(series) = bs.series else { continue };
            let Some(name) = non_empty(series.name) else {
                continue;
            };
            if seen.contains(&name) {
                continue;
            }
            seen.push(name.clone());
            let mut s = SeriesRef::new(name);
            s.slug = non_empty(series.slug);
            s.position = bs.position.filter(|p| p.is_finite()).map(|p| p as f32);
            s.featured = bs.featured;
            d.series.push(s);
        }
        d
    }
}

// ---------- library ----------

#[derive(Debug, Deserialize)]
pub(crate) struct RawEdition {
    pub id: i64,
    pub title: Option<String>,
    pub pages: Option<i64>,
    pub audio_seconds: Option<i64>,
    pub edition_format: Option<String>,
    #[serde(default)]
    pub cached_image: Value,
}

impl RawEdition {
    fn into_summary(self) -> EditionSummary {
        let mut e = EditionSummary::new(EditionId::new(self.id));
        e.title = non_empty(self.title);
        e.pages = opt_u32(self.pages).filter(|p| *p > 0);
        e.audio_seconds = opt_u32(self.audio_seconds).filter(|s| *s > 0);
        e.cover = cover_from_value(&self.cached_image);
        e.format = non_empty(self.edition_format);
        e
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawRead {
    pub id: i64,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    pub progress: Option<f64>,
    pub progress_pages: Option<i64>,
    pub progress_seconds: Option<i64>,
    pub edition_id: Option<i64>,
}

impl RawRead {
    pub(crate) fn into_session(self) -> ReadSession {
        let mut r = ReadSession::new(ReadId::new(self.id));
        r.started_at = self.started_at.as_deref().and_then(parse_date);
        r.finished_at = self.finished_at.as_deref().and_then(parse_date);
        r.progress_pages = opt_u32(self.progress_pages);
        r.progress_seconds = opt_u32(self.progress_seconds);
        r.progress_percent = self
            .progress
            .filter(|p| p.is_finite())
            .map(|p| p.clamp(0.0, 100.0) as f32);
        r.edition_id = self.edition_id.map(EditionId::new);
        r
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawUserBook {
    pub id: i64,
    pub user_id: i64,
    pub status_id: i64,
    pub rating: Option<Value>,
    pub date_added: Option<String>,
    pub updated_at: Option<String>,
    pub book: RawBook,
    pub edition: Option<RawEdition>,
    #[serde(default)]
    pub user_book_reads: Vec<RawRead>,
}

impl RawUserBook {
    /// `None` when the status id is unknown to [`ReadingStatus`].
    pub fn into_entry(self) -> Option<LibraryEntry> {
        let status = ReadingStatus::from_id(self.status_id)?;
        let mut e = LibraryEntry::new(
            UserBookId::new(self.id),
            UserId::new(self.user_id),
            self.book.into_summary(),
            status,
        );
        e.rating = self
            .rating
            .as_ref()
            .and_then(as_f64)
            .and_then(Rating::from_api);
        e.edition = self.edition.map(RawEdition::into_summary);
        e.current_read = self
            .user_book_reads
            .into_iter()
            .max_by_key(|r| r.id)
            .map(RawRead::into_session);
        e.date_added = self.date_added.as_deref().and_then(parse_date);
        e.updated_at = self.updated_at.as_deref().and_then(parse_timestamp);
        Some(e)
    }
}

// ---------- me ----------

#[derive(Debug, Deserialize)]
pub(crate) struct RawUser {
    pub id: i64,
    pub username: Option<String>,
    pub name: Option<String>,
    #[serde(default)]
    pub image: Value,
}

impl RawUser {
    pub fn into_user(self) -> User {
        let mut u = User::new(UserId::new(self.id), self.username.unwrap_or_default());
        u.name = non_empty(self.name);
        u.avatar = cover_from_value(&self.image);
        u
    }
}

// ---------- search ----------

#[derive(Debug, Deserialize)]
pub(crate) struct RawSearch {
    pub error: Option<String>,
    pub page: Option<i64>,
    pub per_page: Option<i64>,
    #[serde(default)]
    pub results: Value,
}

/// `results` is the Typesense response: `{found, page, hits: [{document: {...}}]}`.
/// Hits without a usable id are skipped.
pub(crate) fn search_page(raw: RawSearch, page: u32, per_page: u32) -> SearchPage {
    let results = &raw.results;
    let books = results
        .get("hits")
        .and_then(Value::as_array)
        .map(|hits| {
            hits.iter()
                .filter_map(|h| h.get("document"))
                .filter_map(book_from_search_document)
                .collect()
        })
        .unwrap_or_default();
    let mut p = SearchPage::new(
        books,
        raw.page.and_then(|n| u32::try_from(n).ok()).unwrap_or(page),
        raw.per_page
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(per_page),
    );
    p.total = results.get("found").and_then(as_u32);
    p
}

fn book_from_search_document(doc: &Value) -> Option<BookSummary> {
    let id = doc.get("id").and_then(as_i64)?;
    let title = doc.get("title").and_then(as_string)?;
    let mut b = BookSummary::new(BookId::new(id), title);
    b.subtitle = doc.get("subtitle").and_then(as_string);
    b.authors = dedup(string_list(doc.get("author_names")));
    if b.authors.is_empty() {
        b.authors = doc
            .get("contributions")
            .map(authors_from_contributors)
            .unwrap_or_default();
    }
    b.cover = doc.get("image").and_then(cover_from_value);
    b.pages = doc.get("pages").and_then(as_u32).filter(|p| *p > 0);
    b.release_year = doc
        .get("release_year")
        .and_then(as_i64)
        .and_then(|y| i32::try_from(y).ok());
    b.slug = doc.get("slug").and_then(as_string);
    b.average_rating = doc.get("rating").and_then(as_f64).map(|r| r as f32);
    b.users_count = doc.get("users_count").and_then(as_u32);
    Some(b)
}

// ---------- mutation payloads ----------

#[derive(Debug, Deserialize)]
pub(crate) struct UserBookIdPayload {
    pub error: Option<String>,
    pub id: Option<i64>,
    pub user_book: Option<RawUserBook>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadIdPayload {
    pub error: Option<String>,
    pub id: Option<i64>,
    pub user_book_read: Option<ReadRef>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ReadRef {
    #[serde(flatten)]
    pub read: RawRead,
    pub user_book_id: i64,
    pub user_book: Option<RawUserBook>,
}

impl ReadRef {
    /// The nested entry with the written read as its current read (when it is the latest).
    pub fn into_entry(self) -> Option<LibraryEntry> {
        let mut entry = self.user_book?.into_entry()?;
        let read = self.read.into_session();
        if entry.current_read.as_ref().is_none_or(|r| r.id <= read.id) {
            entry.current_read = Some(read);
        }
        Some(entry)
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct DeletePayload {
    pub id: Option<i64>,
}

// ---------- progress context ----------

#[derive(Debug, Deserialize)]
pub(crate) struct Lengths {
    pub pages: Option<i64>,
    pub audio_seconds: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ContextRead {
    pub id: i64,
    pub edition: Option<Lengths>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct ProgressContext {
    pub status_id: Option<i64>,
    pub book: Option<Lengths>,
    pub edition: Option<Lengths>,
    #[serde(default)]
    pub user_book_reads: Vec<ContextRead>,
}

impl ProgressContext {
    /// (pages, audio seconds) of the read's edition, then the entry's edition, then the book.
    pub fn lengths(&self) -> (Option<u32>, Option<u32>) {
        let candidates = [
            self.user_book_reads
                .first()
                .and_then(|r| r.edition.as_ref()),
            self.edition.as_ref(),
            self.book.as_ref(),
        ];
        let pages = candidates
            .iter()
            .flatten()
            .find_map(|l| opt_u32(l.pages).filter(|p| *p > 0));
        let seconds = candidates
            .iter()
            .flatten()
            .find_map(|l| opt_u32(l.audio_seconds).filter(|s| *s > 0));
        (pages, seconds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamps_and_dates() {
        let t = parse_timestamp("2025-03-04T05:06:07.123456+00:00").unwrap();
        assert_eq!(t.to_rfc3339(), "2025-03-04T05:06:07.123456+00:00");
        assert!(parse_timestamp("2025-03-04T05:06:07.5").is_some());
        assert!(parse_timestamp("2025-03-04T07:06:07+02:00").is_some());
        assert!(parse_timestamp("garbage").is_none());
        assert_eq!(parse_date("2024-02-29").unwrap().to_string(), "2024-02-29");
        assert!(parse_date("2024").is_none());
    }

    #[test]
    fn empty_cached_image_is_no_cover() {
        assert!(cover_from_value(&json!({})).is_none());
        assert!(cover_from_value(&Value::Null).is_none());
        assert!(cover_from_value(&json!({"url": ""})).is_none());
        let c = cover_from_value(
            &json!({"url": "https://assets.example/x.jpg", "width": 10, "height": "20"}),
        )
        .unwrap();
        assert_eq!((c.width, c.height), (Some(10), Some(20)));
    }

    #[test]
    fn authors_prefer_author_role() {
        let v = json!([
            {"author": {"name": "Ada Writer"}, "contribution": null},
            {"author": {"name": "Tom Translator"}, "contribution": "Translator"},
            {"author": {"name": "Bea Coauthor"}, "contribution": "Author"},
            {"author": {"name": "Ada Writer"}, "contribution": "Author"},
            {"contribution": "Author"}
        ]);
        assert_eq!(
            authors_from_contributors(&v),
            vec!["Ada Writer", "Bea Coauthor"]
        );
        let only_editor = json!([{"author": {"name": "Ed Itor"}, "contribution": "Editor"}]);
        assert_eq!(authors_from_contributors(&only_editor), vec!["Ed Itor"]);
        assert!(authors_from_contributors(&json!({})).is_empty());
    }
}
