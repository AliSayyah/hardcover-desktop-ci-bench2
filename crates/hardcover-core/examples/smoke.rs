//! Read-only smoke check against the live API.
//!
//! ```sh
//! HARDCOVER_TOKEN=... cargo run -p hardcover-core --example smoke
//! ```
//!
//! Prints only aggregate counts and public book titles.

use std::collections::BTreeMap;
use std::sync::Arc;

use hardcover_core::auth::{OAuthEndpoints, StaticToken};
use hardcover_core::covers::CoverCache;
use hardcover_core::{Client, ReadingStatus};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let token = StaticToken::from_env().ok_or("set HARDCOVER_TOKEN to a personal access token")?;
    let client = Client::new(Arc::new(token));

    let me = client.me()?;
    println!("signed in: ok");

    let mut pages = 0;
    let library = client.library_with_progress(me.id, |_| pages += 1)?;
    let mut counts: BTreeMap<i64, (ReadingStatus, usize)> = BTreeMap::new();
    for e in &library {
        counts.entry(e.status.id()).or_insert((e.status, 0)).1 += 1;
    }
    println!("library: {} entries in {pages} page(s)", library.len());
    for (status, n) in counts.values() {
        println!("  {status:<18} {n}");
    }
    let with_read = library.iter().filter(|e| e.current_read.is_some()).count();
    let with_cover = library.iter().filter(|e| e.cover().is_some()).count();
    println!("  entries with a read: {with_read}, with a cover: {with_cover}");

    if let Some(first) = library.first() {
        let by_id = client.library_entry(first.user_book_id)?;
        let by_book = client.library_entry_for_book(me.id, first.book.id)?;
        println!(
            "lookups: by id {}, by book {}",
            if by_id.as_ref() == Some(first) {
                "matches"
            } else {
                "MISMATCH"
            },
            if by_book.is_some() {
                "found"
            } else {
                "MISSING"
            }
        );
    }

    let page = client.search_books("dune", 1)?;
    println!("search \"dune\": {} total", page.total.unwrap_or(0));
    for b in page.books.iter().take(3) {
        println!("  {} ({})", b.title, b.authors.join(", "));
    }

    if let Some(top) = page.books.first() {
        let d = client.book_details(top.id)?;
        println!(
            "details for \"{}\": {} series, {} genres, {} editions, description {} chars",
            d.summary.title,
            d.series.len(),
            d.genres.len(),
            d.editions_count.unwrap_or(0),
            d.description.as_deref().map_or(0, str::len)
        );
        if let Some(cover) = &top.cover {
            let dir = std::env::temp_dir().join(format!("hardcover-smoke-{}", std::process::id()));
            let cache = CoverCache::new(&dir)?;
            let path = cache.fetch(cover)?;
            println!("cover: {} bytes", std::fs::metadata(&path)?.len());
            std::fs::remove_dir_all(&dir)?;
        }
    }

    let discovered = OAuthEndpoints::discover()?;
    println!(
        "oauth discovery matches built-in endpoints: {}",
        discovered == OAuthEndpoints::default()
    );
    Ok(())
}
