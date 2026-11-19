//! The book detail pane, shared by the library and search destinations.
//!
//! Shows a library entry (with the "your book" controls) or a book that is not
//! in the library (with `Add to library`). Controls reflect the entry in the
//! [`LibraryModel`]; a write disables them, and on success the returned entry
//! replaces the model's. On failure a notification explains it and the controls
//! are reset to the model's (unchanged) values.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::component::button::{Button, ButtonVariant, ButtonVariants as _};
use gpui_kit::component::calendar::Date as CalendarDate;
use gpui_kit::component::date_picker::{DatePicker, DatePickerEvent, DatePickerState};
use gpui_kit::component::form::{Field, Form};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::link::Link;
use gpui_kit::component::menu::{DropdownMenu as _, PopupMenuItem};
use gpui_kit::component::notification::Notification;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::skeleton::Skeleton;
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, IndexPath, Sizable as _, StyledExt as _,
    WindowExt as _, h_flex, v_flex,
};
use gpui_kit::{
    AnyElement, App, AppContext as _, Axis, Context, Entity, FontWeight, InteractiveElement as _,
    IntoElement, ObjectFit, ParentElement as _, Pixels, Render, RenderImage, SharedString,
    Styled as _, StyledImage as _, Subscription, Task, Window, div, img,
    prelude::FluentBuilder as _, rems,
};
use hardcover_core::{
    BookDetails, BookId, BookSummary, Client, Date, Error, LibraryEntry, ProgressUpdate, Rating,
    ReadingStatus, UserBookId,
};

use super::star_rating::StarRating;
use crate::app_state::{Services, cover_image, release_cover_image};
use crate::format::{
    authors_text, average_rating_text, error_message, progress_summary, series_position_text,
    status_label,
};
use crate::library::model::{LibraryModel, WriteKind};

/// What the pane shows.
#[derive(Debug, Clone, PartialEq)]
pub enum Subject {
    /// An entry in the library, by id (it follows status changes).
    Entry(UserBookId),
    /// A book from search; shown as an entry if it is in the library.
    Book(BookSummary),
}

/// A status in the status controls.
#[derive(Clone)]
pub struct StatusOption {
    status: ReadingStatus,
    label: SharedString,
}

impl SelectItem for StatusOption {
    type Value = ReadingStatus;

    fn title(&self) -> SharedString {
        self.label.clone()
    }

    fn value(&self) -> &ReadingStatus {
        &self.status
    }
}

fn status_options() -> Vec<StatusOption> {
    ReadingStatus::SHELVES
        .into_iter()
        .map(|status| StatusOption {
            status,
            label: status_label(status).into(),
        })
        .collect()
}

/// The entry values the controls currently show.
#[derive(Debug, Clone, PartialEq)]
struct Shown {
    id: UserBookId,
    status: ReadingStatus,
    page: Option<u32>,
    started: Option<Date>,
    finished: Option<Date>,
}

impl Shown {
    fn of(entry: &LibraryEntry) -> Self {
        let read = entry.current_read.as_ref();
        Self {
            id: entry.user_book_id,
            status: entry.status,
            page: progress_summary(entry).page,
            started: read.and_then(|r| r.started_at),
            finished: read.and_then(|r| r.finished_at),
        }
    }
}

enum Details {
    Loaded(Arc<BookDetails>),
    Failed(SharedString),
}

/// Wait before fetching details so arrowing through a list does not spend the
/// rate limit on every book passed over.
const DETAILS_DELAY: Duration = Duration::from_millis(250);

pub struct BookDetailView {
    model: Entity<LibraryModel>,
    subject: Option<Subject>,
    details: HashMap<BookId, Details>,
    details_loading: Option<BookId>,
    _details_task: Option<Task<()>>,
    status: Entity<SelectState<Vec<StatusOption>>>,
    add_status: Entity<SelectState<Vec<StatusOption>>>,
    page: Entity<InputState>,
    page_error: Option<SharedString>,
    started: Entity<DatePickerState>,
    finished: Entity<DatePickerState>,
    shown: Option<Shown>,
    /// The cover URL last requested and its image once loaded, released when
    /// the pane moves to another cover.
    cover: Option<(String, Option<Arc<RenderImage>>)>,
    _subscriptions: Vec<Subscription>,
}

impl BookDetailView {
    pub fn new(model: Entity<LibraryModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let status = cx.new(|cx| SelectState::new(status_options(), None, window, cx));
        let add_status =
            cx.new(|cx| SelectState::new(status_options(), Some(IndexPath::new(0)), window, cx));
        let page = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Page")
                .validate(|text, _| text.chars().all(|c| c.is_ascii_digit()) && text.len() <= 6)
        });
        let started = cx.new(|cx| DatePickerState::new(window, cx).date_format("%b %-d, %Y"));
        let finished = cx.new(|cx| DatePickerState::new(window, cx).date_format("%b %-d, %Y"));

        let subscriptions = vec![
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.sync_controls(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(
                &status,
                window,
                |this, _, event: &SelectEvent<Vec<StatusOption>>, window, cx| {
                    if let SelectEvent::Confirm(Some(status)) = event {
                        this.change_status(*status, window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &page,
                window,
                |this, _, event: &InputEvent, window, cx| match event {
                    InputEvent::PressEnter { .. } => this.submit_progress(window, cx),
                    InputEvent::Change if this.page_error.take().is_some() => cx.notify(),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &started,
                window,
                |this, _, event: &DatePickerEvent, window, cx| {
                    let DatePickerEvent::Change(value) = event;
                    this.change_dates(Some(value.date().start()), None, window, cx);
                },
            ),
            cx.subscribe_in(
                &finished,
                window,
                |this, _, event: &DatePickerEvent, window, cx| {
                    let DatePickerEvent::Change(value) = event;
                    this.change_dates(None, Some(value.date().start()), window, cx);
                },
            ),
        ];

        Self {
            model,
            subject: None,
            details: HashMap::new(),
            details_loading: None,
            _details_task: None,
            status,
            add_status,
            page,
            page_error: None,
            started,
            finished,
            shown: None,
            cover: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn set_subject(
        &mut self,
        subject: Option<Subject>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.subject == subject {
            return;
        }
        self.subject = subject;
        self.page_error = None;
        self.shown = None;
        self.sync_controls(window, cx);
        self.load_details(cx);
        cx.notify();
    }

    /// The library entry for the subject, if the book is in the library.
    fn entry<'a>(&self, model: &'a LibraryModel) -> Option<&'a LibraryEntry> {
        match self.subject.as_ref()? {
            Subject::Entry(id) => model.entry(*id),
            Subject::Book(book) => model.entry_for_book(book.id),
        }
    }

    fn book(&self, cx: &App) -> Option<BookSummary> {
        let model = self.model.read(cx);
        match self.subject.as_ref()? {
            Subject::Entry(id) => model.entry(*id).map(|e| e.book.clone()),
            Subject::Book(book) => Some(book.clone()),
        }
    }

    /// Points the controls at the entry's current values when they differ from
    /// what is shown. Never emits control events, so it cannot loop.
    fn sync_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(next) = self.entry(self.model.read(cx)).map(Shown::of) else {
            self.shown = None;
            return;
        };
        if self.shown.as_ref() == Some(&next) {
            return;
        }
        let previous = self.shown.replace(next.clone());
        self.status
            .update(cx, |s, cx| s.set_selected_value(&next.status, window, cx));
        let page_changed = previous
            .as_ref()
            .is_none_or(|p| p.id != next.id || p.page != next.page);
        if page_changed {
            let text = next.page.map(|p| p.to_string()).unwrap_or_default();
            self.page.update(cx, |s, cx| s.set_value(text, window, cx));
        }
        self.started.update(cx, |s, cx| {
            s.set_date(CalendarDate::Single(next.started), window, cx)
        });
        self.finished.update(cx, |s, cx| {
            s.set_date(CalendarDate::Single(next.finished), window, cx)
        });
    }

    /// Resets the controls to the model after a rejected or failed change.
    fn revert_controls(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.shown = None;
        self.sync_controls(window, cx);
        cx.notify();
    }

    // ----- details -----

    fn load_details(&mut self, cx: &mut Context<Self>) {
        let Some(book) = self.book(cx) else {
            return;
        };
        if matches!(self.details.get(&book.id), Some(Details::Loaded(_))) {
            return;
        }
        if self.details_loading == Some(book.id) {
            return;
        }
        self.details.remove(&book.id);
        self.details_loading = Some(book.id);
        let client = Services::global(cx).client().clone();
        let id = book.id;
        // Replacing the task cancels a fetch for a book no longer shown.
        self._details_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(DETAILS_DELAY).await;
            let result = cx
                .background_spawn(async move { client.book_details(id) })
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.details_loading == Some(id) {
                    this.details_loading = None;
                }
                let details = match result {
                    Ok(d) => Details::Loaded(Arc::new(d)),
                    Err(e) => Details::Failed(error_message(&e).into()),
                };
                this.details.insert(id, details);
                cx.notify();
            });
        }));
    }

    fn retry_details(&mut self, cx: &mut Context<Self>) {
        if let Some(book) = self.book(cx) {
            self.details.remove(&book.id);
        }
        self.details_loading = None;
        self.load_details(cx);
        cx.notify();
    }

    // ----- writes -----

    /// Runs one write on `id` in the background. The model applies the result
    /// whether or not this view still shows the book.
    fn run_write(
        &mut self,
        id: UserBookId,
        kind: WriteKind,
        failure_title: &'static str,
        op: impl FnOnce(&Client) -> Result<Option<LibraryEntry>, Error> + Send + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let model = self.model.clone();
        if !model.update(cx, |m, cx| m.begin_write(id, kind, cx)) {
            self.revert_controls(window, cx);
            return;
        }
        let client = Services::global(cx).client().clone();
        cx.spawn_in(window, async move |this, cx| {
            let result = cx.background_spawn(async move { op(&client) }).await;
            model.update(cx, |m, cx| {
                m.end_write(id, cx);
                match &result {
                    Ok(Some(entry)) => m.apply_written(entry.clone(), cx),
                    Ok(None) => m.apply_removed(id, cx),
                    Err(_) => {}
                }
            });
            if let Err(error) = result {
                let _ = this.update_in(cx, |this, window, cx| {
                    window.push_notification(
                        Notification::error(error_message(&error)).title(failure_title),
                        cx,
                    );
                    this.revert_controls(window, cx);
                });
            }
        })
        .detach();
    }

    fn current_entry(&self, cx: &App) -> Option<LibraryEntry> {
        self.entry(self.model.read(cx)).cloned()
    }

    fn change_status(
        &mut self,
        status: ReadingStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.current_entry(cx) else {
            return;
        };
        if entry.status == status {
            return;
        }
        let id = entry.user_book_id;
        self.run_write(
            id,
            WriteKind::Status,
            "Couldn't change the status",
            move |client| client.set_status(id, status).map(Some),
            window,
            cx,
        );
    }

    fn change_rating(
        &mut self,
        rating: Option<Rating>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.current_entry(cx) else {
            return;
        };
        let id = entry.user_book_id;
        // Sending the shown status keeps Hardcover from marking the book Read.
        let keep = entry.status;
        self.run_write(
            id,
            WriteKind::Rating,
            "Couldn't save the rating",
            move |client| client.set_rating(id, rating, keep).map(Some),
            window,
            cx,
        );
    }

    fn submit_progress(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.current_entry(cx) else {
            return;
        };
        let text = self.page.read(cx).value();
        let Ok(page) = text.trim().parse::<u32>() else {
            self.page_error = Some("Enter the page you're on.".into());
            cx.notify();
            return;
        };
        if let Some(total) = entry.total_pages()
            && page > total
        {
            self.page_error = Some(format!("This edition has {total} pages.").into());
            cx.notify();
            return;
        }
        self.page_error = None;
        let id = entry.user_book_id;
        let read = entry
            .current_read
            .as_ref()
            .filter(|r| !r.is_finished())
            .map(|r| r.id);
        self.run_write(
            id,
            WriteKind::Progress,
            "Couldn't update progress",
            move |client| {
                client
                    .update_progress(id, read, ProgressUpdate::Pages(page))
                    .map(Some)
            },
            window,
            cx,
        );
    }

    /// Changes one date of the current read; `None` leaves that date as it is
    /// in the model, `Some(None)` clears it.
    fn change_dates(
        &mut self,
        started: Option<Option<Date>>,
        finished: Option<Option<Date>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(entry) = self.current_entry(cx) else {
            return;
        };
        let Some(read) = entry.current_read.clone() else {
            return;
        };
        // `set_read_dates` sets both dates, so the other one comes from the model.
        let started = started.unwrap_or(read.started_at);
        let finished = finished.unwrap_or(read.finished_at);
        if started == read.started_at && finished == read.finished_at {
            return;
        }
        if let (Some(s), Some(f)) = (started, finished)
            && f < s
        {
            window.push_notification(
                Notification::warning("The finish date can't be before the start date.")
                    .title("Dates not changed"),
                cx,
            );
            self.revert_controls(window, cx);
            return;
        }
        let id = entry.user_book_id;
        self.run_write(
            id,
            WriteKind::Dates,
            "Couldn't change the dates",
            move |client| client.set_read_dates(read.id, started, finished).map(Some),
            window,
            cx,
        );
    }

    fn confirm_remove(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(entry) = self.current_entry(cx) else {
            return;
        };
        let id = entry.user_book_id;
        let title: SharedString = format!("Remove “{}”?", entry.book.title).into();
        let view = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let view = view.clone();
            alert
                .title(title.clone())
                .description(
                    "Its status, rating, progress and reading dates are deleted from Hardcover.",
                )
                .confirm()
                .ok_text("Remove")
                .ok_variant(ButtonVariant::Danger)
                .on_ok(move |_, window, cx| {
                    let _ = view.update(cx, |this, cx| this.remove(id, window, cx));
                    true
                })
        });
    }

    fn remove(&mut self, id: UserBookId, window: &mut Window, cx: &mut Context<Self>) {
        self.run_write(
            id,
            WriteKind::Remove,
            "Couldn't remove the book",
            move |client| client.remove_from_library(id).map(|()| None),
            window,
            cx,
        );
    }

    fn add_to_library(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(Subject::Book(book)) = self.subject.clone() else {
            return;
        };
        let status = self
            .add_status
            .read(cx)
            .selected_value()
            .copied()
            .unwrap_or(ReadingStatus::WantToRead);
        let model = self.model.clone();
        if !model.update(cx, |m, cx| m.begin_add(book.id, cx)) {
            return;
        }
        let client = Services::global(cx).client().clone();
        let book_id = book.id;
        cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.add_to_library(book_id, status) })
                .await;
            model.update(cx, |m, cx| {
                m.end_add(book_id, cx);
                if let Ok(entry) = &result {
                    m.apply_written(entry.clone(), cx);
                }
            });
            if let Err(error) = result {
                let _ = this.update_in(cx, |_, window, cx| {
                    window.push_notification(
                        Notification::error(error_message(&error)).title("Couldn't add the book"),
                        cx,
                    );
                });
            }
        })
        .detach();
    }

    // ----- rendering -----

    /// Loads the cover for `url`, releasing the previous cover if it changed.
    fn load_cover(
        &mut self,
        url: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Arc<RenderImage>> {
        if self.cover.as_ref().map(|(u, _)| u) != url.as_ref()
            && let Some((old, image)) = self.cover.take()
        {
            release_cover_image(&old, image, window, cx);
        }
        let url = url?;
        let image = cover_image(&url, window, cx);
        self.cover = Some((url, image.clone()));
        image
    }

    fn render_header(
        &mut self,
        book: &BookSummary,
        entry: Option<&LibraryEntry>,
        details: Option<&BookDetails>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let cover_url = entry
            .and_then(|e| e.cover())
            .or(book.cover.as_ref())
            .map(|c| c.url.clone());
        let cover = self.load_cover(cover_url, window, cx);

        let mut facts: Vec<String> = Vec::new();
        if let Some(series) = details.and_then(|d| d.series.first()) {
            facts.push(match series.position {
                Some(p) => format!("{} {}", series.name, series_position_text(p)),
                None => series.name.clone(),
            });
        }
        if let Some(year) = book
            .release_year
            .or_else(|| details.and_then(|d| d.release_date.map(|d| chrono::Datelike::year(&d))))
        {
            facts.push(year.to_string());
        }
        if let Some(pages) = entry.and_then(LibraryEntry::total_pages).or(book.pages) {
            facts.push(format!("{pages} pages"));
        }
        let average = book
            .average_rating
            .or_else(|| details.and_then(|d| d.summary.average_rating))
            .filter(|a| *a > 0.0);

        // Wraps so a narrow pane puts the text under the cover instead of
        // squeezing it beside.
        h_flex()
            .items_start()
            .gap_2()
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .flex_wrap()
                    .items_start()
                    .gap_6()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w_32()
                            .h_48()
                            .rounded(cx.theme().radius_lg)
                            .overflow_hidden()
                            .bg(cx.theme().muted)
                            .flex()
                            .items_center()
                            .justify_center()
                            .map(|this| match cover {
                                Some(image) => {
                                    this.child(img(image).size_full().object_fit(ObjectFit::Cover))
                                }
                                None => this
                                    .child(Icon::new(IconName::BookOpen).large().text_color(muted)),
                            }),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w(rems(12.))
                            .gap_1()
                            .child(
                                div()
                                    .text_2xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(book.title.clone()),
                            )
                            .when_some(book.subtitle.clone(), |this, subtitle| {
                                this.child(div().text_lg().text_color(muted).child(subtitle))
                            })
                            .child(div().text_base().child(authors_text(&book.authors)))
                            .when(!facts.is_empty(), |this| {
                                this.child(
                                    div().text_sm().text_color(muted).child(facts.join(" · ")),
                                )
                            })
                            .when_some(average, |this, avg| {
                                this.child(
                                    h_flex()
                                        .gap_1()
                                        .text_sm()
                                        .text_color(muted)
                                        .child(Icon::new(IconName::StarFill).xsmall())
                                        .child(format!(
                                            "{} average on Hardcover",
                                            average_rating_text(avg)
                                        )),
                                )
                            })
                            .when_some(book.web_url(), |this, url| {
                                this.child(
                                    div().pt_2().child(
                                        Link::new("open-on-hardcover")
                                            .href(url)
                                            .text_sm()
                                            .child("Open on Hardcover"),
                                    ),
                                )
                            }),
                    ),
            )
            .when_some(entry, |this, entry| {
                let busy = self
                    .model
                    .read(cx)
                    .pending_write(entry.user_book_id)
                    .is_some();
                let view = cx.entity().downgrade();
                this.child(
                    Button::new("book-more")
                        .ghost()
                        .icon(IconName::Ellipsis)
                        .tooltip("More actions")
                        .disabled(busy)
                        .dropdown_menu(move |menu, _, _| {
                            let view = view.clone();
                            menu.item(
                                PopupMenuItem::new("Remove from library…")
                                    .icon(IconName::Delete)
                                    .on_click(move |_, window, cx| {
                                        let _ = view
                                            .update(cx, |this, cx| this.confirm_remove(window, cx));
                                    }),
                            )
                        }),
                )
            })
    }

    fn render_your_book(
        &self,
        entry: &LibraryEntry,
        label_width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pending = self.model.read(cx).pending_write(entry.user_book_id);
        let busy = pending.is_some();
        let rating = entry.rating;
        section("Your book").child(
            Form::new()
                .label_layout(Axis::Horizontal)
                .label_width(label_width)
                .child(
                    Field::new().label("Status").child(
                        div()
                            .w_full()
                            .max_w_56()
                            .child(Select::new(&self.status).disabled(busy)),
                    ),
                )
                .child(
                    Field::new().label("Rating").child(
                        h_flex()
                            .gap_2()
                            .child(
                                StarRating::new("your-rating")
                                    .value(rating)
                                    .disabled(busy)
                                    .on_change(cx.listener(
                                        |this, rating: &Option<Rating>, window, cx| {
                                            this.change_rating(*rating, window, cx)
                                        },
                                    )),
                            )
                            .when_some(rating, |this, r| {
                                this.child(
                                    div()
                                        .text_sm()
                                        .text_color(cx.theme().muted_foreground)
                                        .child(crate::format::rating_text(r)),
                                )
                            })
                            .when(rating.is_some(), |this| {
                                this.child(
                                    Button::new("clear-rating")
                                        .ghost()
                                        .xsmall()
                                        .label("Clear")
                                        .disabled(busy)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.change_rating(None, window, cx)
                                        })),
                                )
                            })
                            .when(pending == Some(WriteKind::Rating), |this| {
                                this.child(
                                    div()
                                        .text_xs()
                                        .text_color(cx.theme().muted_foreground)
                                        .child("Saving…"),
                                )
                            }),
                    ),
                ),
        )
    }

    fn render_progress(&self, entry: &LibraryEntry, cx: &mut Context<Self>) -> impl IntoElement {
        let pending = self.model.read(cx).pending_write(entry.user_book_id);
        let progress = progress_summary(entry);
        let muted = cx.theme().muted_foreground;
        let mut summary = progress.text();
        if let (Some(pct), Some(_)) = (progress.percent, progress.page) {
            summary.push_str(&format!(" · {}%", pct.round() as u32));
        }
        section("Progress")
            .child(
                v_flex()
                    .gap_2()
                    .child(div().text_sm().child(summary))
                    .child(
                        Progress::new("reading-progress")
                            .value(progress.percent.unwrap_or(0.0))
                            .accessibility_label("Reading progress"),
                    ),
            )
            .child(
                h_flex()
                    .flex_wrap()
                    .gap_2()
                    .child(Input::new(&self.page).w_24().disabled(pending.is_some()))
                    .when_some(progress.total_pages, |this, total| {
                        this.child(
                            div()
                                .text_sm()
                                .text_color(muted)
                                .child(format!("of {total}")),
                        )
                    })
                    .child(
                        Button::new("update-progress")
                            .primary()
                            .label("Update progress")
                            .loading(pending == Some(WriteKind::Progress))
                            .disabled(pending.is_some())
                            .on_click(
                                cx.listener(|this, _, window, cx| this.submit_progress(window, cx)),
                            ),
                    ),
            )
            .when_some(self.page_error.clone(), |this, error| {
                this.child(div().text_sm().text_color(cx.theme().danger).child(error))
            })
    }

    fn render_dates(
        &self,
        entry: &LibraryEntry,
        label_width: Pixels,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let busy = self
            .model
            .read(cx)
            .pending_write(entry.user_book_id)
            .is_some();
        section("Reading dates").child(
            Form::new()
                .label_layout(Axis::Horizontal)
                .label_width(label_width)
                .child(
                    Field::new().label("Started").child(
                        div().w_full().max_w_56().child(
                            DatePicker::new(&self.started)
                                .placeholder("Not set")
                                .cleanable(true)
                                .disabled(busy),
                        ),
                    ),
                )
                .child(
                    Field::new().label("Finished").child(
                        div().w_full().max_w_56().child(
                            DatePicker::new(&self.finished)
                                .placeholder("Not set")
                                .cleanable(true)
                                .disabled(busy),
                        ),
                    ),
                ),
        )
    }

    fn render_add(&self, book: &BookSummary, cx: &mut Context<Self>) -> impl IntoElement {
        let adding = self.model.read(cx).is_adding(book.id);
        section("Not in your library").child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .max_w_56()
                        .child(Select::new(&self.add_status).disabled(adding)),
                )
                .child(
                    Button::new("add-to-library")
                        .icon(IconName::Plus)
                        .label("Add to library")
                        .loading(adding)
                        .on_click(
                            cx.listener(|this, _, window, cx| this.add_to_library(window, cx)),
                        ),
                ),
        )
    }

    fn render_description(&self, book: &BookSummary, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let body: AnyElement = match self.details.get(&book.id) {
            Some(Details::Loaded(details)) => match details.description.as_deref().map(str::trim) {
                Some(text) if !text.is_empty() => v_flex()
                    .gap_2()
                    .text_sm()
                    .children(
                        text.split('\n')
                            .map(str::trim)
                            .filter(|p| !p.is_empty())
                            .map(|p| div().child(p.to_owned())),
                    )
                    .into_any_element(),
                _ => div()
                    .text_sm()
                    .text_color(muted)
                    .child("No description on Hardcover.")
                    .into_any_element(),
            },
            Some(Details::Failed(message)) => h_flex()
                .gap_2()
                .text_sm()
                .child(div().text_color(muted).child(message.clone()))
                .child(
                    Button::new("retry-details")
                        .small()
                        .label("Try again")
                        .on_click(cx.listener(|this, _, _, cx| this.retry_details(cx))),
                )
                .into_any_element(),
            None => v_flex()
                .gap_2()
                .child(Skeleton::new().h_4().w_full())
                .child(Skeleton::new().h_4().w_full())
                .child(Skeleton::new().h_4().w_2_3())
                .into_any_element(),
        };
        section("Description").child(body).into_any_element()
    }
}

/// A titled region of the pane.
fn section(title: &'static str) -> gpui_kit::Div {
    v_flex()
        .gap_3()
        .child(div().text_sm().font_semibold().child(title))
}

impl Render for BookDetailView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let model = self.model.read(cx);
        let entry = self.entry(model).cloned();
        let book = match (&self.subject, &entry) {
            (_, Some(entry)) => Some(entry.book.clone()),
            (Some(Subject::Book(book)), None) => Some(book.clone()),
            _ => None,
        };
        let Some(book) = book else {
            self.load_cover(None, window, cx);
            return v_flex()
                .id("book-detail-empty")
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_color(cx.theme().muted_foreground)
                .child(Icon::new(IconName::BookOpen).large())
                .child(div().text_sm().child("Select a book to see its details"))
                .into_any_element();
        };
        let details = match self.details.get(&book.id) {
            Some(Details::Loaded(d)) => Some(d.clone()),
            _ => None,
        };

        // Form label widths take pixels; derive them from the rem size.
        let label_width = window.rem_size() * 6.;
        let header = self.render_header(&book, entry.as_ref(), details.as_deref(), window, cx);
        v_flex()
            .id("book-detail")
            .size_full()
            .overflow_y_scrollbar()
            .child(
                v_flex()
                    .w_full()
                    .max_w(rems(48.))
                    .p_6()
                    .gap_6()
                    .child(header)
                    .map(|this| match &entry {
                        Some(entry) => this
                            .child(self.render_your_book(entry, label_width, cx))
                            .when(entry.status == ReadingStatus::CurrentlyReading, |this| {
                                this.child(self.render_progress(entry, cx))
                            })
                            .when(entry.current_read.is_some(), |this| {
                                this.child(self.render_dates(entry, label_width, cx))
                            }),
                        None => this.child(self.render_add(&book, cx)),
                    })
                    .child(self.render_description(&book, cx)),
            )
            .into_any_element()
    }
}
