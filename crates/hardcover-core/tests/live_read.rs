//! Read-only checks against the live API. Ignored by default:
//!
//! ```sh
//! HARDCOVER_TOKEN=... cargo test -p hardcover-core --test live_read -- --ignored
//! ```

use std::sync::Arc;

use hardcover_core::auth::StaticToken;
use hardcover_core::{BookId, Client};

fn client() -> Option<Client> {
    let token = StaticToken::from_env()?;
    Some(Client::new(Arc::new(token)))
}

#[test]
#[ignore = "needs HARDCOVER_TOKEN and network"]
fn me_library_and_lookups() {
    let Some(c) = client() else {
        eprintln!("HARDCOVER_TOKEN not set; skipping");
        return;
    };
    let me = c.me().unwrap();
    let lib = c.library(me.id).unwrap();
    assert!(
        lib.iter()
            .all(|e| e.user_id == me.id && e.status.is_shelf())
    );
    let mut ids: Vec<_> = lib.iter().map(|e| e.user_book_id).collect();
    ids.dedup();
    assert_eq!(
        ids.len(),
        lib.len(),
        "keyset pagination must not repeat entries"
    );
    if let Some(first) = lib.first() {
        assert_eq!(
            c.library_entry(first.user_book_id).unwrap().as_ref(),
            Some(first)
        );
        assert!(
            c.library_entry_for_book(me.id, first.book.id)
                .unwrap()
                .is_some()
        );
    }
}

#[test]
#[ignore = "needs HARDCOVER_TOKEN and network"]
fn search_and_details() {
    let Some(c) = client() else {
        eprintln!("HARDCOVER_TOKEN not set; skipping");
        return;
    };
    let page = c.search_books("dune", 1).unwrap();
    assert!(page.total.unwrap_or(0) > 0);
    assert!(!page.books.is_empty());
    let top = &page.books[0];
    assert!(!top.title.is_empty() && !top.authors.is_empty());
    let d = c.book_details(top.id).unwrap();
    assert_eq!(d.summary.id, top.id);
    assert!(c.book_details(BookId::new(i64::from(i32::MAX))).is_err());
}
