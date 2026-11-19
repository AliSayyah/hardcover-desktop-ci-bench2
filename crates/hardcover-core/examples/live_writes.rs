//! Live write test against the real account. Every change is undone.
//!
//! ```sh
//! HARDCOVER_TOKEN=... HARDCOVER_LIVE_WRITES=yes \
//!   HARDCOVER_LIVE_PHASE=add-remove|full [HARDCOVER_TEST_BOOK_ID=473869] \
//!   cargo run -p hardcover-core --example live_writes
//! ```
//!
//! Uses one public book that must NOT be in the library (any status). The entry
//! is added as Private. A guard armed before the first write (and run on panic
//! too) deletes every `user_books` row for (user, book) and every reading-journal
//! and activity row for that book created during the run, then verifies the
//! library count, journal and activity rows are back to the baseline.
//!
//! Phases:
//! - `add-remove`: raw insert (reports whether the payload's nested `user_book`
//!   is populated and what a duplicate insert does), then `remove_from_library`.
//! - `full`: add, set_status, set_rating (set + clear), update_progress (open
//!   read + percent), set_read_dates, Read, remove.

use std::collections::BTreeSet;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

use hardcover_core::auth::StaticToken;
use hardcover_core::{
    BookId, Client, Date, Privacy, ProgressUpdate, Rating, ReadingStatus, UserBookId, UserId,
};
use serde_json::{Value, json};

const DEFAULT_BOOK: i64 = 473_869; // The Count of Monte Cristo (public catalog book)

type R<T> = Result<T, String>;

/// Direct GraphQL for baseline/cleanup, independent of the code under test.
struct Raw {
    agent: ureq::Agent,
    token: String,
}

impl Raw {
    fn q(&self, doc: &str, vars: Value) -> R<Value> {
        let mut last = String::new();
        for attempt in 0..4 {
            // Stay well under 60/min alongside the client's own limiter.
            std::thread::sleep(Duration::from_millis(2000 + attempt * 5000));
            let resp = self
                .agent
                .post("https://api.hardcover.app/v1/graphql")
                .header("authorization", &format!("Bearer {}", self.token))
                .header("user-agent", hardcover_core::USER_AGENT)
                .send_json(json!({ "query": doc, "variables": vars }));
            let mut resp = match resp {
                Ok(r) => r,
                Err(e) => {
                    last = e.to_string();
                    continue;
                }
            };
            let status = resp.status().as_u16();
            let mut body = String::new();
            let _ = resp.body_mut().as_reader().read_to_string(&mut body);
            if status == 429 || status >= 500 {
                last = format!("HTTP {status}");
                continue;
            }
            let v: Value =
                serde_json::from_str(&body).map_err(|e| format!("HTTP {status}: {e}"))?;
            if status != 200 || v.get("errors").is_some_and(|e| !e.is_null()) {
                return Err(format!(
                    "HTTP {status}: {}",
                    v.get("errors").or(v.get("error")).unwrap_or(&v)
                ));
            }
            return Ok(v["data"].clone());
        }
        Err(format!("giving up after retries: {last}"))
    }

    fn ids(&self, doc: &str, field: &str, vars: Value) -> R<BTreeSet<i64>> {
        let data = self.q(doc, vars)?;
        Ok(data[field]
            .as_array()
            .ok_or_else(|| format!("no {field} in {data}"))?
            .iter()
            .filter_map(|r| r["id"].as_i64())
            .collect())
    }
}

const COUNT: &str =
    "query($u: Int!) { user_books_aggregate(where: {user_id: {_eq: $u}}) { aggregate { count } } }";
const ROWS: &str = "query($u: Int!, $b: Int!) { user_books(where: {user_id: {_eq: $u}, book_id: {_eq: $b}}) { id status_id privacy_setting_id } }";
const JOURNALS: &str = "query($u: Int!, $b: Int!) { reading_journals(where: {user_id: {_eq: $u}, book_id: {_eq: $b}}) { id event } }";
const ACTIVITIES: &str = "query($u: Int!, $b: Int!) { activities(where: {user_id: {_eq: $u}, book_id: {_eq: $b}}) { id event privacy_setting_id } }";
const READS: &str =
    "query($ids: [Int!]!) { user_book_reads(where: {user_book_id: {_in: $ids}}) { id } }";
const ALL_READS: &str = "query($id: Int!) { user_book_reads(where: {user_book_id: {_eq: $id}}, order_by: {id: asc}) { id started_at finished_at progress progress_pages edition_id } user_books_by_pk(id: $id) { status_id edition_id } }";
const AGGREGATES: &str = "query($u: Int!) { users(where: {id: {_eq: $u}}) { books_count goals(order_by: {id: asc}) { id progress } } }";
const RAW_INSERT: &str = "mutation($o: UserBookCreateInput!) { insert_user_book(object: $o) { error id user_book { id status_id privacy_setting_id } } }";
const DELETE_USER_BOOK: &str = "mutation($id: Int!) { delete_user_book(id: $id) { id } }";
const DELETE_JOURNAL: &str = "mutation($id: Int!) { delete_reading_journal(id: $id) { id } }";
const DELETE_ACTIVITY: &str = "mutation($id: Int!) { delete_activities_by_pk(id: $id) { id } }";

struct Baseline {
    count: i64,
    aggregates: Value,
    journals: BTreeSet<i64>,
    activities: BTreeSet<i64>,
}

/// Restores (user, book) to the baseline when dropped, including on panic.
struct Guard<'a> {
    raw: &'a Raw,
    user: i64,
    book: i64,
    base: Baseline,
    seen_user_books: BTreeSet<i64>,
}

impl Guard<'_> {
    fn vars(&self) -> Value {
        json!({ "u": self.user, "b": self.book })
    }

    fn restore(&mut self) -> R<()> {
        let rows = self.raw.ids(ROWS, "user_books", self.vars())?;
        self.seen_user_books.extend(&rows);
        for id in &rows {
            println!("cleanup: deleting leftover library entry");
            self.raw.q(DELETE_USER_BOOK, json!({ "id": id }))?;
        }
        let data = self.raw.q(JOURNALS, self.vars())?;
        let journals: BTreeSet<i64> = data["reading_journals"]
            .as_array()
            .ok_or("no reading_journals")?
            .iter()
            .filter_map(|r| r["id"].as_i64())
            .collect();
        let events: Vec<&str> = data["reading_journals"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|r| {
                r["id"]
                    .as_i64()
                    .is_some_and(|id| !self.base.journals.contains(&id))
            })
            .filter_map(|r| r["event"].as_str())
            .collect();
        if !events.is_empty() {
            println!("cleanup: journal events left by the test: {events:?}");
        }
        for id in journals.difference(&self.base.journals) {
            println!("cleanup: deleting reading journal row created by the test");
            self.raw.q(DELETE_JOURNAL, json!({ "id": id }))?;
        }
        let activities = self.raw.ids(ACTIVITIES, "activities", self.vars())?;
        for id in activities.difference(&self.base.activities) {
            println!("cleanup: deleting activity row created by the test");
            self.raw.q(DELETE_ACTIVITY, json!({ "id": id }))?;
        }
        self.verify()
    }

    fn verify(&self) -> R<()> {
        let count = count(self.raw, self.user)?;
        let rows = self.raw.ids(ROWS, "user_books", self.vars())?;
        let journals = self.raw.ids(JOURNALS, "reading_journals", self.vars())?;
        let activities = self.raw.ids(ACTIVITIES, "activities", self.vars())?;
        let ids: Vec<i64> = self.seen_user_books.iter().copied().collect();
        let reads = self
            .raw
            .ids(READS, "user_book_reads", json!({ "ids": ids }))?;
        println!(
            "verify: library count {} (baseline {}), entries for book {}, new journal rows {}, new activity rows {}, leftover reads {}",
            count,
            self.base.count,
            rows.len(),
            journals.difference(&self.base.journals).count(),
            activities.difference(&self.base.activities).count(),
            reads.len()
        );
        let aggregates = self.raw.q(AGGREGATES, json!({ "u": self.user }))?;
        println!(
            "verify: books_count and reading-goal progress unchanged: {}",
            aggregates == self.base.aggregates
        );
        let ok = aggregates == self.base.aggregates
            && count == self.base.count
            && rows.is_empty()
            && journals == self.base.journals
            && activities == self.base.activities
            && reads.is_empty();
        if ok {
            Ok(())
        } else {
            Err("state differs from baseline".into())
        }
    }
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        match self.restore() {
            Ok(()) => println!("RESTORED: account matches the baseline"),
            Err(e) => println!(
                "RESTORE FAILED: {e} -- check user_books/reading_journals/activities for book {}",
                self.book
            ),
        }
    }
}

fn count(raw: &Raw, user: i64) -> R<i64> {
    let d = raw.q(COUNT, json!({ "u": user }))?;
    d["user_books_aggregate"]["aggregate"]["count"]
        .as_i64()
        .ok_or_else(|| format!("bad count: {d}"))
}

fn main() -> R<()> {
    if std::env::var("HARDCOVER_LIVE_WRITES").as_deref() != Ok("yes") {
        return Err("refusing to write: set HARDCOVER_LIVE_WRITES=yes".into());
    }
    let phase = std::env::var("HARDCOVER_LIVE_PHASE").unwrap_or_else(|_| "add-remove".into());
    let book: i64 = std::env::var("HARDCOVER_TEST_BOOK_ID")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BOOK);
    let token = StaticToken::from_env().ok_or("set HARDCOVER_TOKEN")?;
    let raw = Raw {
        agent: ureq::Agent::new_with_defaults(),
        token: hardcover_core::TokenSource::access_token(&token).map_err(|e| e.to_string())?,
    };
    let client = Client::new(Arc::new(token));
    let me = client.me().map_err(|e| e.to_string())?;
    let user = me.id.get();

    // ----- baseline -----
    let vars = json!({ "u": user, "b": book });
    let existing = raw.ids(ROWS, "user_books", vars.clone())?;
    if !existing.is_empty() {
        return Err(format!(
            "book {book} is already in the library (any status); pick another"
        ));
    }
    if client
        .library_entry_for_book(me.id, BookId::new(book))
        .map_err(|e| e.to_string())?
        .is_some()
    {
        return Err("client sees an entry for the test book; aborting".into());
    }
    let base = Baseline {
        count: count(&raw, user)?,
        aggregates: raw.q(AGGREGATES, json!({ "u": user }))?,
        journals: raw.ids(JOURNALS, "reading_journals", vars.clone())?,
        activities: raw.ids(ACTIVITIES, "activities", vars.clone())?,
    };
    println!(
        "baseline: library count {}, journal rows for book {}, activity rows for book {}",
        base.count,
        base.journals.len(),
        base.activities.len()
    );
    let mut guard = Guard {
        raw: &raw,
        user,
        book,
        base,
        seen_user_books: BTreeSet::new(),
    };

    let result = match phase.as_str() {
        "add-remove" => add_remove(&raw, &client, &mut guard, book),
        "full" => full(&raw, &client, &mut guard, me.id, book),
        other => Err(format!("unknown phase {other}")),
    };
    if let Err(e) = &result {
        println!("TEST STEP FAILED: {e}");
    }
    drop(guard); // restore + verify
    result
}

fn add_remove(raw: &Raw, client: &Client, guard: &mut Guard<'_>, book: i64) -> R<()> {
    let obj = json!({ "o": { "book_id": book, "status_id": 1, "privacy_setting_id": 3 } });
    let d = raw.q(RAW_INSERT, obj.clone())?;
    let p = &d["insert_user_book"];
    println!(
        "insert_user_book: error={} id_present={} nested_user_book_present={} status={} privacy={}",
        p["error"],
        !p["id"].is_null(),
        !p["user_book"].is_null(),
        p["user_book"]["status_id"],
        p["user_book"]["privacy_setting_id"]
    );
    let id = p["id"].as_i64().ok_or("insert returned no id")?;
    guard.seen_user_books.insert(id);

    match raw.q(RAW_INSERT, obj) {
        Ok(d2) => {
            let p2 = &d2["insert_user_book"];
            if let Some(id2) = p2["id"].as_i64() {
                guard.seen_user_books.insert(id2);
            }
            println!(
                "duplicate insert: error={} same_id={}",
                p2["error"],
                p2["id"].as_i64() == Some(id)
            );
        }
        Err(e) => println!("duplicate insert: request error {e}"),
    }
    let rows = raw.ids(ROWS, "user_books", json!({ "u": guard.user, "b": book }))?;
    println!("rows for book after duplicate insert: {}", rows.len());
    guard.seen_user_books.extend(&rows);

    for id in rows {
        client
            .remove_from_library(UserBookId::new(id))
            .map_err(|e| format!("remove_from_library: {e}"))?;
    }
    println!("remove_from_library: ok");
    Ok(())
}

/// Prints every read of the test entry (relative ids, dates and progress only).
fn dump(raw: &Raw, id: i64, label: &str) {
    match raw.q(ALL_READS, json!({ "id": id })) {
        Ok(d) => {
            let reads = d["user_book_reads"].as_array().cloned().unwrap_or_default();
            let base = reads.first().and_then(|r| r["id"].as_i64()).unwrap_or(0);
            let desc: Vec<String> = reads
                .iter()
                .map(|r| {
                    format!(
                        "#{}(start={} fin={} pages={} pct={} ed={})",
                        r["id"].as_i64().unwrap_or(0) - base,
                        r["started_at"],
                        r["finished_at"],
                        r["progress_pages"],
                        r["progress"],
                        !r["edition_id"].is_null()
                    )
                })
                .collect();
            println!(
                "  [{label}] status={} reads={}",
                d["user_books_by_pk"]["status_id"],
                desc.join(" ")
            );
        }
        Err(e) => println!("  [{label}] dump failed: {e}"),
    }
}

fn full(raw: &Raw, client: &Client, guard: &mut Guard<'_>, user: UserId, book: i64) -> R<()> {
    let e = client
        .add_to_library_with_privacy(
            BookId::new(book),
            ReadingStatus::WantToRead,
            Privacy::Private,
        )
        .map_err(|e| format!("add_to_library: {e}"))?;
    guard.seen_user_books.insert(e.user_book_id.get());
    let id = e.user_book_id;
    let ub = id.get();
    println!(
        "add_to_library: status={} read={}",
        e.status,
        e.current_read.is_some()
    );
    dump(raw, ub, "after add");
    let check = |label: &str,
                 r: hardcover_core::Result<hardcover_core::LibraryEntry>|
     -> R<hardcover_core::LibraryEntry> { r.map_err(|e| format!("{label}: {e}")) };

    let e = check(
        "update_progress(None, Pages(5)) without a read",
        client.update_progress(id, None, ProgressUpdate::Pages(5)),
    )?;
    println!(
        "update_progress with no read: status={} read_present={} pages={:?}",
        e.status,
        e.current_read.is_some(),
        e.current_read.as_ref().and_then(|r| r.progress_pages)
    );
    dump(raw, ub, "after progress without read");

    let e = check(
        "set_status(CurrentlyReading)",
        client.set_status(id, ReadingStatus::CurrentlyReading),
    )?;
    println!("set_status CurrentlyReading: status={}", e.status);
    dump(raw, ub, "after set_status(2)");
    let r = e.current_read.clone().ok_or("no read after set_status")?;

    let e = check(
        "update_progress(Some, Pages(10))",
        client.update_progress(id, Some(r.id), ProgressUpdate::Pages(10)),
    )?;
    let r = e.current_read.clone().ok_or("no read after progress")?;
    println!(
        "update_progress(Some, pages=10): pages={:?} status={}",
        r.progress_pages, e.status
    );
    dump(raw, ub, "after progress pages");

    let e = check(
        "update_progress(Some, Percent(50))",
        client.update_progress(id, Some(r.id), ProgressUpdate::Percent(50.0)),
    )?;
    let r2 = e
        .current_read
        .clone()
        .ok_or("no read after percent progress")?;
    println!(
        "update_progress(Some, 50%): pages={:?} percent={:?} status={}",
        r2.progress_pages, r2.progress_percent, e.status
    );
    dump(raw, ub, "after progress 50%");

    let start = Date::from_ymd_opt(2026, 1, 2).unwrap();
    let e = check(
        "set_read_dates",
        client.set_read_dates(r2.id, Some(start), None),
    )?;
    let r3 = e.current_read.clone().ok_or("no read after dates")?;
    println!(
        "set_read_dates(2026-01-02, None): started_at={:?} finished_at={:?} pages_kept={:?} status={}",
        r3.started_at, r3.finished_at, r3.progress_pages, e.status
    );
    dump(raw, ub, "after set_read_dates");

    let e = check(
        "set_rating(4.5)",
        client.set_rating(id, Rating::new(4.5), ReadingStatus::CurrentlyReading),
    )?;
    println!(
        "set_rating 4.5: rating={:?} status={}",
        e.rating.map(|r| r.value()),
        e.status
    );
    dump(raw, ub, "after set_rating(4.5)");
    let e = check(
        "set_rating(None)",
        client.set_rating(id, None, ReadingStatus::CurrentlyReading),
    )?;
    println!(
        "set_rating None: rating={:?} status={}",
        e.rating.map(|r| r.value()),
        e.status
    );
    dump(raw, ub, "after set_rating(None)");

    let e = check(
        "set_status(Read)",
        client.set_status(id, ReadingStatus::Read),
    )?;
    println!(
        "set_status Read: status={} latest_read_finished={}",
        e.status,
        e.current_read
            .as_ref()
            .is_some_and(|r| r.finished_at.is_some())
    );
    dump(raw, ub, "after set_status(3)");

    let fin_read = e.current_read.clone().ok_or("no read after Read")?;
    let start = Date::from_ymd_opt(2026, 1, 2).unwrap();
    let finish = Date::from_ymd_opt(2026, 1, 20).unwrap();
    let e = check(
        "set_read_dates(start, finish)",
        client.set_read_dates(fin_read.id, Some(start), Some(finish)),
    )?;
    println!(
        "set_read_dates(2026-01-02, 2026-01-20): started={:?} finished={:?} status={}",
        e.current_read.as_ref().and_then(|r| r.started_at),
        e.current_read.as_ref().and_then(|r| r.finished_at),
        e.status
    );
    dump(raw, ub, "after set_read_dates with finish");
    let e = check(
        "set_rating(4.0, Read)",
        client.set_rating(id, Rating::new(4.0), ReadingStatus::Read),
    )?;
    println!(
        "set_rating 4.0 on Read entry: rating={:?} status={} finished={:?}",
        e.rating.map(|r| r.value()),
        e.status,
        e.current_read.as_ref().and_then(|r| r.finished_at)
    );
    dump(raw, ub, "after set_rating on Read");

    let looked_up = client
        .library_entry_for_book(user, BookId::new(book))
        .map_err(|e| e.to_string())?;
    println!("library_entry_for_book: found={}", looked_up.is_some());
    client
        .remove_from_library(id)
        .map_err(|e| format!("remove_from_library: {e}"))?;
    let after = client
        .library_entry_for_book(user, BookId::new(book))
        .map_err(|e| e.to_string())?;
    println!("remove_from_library: gone={}", after.is_none());
    Ok(())
}
