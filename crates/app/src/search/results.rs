//! Search results as a plain state machine: the current query and its
//! revision, the pages received so far (deduplicated by book id) and whether
//! more can be loaded.

use std::collections::HashSet;

use hardcover_core::{BookId, BookSummary, SearchPage};

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Phase {
    /// No query.
    #[default]
    Idle,
    /// Waiting for the first page.
    Searching,
    Loaded,
    /// Waiting for a further page; earlier pages stay visible.
    LoadingMore,
    Failed(String),
}

#[derive(Debug, Default)]
pub struct SearchResults {
    query: String,
    revision: u64,
    books: Vec<BookSummary>,
    seen: HashSet<BookId>,
    page: u32,
    total: Option<u32>,
    has_more: bool,
    phase: Phase,
}

impl SearchResults {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn books(&self) -> &[BookSummary] {
        &self.books
    }

    pub fn total(&self) -> Option<u32> {
        self.total
    }

    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    pub fn has_more(&self) -> bool {
        self.has_more && self.phase == Phase::Loaded
    }

    /// Starts a new query, dropping the old results. Returns the revision a
    /// response must carry to be accepted, or `None` for a blank query.
    pub fn start(&mut self, query: String) -> Option<u64> {
        self.revision += 1;
        self.books.clear();
        self.seen.clear();
        self.page = 0;
        self.total = None;
        self.has_more = false;
        if query.trim().is_empty() {
            self.query.clear();
            self.phase = Phase::Idle;
            return None;
        }
        self.query = query;
        self.phase = Phase::Searching;
        Some(self.revision)
    }

    /// The next page to request, marking it as loading. `None` if there is no
    /// further page or a request is already running.
    pub fn next_page_request(&mut self) -> Option<(u64, String, u32)> {
        if !self.has_more() {
            return None;
        }
        self.phase = Phase::LoadingMore;
        Some((self.revision, self.query.clone(), self.page + 1))
    }

    /// Applies a response. Returns `false` (and changes nothing) if it belongs
    /// to an older query.
    pub fn receive(
        &mut self,
        revision: u64,
        result: Result<SearchPage, impl Into<String>>,
    ) -> bool {
        if revision != self.revision {
            return false;
        }
        match result {
            Ok(page) => {
                let fresh = page.books.len();
                self.has_more = page.has_more() && fresh > 0;
                self.page = page.page;
                self.total = page.total.or(self.total);
                for book in page.books {
                    if self.seen.insert(book.id) {
                        self.books.push(book);
                    }
                }
                self.phase = Phase::Loaded;
            }
            Err(message) => {
                // No automatic retry of a failed page; a new query starts over.
                self.has_more = false;
                self.phase = Phase::Failed(message.into());
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(ids: &[i64], page_no: u32, total: u32) -> SearchPage {
        let books = ids
            .iter()
            .map(|&id| BookSummary::new(BookId::new(id), format!("Book {id}")))
            .collect();
        let mut p = SearchPage::new(books, page_no, 2);
        p.total = Some(total);
        p
    }

    #[test]
    fn blank_query_is_idle_without_request() {
        let mut r = SearchResults::default();
        assert_eq!(r.start("   ".into()), None);
        assert_eq!(r.phase(), &Phase::Idle);
    }

    #[test]
    fn stale_responses_are_rejected() {
        let mut r = SearchResults::default();
        let old = r.start("dun".into()).unwrap();
        let new = r.start("dune".into()).unwrap();
        assert!(!r.receive(old, Ok::<_, String>(page(&[1, 2], 1, 10))));
        assert!(r.books().is_empty());
        assert_eq!(r.phase(), &Phase::Searching);
        assert!(r.receive(new, Ok::<_, String>(page(&[3, 4], 1, 10))));
        assert_eq!(r.books().len(), 2);
        assert_eq!(r.total(), Some(10));
    }

    #[test]
    fn pages_append_without_duplicates() {
        let mut r = SearchResults::default();
        let rev = r.start("dune".into()).unwrap();
        r.receive(rev, Ok::<_, String>(page(&[1, 2], 1, 3)));
        assert!(r.has_more());
        let (rev2, query, next) = r.next_page_request().unwrap();
        assert_eq!((rev2, query.as_str(), next), (rev, "dune", 2));
        // A second request while one runs is refused.
        assert!(r.next_page_request().is_none());
        r.receive(rev, Ok::<_, String>(page(&[2, 3], 2, 3)));
        let ids: Vec<i64> = r.books().iter().map(|b| b.id.get()).collect();
        assert_eq!(ids, [1, 2, 3]);
        assert!(!r.has_more());
    }

    #[test]
    fn failure_keeps_earlier_pages_and_stops_paging() {
        let mut r = SearchResults::default();
        let rev = r.start("dune".into()).unwrap();
        r.receive(rev, Ok::<_, String>(page(&[1, 2], 1, 9)));
        r.next_page_request().unwrap();
        r.receive(rev, Err("rate limited"));
        assert_eq!(r.books().len(), 2);
        assert_eq!(r.phase(), &Phase::Failed("rate limited".into()));
        assert!(!r.has_more());
    }
}
