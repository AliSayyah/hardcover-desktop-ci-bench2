//! One book in a collection list: cover thumbnail, title, authors and a fixed
//! trailing lane. Shared by the library shelves and search results.

use gpui_kit::component::tag::Tag;
use gpui_kit::component::{ActiveTheme as _, Icon, IconName, Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, IntoElement, ObjectFit, ParentElement as _, RenderOnce, SharedString, Styled as _,
    StyledImage as _, Window, div, img, prelude::FluentBuilder as _,
};
use hardcover_core::{BookSummary, LibraryEntry, ReadingStatus};

use crate::app_state::cover_thumbnail;
use crate::format::{authors_text, progress_summary, rating_text, status_label};

/// What the trailing lane shows.
#[derive(Clone)]
pub enum Trailing {
    None,
    /// Reading progress, e.g. "42%".
    Progress(SharedString),
    /// The user's rating.
    Rating(SharedString),
    /// Library status of a search result.
    Status(SharedString),
}

#[derive(IntoElement)]
pub struct BookRow {
    title: SharedString,
    secondary: SharedString,
    cover_url: Option<String>,
    trailing: Trailing,
}

impl BookRow {
    /// A row on a library shelf.
    pub fn for_entry(entry: &LibraryEntry) -> Self {
        let trailing = match entry.status {
            ReadingStatus::CurrentlyReading => progress_summary(entry)
                .short_text()
                .map_or(Trailing::None, |t| Trailing::Progress(t.into())),
            ReadingStatus::Read => entry
                .rating
                .map_or(Trailing::None, |r| Trailing::Rating(rating_text(r).into())),
            _ => Trailing::None,
        };
        Self {
            title: entry.book.title.clone().into(),
            secondary: authors_text(&entry.book.authors).into(),
            cover_url: entry.cover().map(|c| c.url.clone()),
            trailing,
        }
    }

    /// A search result, with its library status when the book is shelved.
    pub fn for_search_result(book: &BookSummary, status: Option<ReadingStatus>) -> Self {
        let mut secondary = authors_text(&book.authors);
        if let Some(year) = book.release_year {
            secondary.push_str(&format!(" · {year}"));
        }
        Self {
            title: book.title.clone().into(),
            secondary: secondary.into(),
            cover_url: book.cover.as_ref().map(|c| c.url.clone()),
            trailing: status.map_or(Trailing::None, |s| Trailing::Status(status_label(s).into())),
        }
    }
}

impl RenderOnce for BookRow {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let thumbnail = self
            .cover_url
            .as_deref()
            .and_then(|url| cover_thumbnail(url, window, cx));
        let muted = cx.theme().muted_foreground;
        h_flex()
            .w_full()
            .gap_3()
            .py_0p5()
            .child(
                // Fixed 2:3 slot so rows align whether or not the cover has loaded.
                div()
                    .flex_shrink_0()
                    .w_8()
                    .h_12()
                    .rounded(cx.theme().radius)
                    .overflow_hidden()
                    .bg(cx.theme().muted)
                    .flex()
                    .items_center()
                    .justify_center()
                    .map(|this| match thumbnail {
                        Some(image) => {
                            this.child(img(image).size_full().object_fit(ObjectFit::Cover))
                        }
                        None => this.child(Icon::new(IconName::BookOpen).small().text_color(muted)),
                    }),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_0p5()
                    .child(
                        div()
                            .text_sm()
                            .font_weight(gpui_kit::FontWeight::MEDIUM)
                            .truncate()
                            .child(self.title),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .truncate()
                            .child(self.secondary),
                    ),
            )
            .map(|this| match self.trailing {
                Trailing::None => this.child(div().flex_shrink_0().w_12()),
                Trailing::Progress(text) => this.child(
                    div()
                        .flex_shrink_0()
                        .w_12()
                        .text_xs()
                        .text_right()
                        .text_color(muted)
                        .child(text),
                ),
                Trailing::Rating(text) => this.child(
                    h_flex()
                        .flex_shrink_0()
                        .w_12()
                        .justify_end()
                        .gap_1()
                        .text_xs()
                        .text_color(muted)
                        .child(Icon::new(IconName::StarFill).xsmall())
                        .child(text),
                ),
                Trailing::Status(text) => this.child(
                    div()
                        .flex_shrink_0()
                        .child(Tag::secondary().small().child(text)),
                ),
            })
    }
}
