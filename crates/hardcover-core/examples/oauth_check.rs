//! Verifies that the stored OAuth session's scopes cover everything the app
//! does: reads, search, details, and a full write cycle on a book that must
//! NOT already be in the library (it is added as Private and removed again).
//! Also forces one token refresh and checks the rotated token works.
//!
//! Prints only counts and public book data. Removing a book leaves reading
//! journal rows on Hardcover; clean those up separately with a token that has
//! journal scope.
//!
//! ```sh
//! cargo run -p hardcover-core --example oauth_check -- <book-id-not-in-library>
//! ```

use std::sync::Arc;

use hardcover_core::{
    BookId, Client, Privacy, ProgressUpdate, Rating, ReadingStatus, TokenSource,
    auth::{KeyringTokenStore, OAuthConfig, Session},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let book = BookId::new(std::env::args().nth(1).ok_or("pass a book id")?.parse()?);
    let session = Arc::new(
        Session::restore(OAuthConfig::default(), Arc::new(KeyringTokenStore::new()?))?
            .ok_or("no stored session")?,
    );
    println!("scope: {:?}", session.scope());
    let client = Client::new(session.clone());

    let me = client.me()?;
    let library = client.library(me.id)?;
    println!("library entries: {}", library.len());
    let search = client.search_books("dune", 1)?;
    println!("search hits on page 1: {}", search.books.len());
    let details = client.book_details(book)?;
    println!("details: {}", details.summary.title);

    if client.library_entry_for_book(me.id, book)?.is_some() {
        return Err("book is already in the library; pick another".into());
    }
    let cycle = || -> hardcover_core::Result<()> {
        let e = client.add_to_library_with_privacy(
            book,
            ReadingStatus::WantToRead,
            Privacy::Private,
        )?;
        println!("added: {:?}", e.status);
        let e = client.set_status(e.user_book_id, ReadingStatus::CurrentlyReading)?;
        println!("status: {:?}", e.status);
        let e = client.set_rating(e.user_book_id, Rating::new(4.0), e.status)?;
        println!("rating: {:?} status kept: {:?}", e.rating, e.status);
        let read = e.current_read.as_ref().map(|r| r.id);
        let e = client.update_progress(e.user_book_id, read, ProgressUpdate::Pages(10))?;
        println!(
            "progress pages: {:?}",
            e.current_read.as_ref().and_then(|r| r.progress_pages)
        );
        let read = e
            .current_read
            .as_ref()
            .ok_or(hardcover_core::Error::Api("no read".into()))?;
        let started = chrono::Local::now().date_naive();
        let e = client.set_read_dates(read.id, Some(started), None)?;
        println!(
            "started: {:?}",
            e.current_read.as_ref().and_then(|r| r.started_at)
        );
        Ok(())
    };
    let result = cycle();
    // Always remove the test book, even when the cycle failed part-way.
    if let Some(entry) = client.library_entry_for_book(me.id, book)? {
        client.remove_from_library(entry.user_book_id)?;
    }
    println!(
        "removed; still in library: {}",
        client.library_entry_for_book(me.id, book)?.is_some()
    );
    result?;

    let old = session.access_token()?;
    let refreshed = session.on_unauthorized(&old)?;
    let rotated = session.access_token()? != old;
    println!("refresh: {refreshed}, token rotated: {rotated}");
    println!("me after refresh: {}", client.me().is_ok());
    Ok(())
}
