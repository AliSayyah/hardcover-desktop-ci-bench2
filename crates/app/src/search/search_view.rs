//! Search: a debounced query, paged results and the shared detail pane.
//!
//! Every query change bumps a revision; a response is applied only if its
//! revision is still current, so a slow page for an old query never replaces
//! newer results.

use std::time::Duration;

use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::spinner::Spinner;
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, h_resizable, resizable_panel, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, IntoElement, ParentElement as _, Render, SharedString,
    Styled as _, Subscription, Task, WeakEntity, Window, div, prelude::FluentBuilder as _,
};
use hardcover_core::{BookId, BookSummary, SearchPage};

use crate::app_state::Services;
use crate::book::detail_view::{BookDetailView, Subject};
use crate::format::error_message;
use crate::library::book_row::BookRow;
use crate::library::library_view::empty_state;
use crate::library::model::LibraryModel;
use crate::search::results::{Phase, SearchResults};

const DEBOUNCE: Duration = Duration::from_millis(300);

pub struct ResultsDelegate {
    results: SearchResults,
    model: Entity<LibraryModel>,
    view: WeakEntity<SearchView>,
}

impl ListDelegate for ResultsDelegate {
    type Item = ListItem;

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.results.books().len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        cx: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let book = self.results.books().get(ix.row)?;
        let status = self
            .model
            .read(cx)
            .entry_for_book(book.id)
            .map(|e| e.status);
        Some(
            ListItem::new(("search-book", book.id.get() as u64))
                .accessibility_label(book.title.clone())
                .child(BookRow::for_search_result(book, status)),
        )
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        // Selection is owned by `SearchView`, keyed by book id.
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        let (icon, title, description): (IconName, SharedString, SharedString) =
            match self.results.phase() {
                Phase::Idle => (
                    IconName::Search,
                    "Search Hardcover".into(),
                    "Find books by title, author or series.".into(),
                ),
                Phase::Failed(message) => (
                    IconName::CircleAlert,
                    "Search failed".into(),
                    message.clone().into(),
                ),
                _ => (
                    IconName::Search,
                    "No results".into(),
                    format!("Nothing on Hardcover matches “{}”.", self.results.query()).into(),
                ),
            };
        empty_state(icon, title, description)
    }

    fn has_more(&self, _: &App) -> bool {
        self.results.has_more()
    }

    fn load_more_threshold(&self) -> usize {
        5
    }

    fn load_more(&mut self, window: &mut Window, cx: &mut Context<ListState<Self>>) {
        if !self.results.has_more() {
            return;
        }
        let view = self.view.clone();
        // Deferred: the view updates this list when it starts the request.
        window.defer(cx, move |window, cx| {
            let _ = view.update(cx, |view, cx| view.load_next_page(window, cx));
        });
    }
}

pub struct SearchView {
    input: Entity<InputState>,
    list: Entity<ListState<ResultsDelegate>>,
    detail: Entity<BookDetailView>,
    selected: Option<BookId>,
    _search_task: Option<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl SearchView {
    pub fn new(model: Entity<LibraryModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Search books"));
        let view = cx.entity().downgrade();
        let list = cx.new(|cx| {
            ListState::new(
                ResultsDelegate {
                    results: SearchResults::default(),
                    model: model.clone(),
                    view,
                },
                window,
                cx,
            )
        });
        let detail = cx.new(|cx| BookDetailView::new(model.clone(), window, cx));
        let subscriptions = vec![
            cx.subscribe_in(
                &input,
                window,
                |this, input, event: &InputEvent, window, cx| match event {
                    InputEvent::Change => {
                        let query = input.read(cx).value();
                        this.query_changed(query, window, cx);
                    }
                    InputEvent::PressEnter { .. } => this.focus_results(window, cx),
                    _ => {}
                },
            ),
            cx.subscribe_in(
                &list,
                window,
                |this, list, event: &ListEvent, window, cx| {
                    let ix = match event {
                        ListEvent::Select(ix) | ListEvent::Confirm(ix) => *ix,
                        ListEvent::Cancel => return,
                    };
                    let book = list
                        .read(cx)
                        .delegate()
                        .results
                        .books()
                        .get(ix.row)
                        .cloned();
                    if let Some(book) = book {
                        this.select(book, window, cx);
                    }
                },
            ),
            // Status badges follow library changes.
            cx.observe(&model, |this, _, cx| {
                this.list.update(cx, |_, cx| cx.notify())
            }),
        ];
        Self {
            input,
            list,
            detail,
            selected: None,
            _search_task: None,
            _subscriptions: subscriptions,
        }
    }

    pub fn focus_input(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
    }

    fn focus_results(&self, window: &mut Window, cx: &mut App) {
        if !self.list.read(cx).delegate().results.books().is_empty() {
            self.list.update(cx, |list, cx| list.focus(window, cx));
        }
    }

    fn select(&mut self, book: BookSummary, window: &mut Window, cx: &mut Context<Self>) {
        self.selected = Some(book.id);
        self.sync_list_selection(window, cx);
        self.detail.update(cx, |d, cx| {
            d.set_subject(Some(Subject::Book(book)), window, cx)
        });
    }

    fn query_changed(&mut self, query: SharedString, window: &mut Window, cx: &mut Context<Self>) {
        let revision = self.list.update(cx, |list, cx| {
            let revision = list.delegate_mut().results.start(query.trim().into());
            cx.notify();
            revision
        });
        self.sync_list_selection(window, cx);
        let Some(revision) = revision else {
            // Blank query: nothing to fetch.
            self._search_task = None;
            return;
        };
        let client = Services::global(cx).client().clone();
        // Replacing the task drops a pending debounce for the previous query.
        self._search_task = Some(cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(DEBOUNCE).await;
            let q = query.to_string();
            let result = cx
                .background_spawn(async move { client.search_books(&q, 1) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.receive(revision, result, window, cx)
            });
        }));
    }

    fn load_next_page(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let request = self.list.update(cx, |list, _| {
            list.delegate_mut().results.next_page_request()
        });
        let Some((revision, query, page)) = request else {
            return;
        };
        self.list.update(cx, |_, cx| cx.notify());
        let client = Services::global(cx).client().clone();
        self._search_task = Some(cx.spawn_in(window, async move |this, cx| {
            let result = cx
                .background_spawn(async move { client.search_books(&query, page) })
                .await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.receive(revision, result, window, cx)
            });
        }));
    }

    fn receive(
        &mut self,
        revision: u64,
        result: hardcover_core::Result<SearchPage>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let result = result.map_err(|e| error_message(&e));
        self.list.update(cx, |list, cx| {
            if list.delegate_mut().results.receive(revision, result) {
                cx.notify();
            }
        });
        self.sync_list_selection(window, cx);
        cx.notify();
    }

    /// Points the list's positional selection at the selected book, if shown.
    fn sync_list_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let selected = self.selected;
        self.list.update(cx, |list, cx| {
            let ix = selected
                .and_then(|id| {
                    list.delegate()
                        .results
                        .books()
                        .iter()
                        .position(|b| b.id == id)
                })
                .map(IndexPath::new);
            if list.selected_index() != ix {
                list.set_selected_index(ix, window, cx);
            }
        });
    }

    fn render_results(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let delegate = self.list.read(cx).delegate();
        let results = &delegate.results;
        let status: Option<SharedString> = match results.phase() {
            Phase::Searching => Some("Searching…".into()),
            Phase::LoadingMore => Some("Loading more…".into()),
            Phase::Loaded => results.total().map(|t| match t {
                1 => "1 result".into(),
                n => format!("{n} results").into(),
            }),
            Phase::Failed(message) if !results.books().is_empty() => Some(message.clone().into()),
            _ => None,
        };
        let busy = matches!(results.phase(), Phase::Searching | Phase::LoadingMore);
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        Input::new(&self.input)
                            .cleanable(true)
                            .prefix(
                                Icon::new(IconName::Search).text_color(cx.theme().muted_foreground),
                            )
                            .when(busy, |this| this.suffix(Spinner::new().small())),
                    )
                    .child(
                        div()
                            .h_4()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .children(status),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(List::new(&self.list).size_full().p_1()),
            )
    }
}

impl Render for SearchView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        h_resizable("search-split")
            .child(
                resizable_panel()
                    .size(rem * 24.)
                    .size_range(rem * 16.0..rem * 40.)
                    .child(self.render_results(cx)),
            )
            .child(
                resizable_panel()
                    // With the sidebar, 16rem + 18rem fits the 800px minimum window.
                    .size_range(rem * 18.0..gpui_kit::Pixels::MAX)
                    .child(self.detail.clone()),
            )
    }
}
