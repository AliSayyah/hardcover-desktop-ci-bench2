//! A library shelf: the filtered, sorted collection beside the detail pane.

use std::collections::HashMap;

use gpui_kit::component::empty::{
    Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyMediaVariant, EmptyTitle,
};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::list::{List, ListDelegate, ListEvent, ListItem, ListState};
use gpui_kit::component::select::{Select, SelectEvent, SelectItem, SelectState};
use gpui_kit::component::{
    ActiveTheme as _, Icon, IconName, IndexPath, Sizable as _, h_flex, h_resizable,
    resizable_panel, v_flex,
};
use gpui_kit::{
    App, AppContext as _, Context, Entity, FontWeight, IntoElement, ParentElement as _, Render,
    ScrollStrategy, SharedString, Styled as _, Subscription, Window, div,
};
use hardcover_core::{LibraryEntry, ReadingStatus, UserBookId};

use super::book_row::BookRow;
use super::collection::{SortOrder, shelf_rows};
use super::model::{LibraryModel, SyncStatus};
use crate::book::detail_view::{BookDetailView, Subject};
use crate::format::status_label;

#[derive(Clone)]
pub struct SortOption(SortOrder, SharedString);

impl SelectItem for SortOption {
    type Value = SortOrder;

    fn title(&self) -> SharedString {
        self.1.clone()
    }

    fn value(&self) -> &SortOrder {
        &self.0
    }
}

/// The empty state the list shows, decided by the view.
#[derive(Clone, Default)]
struct EmptyCopy {
    title: SharedString,
    description: SharedString,
}

/// Rows of one shelf for the virtualized list.
pub struct ShelfDelegate {
    rows: Vec<LibraryEntry>,
    loading: bool,
    empty: EmptyCopy,
}

impl ShelfDelegate {
    fn id_at(&self, ix: IndexPath) -> Option<UserBookId> {
        self.rows.get(ix.row).map(|e| e.user_book_id)
    }
}

impl ListDelegate for ShelfDelegate {
    type Item = ListItem;

    fn items_count(&self, _: usize, _: &App) -> usize {
        self.rows.len()
    }

    fn render_item(
        &mut self,
        ix: IndexPath,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> Option<Self::Item> {
        let entry = self.rows.get(ix.row)?;
        Some(
            ListItem::new(("library-book", entry.user_book_id.get() as u64))
                .accessibility_label(entry.book.title.clone())
                .child(BookRow::for_entry(entry)),
        )
    }

    fn set_selected_index(
        &mut self,
        _: Option<IndexPath>,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) {
        // Selection is owned by `LibraryView`, keyed by `user_book_id`.
    }

    fn render_empty(
        &mut self,
        _: &mut Window,
        _: &mut Context<ListState<Self>>,
    ) -> impl IntoElement {
        empty_state(
            IconName::Inbox,
            self.empty.title.clone(),
            self.empty.description.clone(),
        )
    }

    fn loading(&self, _: &App) -> bool {
        self.loading
    }
}

/// A centred empty state for a region.
pub fn empty_state(
    icon: IconName,
    title: SharedString,
    description: SharedString,
) -> impl IntoElement {
    div()
        .size_full()
        .p_6()
        .flex()
        .items_center()
        .justify_center()
        .child(
            Empty::new().header(
                EmptyHeader::new()
                    .media(
                        EmptyMedia::new()
                            .with_variant(EmptyMediaVariant::Icon)
                            .child(Icon::new(icon)),
                    )
                    .title(EmptyTitle::new().child(title))
                    .description(EmptyDescription::new().child(description)),
            ),
        )
}

pub struct LibraryView {
    model: Entity<LibraryModel>,
    status: ReadingStatus,
    filter: Entity<InputState>,
    sort: Entity<SelectState<Vec<SortOption>>>,
    order: SortOrder,
    list: Entity<ListState<ShelfDelegate>>,
    detail: Entity<BookDetailView>,
    /// The selected entry per shelf, so switching shelves keeps each one's place.
    selection: HashMap<ReadingStatus, UserBookId>,
    /// Inputs of the rows currently in the list, to skip needless rebuilds.
    rows_key: Option<(u64, ReadingStatus, SharedString, SortOrder)>,
    _subscriptions: Vec<Subscription>,
}

impl LibraryView {
    pub fn new(model: Entity<LibraryModel>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter books"));
        let options: Vec<SortOption> = SortOrder::ALL
            .into_iter()
            .map(|o| SortOption(o, o.label().into()))
            .collect();
        let sort = cx.new(|cx| SelectState::new(options, Some(IndexPath::new(0)), window, cx));
        let list = cx.new(|cx| {
            ListState::new(
                ShelfDelegate {
                    rows: Vec::new(),
                    loading: true,
                    empty: EmptyCopy::default(),
                },
                window,
                cx,
            )
        });
        let detail = cx.new(|cx| BookDetailView::new(model.clone(), window, cx));

        let subscriptions = vec![
            cx.observe_in(&model, window, |this, _, window, cx| {
                this.refresh_rows(window, cx);
                if !this.selection.contains_key(&this.status) {
                    this.select_first(window, cx);
                }
                cx.notify();
            }),
            cx.subscribe_in(
                &filter,
                window,
                |this, _, event: &InputEvent, window, cx| {
                    if matches!(event, InputEvent::Change) {
                        this.refresh_rows(window, cx);
                    }
                },
            ),
            cx.subscribe_in(
                &sort,
                window,
                |this, _, event: &SelectEvent<Vec<SortOption>>, window, cx| {
                    if let SelectEvent::Confirm(Some(order)) = event {
                        this.order = *order;
                        this.refresh_rows(window, cx);
                        this.scroll_to_selection(window, cx);
                    }
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
                    let id = list.read(cx).delegate().id_at(ix);
                    if let Some(id) = id {
                        this.select(id, window, cx);
                    }
                },
            ),
        ];

        let mut this = Self {
            model,
            status: ReadingStatus::CurrentlyReading,
            filter,
            sort,
            order: SortOrder::RecentlyUpdated,
            list,
            detail,
            selection: HashMap::new(),
            rows_key: None,
            _subscriptions: subscriptions,
        };
        this.refresh_rows(window, cx);
        this
    }

    /// Switches to `status`, restoring that shelf's selection.
    pub fn show_shelf(
        &mut self,
        status: ReadingStatus,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.status != status {
            self.status = status;
            self.filter.update(cx, |f, cx| f.set_value("", window, cx));
            self.refresh_rows(window, cx);
        }
        match self.selection.get(&status).copied() {
            Some(id) => self.select(id, window, cx),
            None => self.select_first(window, cx),
        }
        self.scroll_to_selection(window, cx);
        cx.notify();
    }

    pub fn focus_list(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| list.focus(window, cx));
    }

    fn select(&mut self, id: UserBookId, window: &mut Window, cx: &mut Context<Self>) {
        self.selection.insert(self.status, id);
        self.sync_list_selection(window, cx);
        self.detail.update(cx, |d, cx| {
            d.set_subject(Some(Subject::Entry(id)), window, cx)
        });
    }

    fn select_first(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let first = self
            .list
            .read(cx)
            .delegate()
            .rows
            .first()
            .map(|e| e.user_book_id);
        match first {
            Some(id) => self.select(id, window, cx),
            None => self
                .detail
                .update(cx, |d, cx| d.set_subject(None, window, cx)),
        }
    }

    /// Rebuilds the shelf rows from the model, the filter and the sort order.
    fn refresh_rows(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).value();
        let model = self.model.read(cx);
        let key = (model.revision(), self.status, query.clone(), self.order);
        let loading = !model.is_loaded();
        if self.rows_key.as_ref() == Some(&key) && !loading {
            return;
        }
        let entries = model.entries();
        let rows: Vec<LibraryEntry> = shelf_rows(entries, self.status, &query, self.order)
            .into_iter()
            .map(|ix| entries[ix].clone())
            .collect();
        let shelf = status_label(self.status);
        let empty = if !query.trim().is_empty() {
            EmptyCopy {
                title: "No matches".into(),
                description: format!("No books in {shelf} match “{}”.", query.trim()).into(),
            }
        } else if let SyncStatus::Failed { message, .. } = model.sync_status()
            && entries.is_empty()
        {
            EmptyCopy {
                title: "Couldn't load your library".into(),
                description: message.clone(),
            }
        } else {
            EmptyCopy {
                title: format!("No books in {shelf}").into(),
                description: "Books you shelve here on Hardcover or from Search appear here."
                    .into(),
            }
        };
        self.rows_key = Some(key);
        self.list.update(cx, |list, cx| {
            let delegate = list.delegate_mut();
            delegate.rows = rows;
            delegate.loading = loading;
            delegate.empty = empty;
            cx.notify();
        });
        self.sync_list_selection(window, cx);
    }

    /// Points the list's positional selection at the selected id, if it is shown.
    fn sync_list_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let id = self.selection.get(&self.status).copied();
        self.list.update(cx, |list, cx| {
            let pos = id.and_then(|id| {
                list.delegate()
                    .rows
                    .iter()
                    .position(|e| e.user_book_id == id)
            });
            let ix = pos.map(IndexPath::new);
            if list.selected_index() != ix {
                list.set_selected_index(ix, window, cx);
            }
        });
    }

    fn scroll_to_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.list.update(cx, |list, cx| {
            if let Some(ix) = list.selected_index() {
                list.scroll_to_item(ix, ScrollStrategy::Center, window, cx);
            }
        });
    }

    fn render_collection(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let count = self.model.read(cx).count(self.status);
        v_flex()
            .size_full()
            .min_w_0()
            .child(
                v_flex()
                    .gap_3()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div()
                                    .text_lg()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .child(status_label(self.status)),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(count.to_string()),
                            ),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                div().flex_1().min_w_0().child(
                                    Input::new(&self.filter).small().cleanable(true).prefix(
                                        Icon::new(IconName::Search)
                                            .small()
                                            .text_color(cx.theme().muted_foreground),
                                    ),
                                ),
                            )
                            .child(
                                div()
                                    .flex_shrink_0()
                                    .w_40()
                                    .child(Select::new(&self.sort).small()),
                            ),
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

impl Render for LibraryView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rem = window.rem_size();
        h_resizable("library-split")
            .child(
                resizable_panel()
                    .size(rem * 24.)
                    .size_range(rem * 16.0..rem * 40.)
                    .child(self.render_collection(cx)),
            )
            .child(
                resizable_panel()
                    // With the sidebar, 16rem + 18rem fits the 800px minimum window.
                    .size_range(rem * 18.0..gpui_kit::Pixels::MAX)
                    .child(self.detail.clone()),
            )
    }
}
