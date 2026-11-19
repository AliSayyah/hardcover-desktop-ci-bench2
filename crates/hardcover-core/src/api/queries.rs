//! Every GraphQL document the client sends.
//!
//! Each document has exactly one top-level field: Hardcover charges one rate-limit
//! token per top-level field and rejects requests with more fields than the burst.
//! The test at the bottom validates every document against `schema/hardcover.graphql`.

macro_rules! book_summary_fields {
    () => {
        "fragment BookSummaryFields on books {
  id title subtitle slug pages release_year rating users_count cached_image cached_contributors
}
"
    };
}

macro_rules! user_book_fields {
    () => {
        "fragment UserBookFields on user_books {
  id user_id status_id rating date_added updated_at
  book { ...BookSummaryFields }
  edition { id title pages audio_seconds edition_format cached_image }
  user_book_reads(order_by: {id: desc}, limit: 1) {
    id started_at finished_at progress progress_pages progress_seconds edition_id
  }
}
"
    };
}

pub(crate) const ME: &str = "query Me {
  me { id username name image { url width height } }
}
";

/// Keyset pagination over one user's shelves (status 1..=5; "Ignored" is left out).
pub(crate) const LIBRARY_PAGE: &str = concat!(
    "query LibraryPage($userId: Int!, $afterId: Int!, $limit: Int!) {
  user_books(
    where: {user_id: {_eq: $userId}, id: {_gt: $afterId}, status_id: {_in: [1, 2, 3, 4, 5]}}
    order_by: {id: asc}
    limit: $limit
  ) { ...UserBookFields }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

/// Not scoped to a user: used to re-read an entry right after mutating it.
pub(crate) const USER_BOOK_BY_ID: &str = concat!(
    "query UserBookById($id: Int!) {
  user_books_by_pk(id: $id) { ...UserBookFields }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

pub(crate) const USER_BOOK_FOR_USER: &str = concat!(
    "query UserBookForUser($userId: Int!, $id: Int!) {
  user_books(where: {id: {_eq: $id}, user_id: {_eq: $userId}}, limit: 1) { ...UserBookFields }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

pub(crate) const USER_BOOK_FOR_BOOK: &str = concat!(
    "query UserBookForBook($userId: Int!, $bookId: Int!) {
  user_books(where: {user_id: {_eq: $userId}, book_id: {_eq: $bookId}}, order_by: {id: asc}, limit: 1) {
    ...UserBookFields
  }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

pub(crate) const SEARCH_BOOKS: &str =
    "query SearchBooks($query: String!, $page: Int!, $perPage: Int!) {
  search(query: $query, query_type: \"book\", page: $page, per_page: $perPage) {
    error page per_page results
  }
}
";

pub(crate) const BOOK_DETAILS: &str = concat!(
    "query BookDetails($id: Int!) {
  books_by_pk(id: $id) {
    ...BookSummaryFields
    description release_date editions_count ratings_count cached_tags
    book_series(order_by: [{featured: desc}, {position: asc_nulls_last}]) {
      position featured series { name slug }
    }
  }
}
",
    book_summary_fields!()
);

/// Lengths needed to turn a percentage into pages/seconds, plus the read to update.
/// `$reads` selects either one read by id or the unfinished read.
pub(crate) const PROGRESS_CONTEXT: &str =
    "query ProgressContext($id: Int!, $reads: user_book_reads_bool_exp!) {
  user_books(where: {id: {_eq: $id}}, limit: 1) {
    id status_id
    book { pages audio_seconds }
    edition { pages audio_seconds }
    user_book_reads(where: $reads, order_by: {id: desc}, limit: 1) {
      id edition { pages audio_seconds }
    }
  }
}
";

pub(crate) const INSERT_USER_BOOK: &str = concat!(
    "mutation InsertUserBook($object: UserBookCreateInput!) {
  insert_user_book(object: $object) { error id user_book { ...UserBookFields } }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

pub(crate) const UPDATE_USER_BOOK: &str = concat!(
    "mutation UpdateUserBook($id: Int!, $object: UserBookUpdateInput!) {
  update_user_book(id: $id, object: $object) { error id user_book { ...UserBookFields } }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

pub(crate) const DELETE_USER_BOOK: &str = "mutation DeleteUserBook($id: Int!) {
  delete_user_book(id: $id) { id book_id user_id }
}
";

/// Returns the written read and, nested, the whole entry as of this write:
/// reading the entry back in a separate request right afterwards can be stale.
pub(crate) const INSERT_USER_BOOK_READ: &str = concat!(
    "mutation InsertUserBookRead($userBookId: Int!, $read: DatesReadInput!) {
  insert_user_book_read(user_book_id: $userBookId, user_book_read: $read) {
    error id
    user_book_read {
      id user_book_id started_at finished_at progress progress_pages progress_seconds edition_id
      user_book { ...UserBookFields }
    }
  }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

/// Returns the written read and, nested, the whole entry as of this write:
/// reading the entry back in a separate request right afterwards can be stale.
pub(crate) const UPDATE_USER_BOOK_READ: &str = concat!(
    "mutation UpdateUserBookRead($id: Int!, $read: DatesReadInput!) {
  update_user_book_read(id: $id, object: $read) {
    error id
    user_book_read {
      id user_book_id started_at finished_at progress progress_pages progress_seconds edition_id
      user_book { ...UserBookFields }
    }
  }
}
",
    user_book_fields!(),
    book_summary_fields!()
);

/// Every document the client sends, by name. Validated against
/// `schema/hardcover.graphql` in the tests below and against the live schema by
/// the `api_check` example.
pub(crate) const ALL: &[(&str, &str)] = &[
    ("ME", ME),
    ("LIBRARY_PAGE", LIBRARY_PAGE),
    ("USER_BOOK_BY_ID", USER_BOOK_BY_ID),
    ("USER_BOOK_FOR_USER", USER_BOOK_FOR_USER),
    ("USER_BOOK_FOR_BOOK", USER_BOOK_FOR_BOOK),
    ("SEARCH_BOOKS", SEARCH_BOOKS),
    ("BOOK_DETAILS", BOOK_DETAILS),
    ("PROGRESS_CONTEXT", PROGRESS_CONTEXT),
    ("INSERT_USER_BOOK", INSERT_USER_BOOK),
    ("UPDATE_USER_BOOK", UPDATE_USER_BOOK),
    ("DELETE_USER_BOOK", DELETE_USER_BOOK),
    ("INSERT_USER_BOOK_READ", INSERT_USER_BOOK_READ),
    ("UPDATE_USER_BOOK_READ", UPDATE_USER_BOOK_READ),
];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::sdl;

    #[test]
    fn every_document_validates_against_the_schema() {
        let schema = sdl::schema();
        let mut failures = Vec::new();
        for (name, doc) in ALL {
            if let Err(e) = sdl::validate(schema, name, doc) {
                failures.push(format!("{name}:\n{e}"));
            }
        }
        assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    }

    #[test]
    fn validator_rejects_bad_documents() {
        // Guards against the validation test silently passing everything.
        let schema = sdl::schema();
        assert!(sdl::validate(schema, "bad_field", "query { me { id nope } }").is_err());
        assert!(
            sdl::validate(
                schema,
                "bad_type",
                "mutation($o: UserBookInput!) { insert_user_book(object: $o) { id } }"
            )
            .is_err()
        );
        assert!(
            sdl::validate(
                schema,
                "no_such_root",
                "query { user_books_by_slug(id: 1) { id } }"
            )
            .is_err()
        );
    }

    #[test]
    fn every_document_has_one_top_level_field() {
        use apollo_compiler::ExecutableDocument;
        let schema = sdl::schema();
        for (name, doc) in ALL {
            let doc = ExecutableDocument::parse_and_validate(schema, *doc, *name).unwrap();
            let op = doc.operations.iter().next().unwrap();
            assert_eq!(
                op.selection_set.selections.len(),
                1,
                "{name} has more than one top-level field"
            );
        }
    }
}
