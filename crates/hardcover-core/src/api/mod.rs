//! Blocking GraphQL client for the Hardcover API.

mod model;
mod queries;
#[cfg(test)]
pub(crate) mod sdl;
mod wire;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};

pub use model::*;

/// Every GraphQL document the client sends, as `(name, document)`. For tooling
/// that checks them against the live API (the `api_check` example).
#[doc(hidden)]
pub const DOCUMENTS: &[(&str, &str)] = queries::ALL;

use crate::http::{HttpRequest, HttpResponse, HttpTransport, Method, UreqTransport};
use crate::ratelimit::TokenBucket;
use crate::{Error, Result};
pub(crate) use wire::as_i64 as wire_i64;
use wire::*;

/// GraphQL endpoint.
pub const GRAPHQL_ENDPOINT: &str = "https://api.hardcover.app/v1/graphql";

/// Library page size. One request per page; each costs one rate-limit token.
const LIBRARY_PAGE_SIZE: u32 = 100;
/// Search results per page.
pub const SEARCH_PER_PAGE: u32 = 25;

/// The server cuts queries off at 30s and searches at 2s; leave room for the network.
const QUERY_TIMEOUT: Duration = Duration::from_secs(40);
const SEARCH_TIMEOUT: Duration = Duration::from_secs(15);
/// A 429 asking us to wait at most this long is retried once (after waiting).
const MAX_AUTO_RETRY_AFTER: Duration = Duration::from_secs(15);

/// Supplies bearer tokens to the [`Client`].
pub trait TokenSource: Send + Sync {
    /// A token to send now. May refresh first if the current one is about to expire.
    fn access_token(&self) -> Result<String>;

    /// The API rejected `rejected` with HTTP 401. Return `Ok(true)` if a different
    /// token is now available (the client retries once), `Ok(false)` to give up
    /// with [`Error::Unauthorized`].
    ///
    /// `rejected` lets an implementation see that another thread already refreshed,
    /// so concurrent 401s do not spend a rotating refresh token twice.
    fn on_unauthorized(&self, rejected: &str) -> Result<bool>;
}

/// Hardcover API client. Cheap to clone; clones share the rate limiter, the
/// token source and the cached `me` id.
///
/// Every method blocks (including while waiting for the client-side rate limiter)
/// and must run off the UI thread.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

struct Inner {
    tokens: Arc<dyn TokenSource>,
    http: Arc<dyn HttpTransport>,
    bucket: TokenBucket,
    endpoint: String,
    me_id: OnceLock<UserId>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("endpoint", &self.inner.endpoint)
            .finish_non_exhaustive()
    }
}

impl Client {
    pub fn new(tokens: Arc<dyn TokenSource>) -> Self {
        Self::with_transport(
            tokens,
            Arc::new(UreqTransport::new()),
            TokenBucket::default(),
        )
    }

    pub(crate) fn with_transport(
        tokens: Arc<dyn TokenSource>,
        http: Arc<dyn HttpTransport>,
        bucket: TokenBucket,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                tokens,
                http,
                bucket,
                endpoint: GRAPHQL_ENDPOINT.to_owned(),
                me_id: OnceLock::new(),
            }),
        }
    }

    // ----- reads -----

    /// The signed-in user. Also remembers their id for [`Client::library_entry`].
    pub fn me(&self) -> Result<User> {
        #[derive(Deserialize)]
        struct Data {
            me: Vec<RawUser>,
        }
        let data: Data = self.query("Me", queries::ME, json!({}), QUERY_TIMEOUT)?;
        let user = data
            .me
            .into_iter()
            .next()
            .ok_or_else(|| Error::Decode("`me` returned no user".into()))?
            .into_user();
        let _ = self.inner.me_id.set(user.id);
        Ok(user)
    }

    fn my_id(&self) -> Result<UserId> {
        match self.inner.me_id.get() {
            Some(id) => Ok(*id),
            None => Ok(self.me()?.id),
        }
    }

    /// The user's whole library (statuses 1..=5; "Ignored" entries are left out),
    /// ordered by `user_book_id`. Pages of 100, one request each.
    pub fn library(&self, user: UserId) -> Result<Vec<LibraryEntry>> {
        self.library_with_progress(user, |_| {})
    }

    /// Like [`Client::library`], calling `on_page` with each page as it arrives
    /// so the UI can show partial results.
    pub fn library_with_progress(
        &self,
        user: UserId,
        mut on_page: impl FnMut(&[LibraryEntry]),
    ) -> Result<Vec<LibraryEntry>> {
        #[derive(Deserialize)]
        struct Data {
            user_books: Vec<RawUserBook>,
        }
        let mut all = Vec::new();
        let mut after = 0_i64;
        loop {
            let data: Data = self.query(
                "LibraryPage",
                queries::LIBRARY_PAGE,
                json!({ "userId": user.get(), "afterId": after, "limit": LIBRARY_PAGE_SIZE }),
                QUERY_TIMEOUT,
            )?;
            let fetched = data.user_books.len();
            let mut page = Vec::with_capacity(fetched);
            for raw in data.user_books {
                after = after.max(raw.id);
                if let Some(entry) = raw.into_entry() {
                    page.push(entry);
                }
            }
            on_page(&page);
            all.extend(page);
            if fetched < LIBRARY_PAGE_SIZE as usize {
                return Ok(all);
            }
        }
    }

    /// One of the signed-in user's entries. `None` if it does not exist or belongs
    /// to someone else (ids are global, and other users' public entries are readable).
    pub fn library_entry(&self, id: UserBookId) -> Result<Option<LibraryEntry>> {
        let me = self.my_id()?;
        self.fetch_entry(
            "UserBookForUser",
            queries::USER_BOOK_FOR_USER,
            json!({ "userId": me.get(), "id": id.get() }),
        )
    }

    /// The user's entry for `book`, whatever its status (including Ignored).
    pub fn library_entry_for_book(
        &self,
        user: UserId,
        book: BookId,
    ) -> Result<Option<LibraryEntry>> {
        self.fetch_entry(
            "UserBookForBook",
            queries::USER_BOOK_FOR_BOOK,
            json!({ "userId": user.get(), "bookId": book.get() }),
        )
    }

    fn entry_by_id(&self, id: i64) -> Result<Option<LibraryEntry>> {
        #[derive(Deserialize)]
        struct Data {
            user_books_by_pk: Option<RawUserBook>,
        }
        let data: Data = self.query(
            "UserBookById",
            queries::USER_BOOK_BY_ID,
            json!({ "id": id }),
            QUERY_TIMEOUT,
        )?;
        Ok(data.user_books_by_pk.and_then(RawUserBook::into_entry))
    }

    fn fetch_entry(&self, op: &str, doc: &str, vars: Value) -> Result<Option<LibraryEntry>> {
        #[derive(Deserialize)]
        struct Data {
            user_books: Vec<RawUserBook>,
        }
        let data: Data = self.query(op, doc, vars, QUERY_TIMEOUT)?;
        Ok(data
            .user_books
            .into_iter()
            .next()
            .and_then(RawUserBook::into_entry))
    }

    /// Book search (Hardcover's Typesense index, same ranking as the website).
    /// `page` is 1-based; a blank query returns an empty page without a request.
    pub fn search_books(&self, query: &str, page: u32) -> Result<SearchPage> {
        let query = query.trim();
        let page = page.max(1);
        if query.is_empty() {
            return Ok(SearchPage::new(Vec::new(), page, SEARCH_PER_PAGE));
        }
        #[derive(Deserialize)]
        struct Data {
            search: Option<RawSearch>,
        }
        let data: Data = self.query(
            "SearchBooks",
            queries::SEARCH_BOOKS,
            json!({ "query": query, "page": page, "perPage": SEARCH_PER_PAGE }),
            SEARCH_TIMEOUT,
        )?;
        let raw = data
            .search
            .ok_or_else(|| Error::Decode("`search` returned null".into()))?;
        if let Some(e) = raw.error.as_deref().filter(|e| !e.is_empty()) {
            return Err(Error::Api(e.to_owned()));
        }
        Ok(search_page(raw, page, SEARCH_PER_PAGE))
    }

    /// Full details for the book page. `Error::Api` if the book does not exist.
    pub fn book_details(&self, id: BookId) -> Result<BookDetails> {
        #[derive(Deserialize)]
        struct Data {
            books_by_pk: Option<RawBookDetails>,
        }
        let data: Data = self.query(
            "BookDetails",
            queries::BOOK_DETAILS,
            json!({ "id": id.get() }),
            QUERY_TIMEOUT,
        )?;
        data.books_by_pk
            .map(RawBookDetails::into_details)
            .ok_or_else(|| Error::Api(format!("book {id} not found")))
    }

    // ----- writes -----

    /// Adds `book` with `status`, using the account's default privacy. The server
    /// dates the status change today.
    pub fn add_to_library(&self, book: BookId, status: ReadingStatus) -> Result<LibraryEntry> {
        self.insert_user_book(json!({ "book_id": book.get(), "status_id": status.id() }))
    }

    /// Like [`Client::add_to_library`] with an explicit privacy for the entry.
    pub fn add_to_library_with_privacy(
        &self,
        book: BookId,
        status: ReadingStatus,
        privacy: Privacy,
    ) -> Result<LibraryEntry> {
        self.insert_user_book(json!({
            "book_id": book.get(),
            "status_id": status.id(),
            "privacy_setting_id": privacy.id(),
        }))
    }

    fn insert_user_book(&self, object: Value) -> Result<LibraryEntry> {
        #[derive(Deserialize)]
        struct Data {
            insert_user_book: Option<UserBookIdPayload>,
        }
        let data: Data = self.query(
            "InsertUserBook",
            queries::INSERT_USER_BOOK,
            json!({ "object": object }),
            QUERY_TIMEOUT,
        )?;
        self.user_book_payload(data.insert_user_book, "insert_user_book")
    }

    /// Changes the status. Hardcover starts a read on Currently Reading and
    /// finishes the open read on Read.
    pub fn set_status(&self, id: UserBookId, status: ReadingStatus) -> Result<LibraryEntry> {
        self.update_user_book(id, json!({ "status_id": status.id() }))
    }

    /// Sets or (with `None`) clears the rating, keeping the entry at `keep_status`.
    ///
    /// Hardcover treats a bare rating as "I read this": it sets the status to Read
    /// and finishes the open read at 100% (verified live). Sending the status the
    /// entry should keep avoids that; pass `ReadingStatus::Read` to get the
    /// server's behaviour on purpose. Pass the status the UI currently shows.
    pub fn set_rating(
        &self,
        id: UserBookId,
        rating: Option<Rating>,
        keep_status: ReadingStatus,
    ) -> Result<LibraryEntry> {
        let rating = rating.map(|r| f64::from(r.value()));
        self.update_user_book(
            id,
            json!({ "rating": rating, "status_id": keep_status.id() }),
        )
    }

    fn update_user_book(&self, id: UserBookId, object: Value) -> Result<LibraryEntry> {
        #[derive(Deserialize)]
        struct Data {
            update_user_book: Option<UserBookIdPayload>,
        }
        let data: Data = self.query(
            "UpdateUserBook",
            queries::UPDATE_USER_BOOK,
            json!({ "id": id.get(), "object": object }),
            QUERY_TIMEOUT,
        )?;
        self.user_book_payload(data.update_user_book, "update_user_book")
    }

    /// Uses the entry returned by the mutation, or re-reads it if the payload had none.
    fn user_book_payload(
        &self,
        payload: Option<UserBookIdPayload>,
        field: &str,
    ) -> Result<LibraryEntry> {
        let p = payload.ok_or_else(|| Error::Api(format!("`{field}` returned null")))?;
        if let Some(e) = p.error.filter(|e| !e.is_empty()) {
            return Err(Error::Api(e));
        }
        if let Some(entry) = p.user_book.and_then(RawUserBook::into_entry) {
            return Ok(entry);
        }
        let id = p.id.ok_or_else(|| {
            Error::Decode(format!("`{field}` returned neither an entry nor an id"))
        })?;
        self.entry_by_id(id)?
            .ok_or_else(|| Error::Decode(format!("entry {id} not found after `{field}`")))
    }

    /// Removes the book (and its reads) from the library.
    pub fn remove_from_library(&self, id: UserBookId) -> Result<()> {
        #[derive(Deserialize)]
        struct Data {
            delete_user_book: Option<DeletePayload>,
        }
        let data: Data = self.query(
            "DeleteUserBook",
            queries::DELETE_USER_BOOK,
            json!({ "id": id.get() }),
            QUERY_TIMEOUT,
        )?;
        match data.delete_user_book {
            Some(DeletePayload { id: Some(_) }) => Ok(()),
            _ => Err(Error::Api(format!("could not remove library entry {id}"))),
        }
    }

    /// Records reading progress on `read`, or (with `None`) on the latest unfinished read.
    ///
    /// With `None` and no unfinished read: if the entry is not Currently Reading it
    /// is moved there first (Hardcover then starts a read, which gets the progress);
    /// otherwise a read started today is created. This avoids two open reads:
    /// Hardcover starts a *new* read on every move to Currently Reading.
    ///
    /// Prefer passing the `ReadId` from the entry you already hold: a lookup made
    /// right after another write can miss a read the server just started.
    ///
    /// Percentages are converted with the edition's page count (or audio length).
    /// Progress that reaches the end makes Hardcover finish the read and mark the
    /// book Read; the returned entry reflects that.
    pub fn update_progress(
        &self,
        id: UserBookId,
        read: Option<ReadId>,
        progress: ProgressUpdate,
    ) -> Result<LibraryEntry> {
        if let ProgressUpdate::Percent(pct) = progress
            && !(0.0..=100.0).contains(&pct)
        {
            return Err(Error::InvalidInput(format!(
                "progress {pct}% is outside 0..=100"
            )));
        }
        let needs_context = read.is_none() || matches!(progress, ProgressUpdate::Percent(_));
        let context = if needs_context {
            Some(self.progress_context(id, read)?)
        } else {
            None
        };
        let fields = progress_fields(progress, context.as_ref())?;
        let target = read.or_else(|| {
            context
                .as_ref()
                .and_then(|c| c.user_book_reads.first())
                .map(|r| ReadId::new(r.id))
        });
        let target = match (target, &context) {
            (None, Some(ctx)) if ctx.status_id != Some(ReadingStatus::CurrentlyReading.id()) => {
                let entry = self.set_status(id, ReadingStatus::CurrentlyReading)?;
                entry
                    .current_read
                    .filter(|r| !r.is_finished())
                    .map(|r| r.id)
            }
            (t, _) => t,
        };
        let written = match target {
            Some(read) => self.update_read(read, Value::Object(fields))?,
            None => {
                let mut object = fields;
                let today = chrono::Local::now().date_naive();
                object.insert("started_at".into(), json!(today.to_string()));
                self.insert_read(id, Value::Object(object))?
            }
        };
        self.entry_after_read_write(written, "progress update")
    }

    /// The entry from a read mutation's payload, or (if the server left it out)
    /// read back by id.
    fn entry_after_read_write(&self, written: ReadRef, what: &str) -> Result<LibraryEntry> {
        let user_book_id = written.user_book_id;
        if let Some(entry) = written.into_entry() {
            return Ok(entry);
        }
        self.entry_by_id(user_book_id)?
            .ok_or_else(|| Error::Decode(format!("entry {user_book_id} not found after {what}")))
    }

    fn progress_context(&self, id: UserBookId, read: Option<ReadId>) -> Result<ProgressContext> {
        #[derive(Deserialize)]
        struct Data {
            user_books: Vec<ProgressContext>,
        }
        let reads = match read {
            Some(r) => json!({ "id": { "_eq": r.get() } }),
            None => json!({ "finished_at": { "_is_null": true } }),
        };
        let data: Data = self.query(
            "ProgressContext",
            queries::PROGRESS_CONTEXT,
            json!({ "id": id.get(), "reads": reads }),
            QUERY_TIMEOUT,
        )?;
        data.user_books
            .into_iter()
            .next()
            .ok_or_else(|| Error::Api(format!("library entry {id} not found")))
    }

    /// Sets both dates of a read exactly: `None` clears that date.
    pub fn set_read_dates(
        &self,
        read: ReadId,
        started: Option<Date>,
        finished: Option<Date>,
    ) -> Result<LibraryEntry> {
        if let (Some(s), Some(f)) = (started, finished)
            && f < s
        {
            return Err(Error::InvalidInput(
                "finish date is before the start date".into(),
            ));
        }
        let object = json!({
            "started_at": started.map(|d| d.to_string()),
            "finished_at": finished.map(|d| d.to_string()),
        });
        let written = self.update_read(read, object)?;
        self.entry_after_read_write(written, "date update")
    }

    fn update_read(&self, read: ReadId, object: Value) -> Result<ReadRef> {
        #[derive(Deserialize)]
        struct Data {
            update_user_book_read: Option<ReadIdPayload>,
        }
        let data: Data = self.query(
            "UpdateUserBookRead",
            queries::UPDATE_USER_BOOK_READ,
            json!({ "id": read.get(), "read": object }),
            QUERY_TIMEOUT,
        )?;
        read_payload(data.update_user_book_read, "update_user_book_read")
    }

    fn insert_read(&self, id: UserBookId, object: Value) -> Result<ReadRef> {
        #[derive(Deserialize)]
        struct Data {
            insert_user_book_read: Option<ReadIdPayload>,
        }
        let data: Data = self.query(
            "InsertUserBookRead",
            queries::INSERT_USER_BOOK_READ,
            json!({ "userBookId": id.get(), "read": object }),
            QUERY_TIMEOUT,
        )?;
        read_payload(data.insert_user_book_read, "insert_user_book_read")
    }

    // ----- transport -----

    fn query<T: DeserializeOwned>(
        &self,
        op: &str,
        doc: &str,
        variables: Value,
        timeout: Duration,
    ) -> Result<T> {
        let body = json!({ "operationName": op, "query": doc, "variables": variables });
        let mut auth_retried = false;
        let mut rate_retried = false;
        loop {
            let token = self.inner.tokens.access_token()?;
            self.inner.bucket.acquire(1);
            let req = HttpRequest::new(Method::Post, &self.inner.endpoint)
                .header("authorization", format!("Bearer {token}"))
                .header("accept", "application/json")
                .json(&body)
                .timeout(timeout);
            let resp = self.inner.http.send(req)?;
            if resp.status == 401 && !auth_retried && self.inner.tokens.on_unauthorized(&token)? {
                auth_retried = true;
                continue;
            }
            if resp.status == 429 {
                // Throttled requests are rejected before they run, so retrying is
                // safe even for mutations. Retry once if the wait is short (the
                // per-minute bucket); the daily limit's wait is hours.
                // A long wait (daily limit) is not fed to the bucket: callers
                // should fail fast with `RateLimited`, not block for hours.
                let wait = retry_after(&resp);
                let short = wait.filter(|w| *w <= MAX_AUTO_RETRY_AFTER);
                self.inner.bucket.penalize(short);
                if !rate_retried && short.is_some() {
                    rate_retried = true;
                    continue;
                }
            }
            return decode_graphql(&resp);
        }
    }
}

fn read_payload(payload: Option<ReadIdPayload>, field: &str) -> Result<ReadRef> {
    let p = payload.ok_or_else(|| Error::Api(format!("`{field}` returned null")))?;
    if let Some(e) = p.error.filter(|e| !e.is_empty()) {
        return Err(Error::Api(e));
    }
    match p.user_book_read {
        Some(r) => Ok(r),
        None => Err(Error::Decode(format!(
            "`{field}` returned no read (id {:?})",
            p.id
        ))),
    }
}

/// The `DatesReadInput` fields for a progress update.
fn progress_fields(
    progress: ProgressUpdate,
    ctx: Option<&ProgressContext>,
) -> Result<Map<String, Value>> {
    let mut m = Map::new();
    match progress {
        ProgressUpdate::Pages(p) => {
            m.insert("progress_pages".into(), json!(p));
        }
        ProgressUpdate::Seconds(s) => {
            m.insert("progress_seconds".into(), json!(s));
        }
        ProgressUpdate::Percent(pct) => {
            if !(0.0..=100.0).contains(&pct) {
                return Err(Error::InvalidInput(format!(
                    "progress {pct}% is outside 0..=100"
                )));
            }
            let (pages, seconds) = ctx.map(ProgressContext::lengths).unwrap_or_default();
            let fraction = f64::from(pct) / 100.0;
            if let Some(total) = pages {
                m.insert("progress_pages".into(), json!(scale(fraction, total)));
            } else if let Some(total) = seconds {
                m.insert("progress_seconds".into(), json!(scale(fraction, total)));
            } else {
                return Err(Error::InvalidInput(
                    "cannot convert a percentage: the edition's length is unknown; set pages instead".into(),
                ));
            }
        }
    }
    Ok(m)
}

fn scale(fraction: f64, total: u32) -> u32 {
    ((fraction * f64::from(total)).round() as u32).min(total)
}

fn retry_after(resp: &HttpResponse) -> Option<Duration> {
    resp.header("retry-after")
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(Duration::from_secs)
}

/// Maps an API response to data or a typed error.
fn decode_graphql<T: DeserializeOwned>(resp: &HttpResponse) -> Result<T> {
    let body: Option<Value> = serde_json::from_slice(&resp.body).ok();
    let message = || {
        body.as_ref()
            .and_then(error_message)
            .unwrap_or_else(|| snippet(&resp.body))
    };
    match resp.status {
        200..=299 => {}
        401 => return Err(Error::Unauthorized),
        403 => {
            let b = body.as_ref();
            if b.and_then(|b| b.get("error")).and_then(Value::as_str) == Some("insufficient_scope")
            {
                return Err(Error::InsufficientScope {
                    scope: b
                        .and_then(|b| b.get("scope"))
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                });
            }
            return Err(Error::Api(message()));
        }
        408 => return Err(Error::Timeout),
        429 => {
            return Err(Error::RateLimited {
                retry_after: retry_after(resp),
            });
        }
        500..=599 => {
            return Err(Error::Network(format!(
                "server error {}: {}",
                resp.status,
                message()
            )));
        }
        s => return Err(Error::Api(format!("HTTP {s}: {}", message()))),
    }
    let body = body.ok_or_else(|| Error::Decode(format!("not JSON: {}", snippet(&resp.body))))?;
    if let Some(errors) = body.get("errors").filter(|e| !e.is_null()) {
        let msg = graphql_errors(errors);
        if !msg.is_empty() {
            return Err(Error::Api(msg));
        }
    }
    let data = body
        .get("data")
        .filter(|d| !d.is_null())
        .ok_or_else(|| Error::Decode("response has no `data`".into()))?;
    T::deserialize(data).map_err(|e| Error::Decode(e.to_string()))
}

/// GraphQL `errors` is normally `[{message, extensions}]`; Hardcover's gateway
/// also sends `["request_exceeds_capacity"]`.
fn graphql_errors(errors: &Value) -> String {
    let list: Vec<String> = match errors {
        Value::Array(items) => items
            .iter()
            .filter_map(|e| match e {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o.get("message").and_then(Value::as_str).map(str::to_owned),
                _ => None,
            })
            .collect(),
        Value::String(s) => vec![s.clone()],
        _ => Vec::new(),
    };
    list.join("; ")
}

/// Error text from any of the API's error bodies:
/// `{error, error_description}`, `{error, message}`, `{errors: [...], message}`.
fn error_message(body: &Value) -> Option<String> {
    let s = |k: &str| {
        body.get(k)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let errors = body
        .get("errors")
        .map(graphql_errors)
        .filter(|s| !s.is_empty());
    let head = s("error").map(str::to_owned).or(errors);
    let detail = s("error_description").or_else(|| s("message"));
    match (head, detail) {
        (Some(h), Some(d)) => Some(format!("{h}: {d}")),
        (Some(h), None) => Some(h),
        (None, Some(d)) => Some(d.to_owned()),
        (None, None) => None,
    }
}

fn snippet(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    if text.is_empty() {
        return "empty response".into();
    }
    text.chars().take(200).collect()
}

#[cfg(test)]
mod tests;
