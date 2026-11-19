//! A half-star rating control.
//!
//! gpui-kit's `Rating` stores whole stars (`usize`), but Hardcover ratings go
//! in half steps, so showing a 3.5 there would round it and the next click
//! would overwrite it. This control keeps the `Rating` value exact: each star
//! has a leading half (n − 0.5) and a trailing half (n), clicking the current
//! value clears it, and the keyboard steps by half a star.
//!
//! Controlled: the owner passes the value and receives requested changes.

use std::rc::Rc;

use gpui_kit::component::{ActiveTheme as _, Icon, IconName, h_flex};
use gpui_kit::{
    App, ElementId, FocusHandle, InteractiveElement as _, IntoElement, ParentElement as _,
    RenderOnce, SharedString, StatefulInteractiveElement as _, Styled as _, Window, actions, div,
    prelude::FluentBuilder as _, relative,
};
use hardcover_core::Rating;

/// Key context of the control; its bindings live in `main`.
pub const KEY_CONTEXT: &str = "StarRating";

actions!(star_rating, [IncreaseRating, DecreaseRating, ClearRating]);

const STARS: u8 = 5;

/// How much of one star is filled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StarFill {
    Empty,
    Half,
    Full,
}

/// The fill of star `star` (1-based) for `value` stars.
pub fn star_fill(value: f32, star: u8) -> StarFill {
    let star = f32::from(star);
    if value >= star {
        StarFill::Full
    } else if value >= star - 0.5 {
        StarFill::Half
    } else {
        StarFill::Empty
    }
}

/// The value a click on `target` requests: clicking the current value clears.
pub fn clicked_value(current: Option<Rating>, target: f32) -> Option<Rating> {
    match current {
        Some(r) if r.value() == target => None,
        _ => Rating::new(target),
    }
}

/// The value after one keyboard step of half a star (`up` or down). Stepping
/// down from half a star clears; stepping up from none gives half a star.
pub fn stepped_value(current: Option<Rating>, up: bool) -> Option<Rating> {
    let v = current.map_or(0.0, Rating::value);
    let next = if up {
        (v + 0.5).min(Rating::MAX)
    } else {
        v - 0.5
    };
    Rating::new(next)
}

type ChangeHandler = Rc<dyn Fn(&Option<Rating>, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub struct StarRating {
    id: SharedString,
    value: Option<Rating>,
    disabled: bool,
    on_change: Option<ChangeHandler>,
}

impl StarRating {
    pub fn new(id: impl Into<SharedString>) -> Self {
        Self {
            id: id.into(),
            value: None,
            disabled: false,
            on_change: None,
        }
    }

    pub fn value(mut self, value: Option<Rating>) -> Self {
        self.value = value;
        self
    }

    pub fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }

    /// Called with the requested value; the owner decides whether to apply it.
    pub fn on_change(
        mut self,
        handler: impl Fn(&Option<Rating>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }

    fn child_id(&self, part: &str) -> ElementId {
        ElementId::from(SharedString::from(format!("{}-{part}", self.id)))
    }
}

impl RenderOnce for StarRating {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus: FocusHandle = window
            .use_keyed_state(self.child_id("focus"), cx, |_, cx| {
                cx.focus_handle().tab_stop(true)
            })
            .read(cx)
            .clone();
        let hover = window.use_keyed_state(self.child_id("hover"), cx, |_, _| None::<f32>);
        let hovered = if self.disabled { None } else { *hover.read(cx) };
        let shown = hovered.unwrap_or_else(|| self.value.map_or(0.0, Rating::value));
        let is_focused = focus.is_focused(window);

        let value = self.value;
        let disabled = self.disabled;
        let on_change = self.on_change.clone();
        let request = move |next: Option<Rating>, window: &mut Window, cx: &mut App| {
            if let Some(handler) = &on_change
                && next != value
            {
                handler(&next, window, cx);
            }
        };

        let filled_color = cx.theme().yellow;
        let empty_color = cx.theme().muted_foreground;
        let label = match value {
            Some(r) => format!("Rating: {} of 5 stars", r.value()),
            None => "Rating: not rated".to_owned(),
        };

        h_flex()
            .id(ElementId::from(self.id.clone()))
            .relative()
            .gap_0p5()
            .p_0p5()
            .rounded(cx.theme().radius)
            .aria_label(label)
            .when(!disabled, |this| {
                let request = Rc::new(request);
                this.track_focus(&focus)
                    .key_context(KEY_CONTEXT)
                    .on_action({
                        let request = request.clone();
                        move |_: &IncreaseRating, window, cx| {
                            request(stepped_value(value, true), window, cx)
                        }
                    })
                    .on_action({
                        let request = request.clone();
                        move |_: &DecreaseRating, window, cx| {
                            request(stepped_value(value, false), window, cx)
                        }
                    })
                    .on_action({
                        let request = request.clone();
                        move |_: &ClearRating, window, cx| request(None, window, cx)
                    })
                    .on_hover({
                        let hover = hover.clone();
                        move |hovered, _, cx| {
                            if !*hovered {
                                hover.update(cx, |h, cx| {
                                    *h = None;
                                    cx.notify();
                                });
                            }
                        }
                    })
                    .children((1..=STARS).map(|star| {
                        let half = |trailing: bool| {
                            let target = f32::from(star) - if trailing { 0.0 } else { 0.5 };
                            let hover = hover.clone();
                            let request = request.clone();
                            div()
                                .id(ElementId::from(SharedString::from(format!(
                                    "{star}-{}",
                                    if trailing { "end" } else { "start" }
                                ))))
                                .absolute()
                                .top_0()
                                .h_full()
                                .w(relative(0.5))
                                .map(|this| {
                                    if trailing {
                                        this.right_0()
                                    } else {
                                        this.left_0()
                                    }
                                })
                                .on_mouse_move(move |_, _, cx| {
                                    hover.update(cx, |h, cx| {
                                        if *h != Some(target) {
                                            *h = Some(target);
                                            cx.notify();
                                        }
                                    });
                                })
                                .on_click({
                                    let focus = focus.clone();
                                    move |_, window, cx| {
                                        focus.focus(window, cx);
                                        request(clicked_value(value, target), window, cx);
                                    }
                                })
                        };
                        render_star(star, shown, filled_color, empty_color)
                            .child(half(false))
                            .child(half(true))
                    }))
            })
            .when(disabled, |this| {
                this.opacity(0.5).children(
                    (1..=STARS).map(|star| render_star(star, shown, filled_color, empty_color)),
                )
            })
            .when(is_focused, |this| {
                this.child(
                    div()
                        .absolute()
                        .inset_0()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().ring),
                )
            })
    }
}

fn render_star(
    star: u8,
    value: f32,
    filled: gpui_kit::Hsla,
    empty: gpui_kit::Hsla,
) -> gpui_kit::Div {
    let fill = star_fill(value, star);
    div()
        .relative()
        .flex_shrink_0()
        .size_5()
        .map(|this| match fill {
            StarFill::Full => this.child(Icon::new(IconName::StarFill).size_5().text_color(filled)),
            StarFill::Empty => this.child(Icon::new(IconName::Star).size_5().text_color(empty)),
            // An outline star with its leading half filled.
            StarFill::Half => this
                .child(Icon::new(IconName::Star).size_5().text_color(empty))
                .child(
                    div()
                        .absolute()
                        .top_0()
                        .left_0()
                        .h_full()
                        .w(relative(0.5))
                        .overflow_hidden()
                        .child(
                            Icon::new(IconName::StarFill)
                                .size_5()
                                .flex_shrink_0()
                                .text_color(filled),
                        ),
                ),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(v: f32) -> Option<Rating> {
        Rating::new(v)
    }

    #[test]
    fn fills() {
        assert_eq!(star_fill(3.5, 3), StarFill::Full);
        assert_eq!(star_fill(3.5, 4), StarFill::Half);
        assert_eq!(star_fill(3.5, 5), StarFill::Empty);
        assert_eq!(star_fill(0.0, 1), StarFill::Empty);
        assert_eq!(star_fill(0.5, 1), StarFill::Half);
        assert_eq!(star_fill(5.0, 5), StarFill::Full);
    }

    #[test]
    fn clicks_set_half_steps_and_clicking_the_value_clears() {
        assert_eq!(clicked_value(None, 3.5), r(3.5));
        assert_eq!(clicked_value(r(2.0), 4.0), r(4.0));
        assert_eq!(clicked_value(r(4.0), 4.0), None);
        assert_eq!(clicked_value(r(4.5), 4.0), r(4.0));
    }

    #[test]
    fn keyboard_steps() {
        assert_eq!(stepped_value(None, true), r(0.5));
        assert_eq!(stepped_value(r(3.5), true), r(4.0));
        assert_eq!(stepped_value(r(5.0), true), r(5.0));
        assert_eq!(stepped_value(r(3.0), false), r(2.5));
        assert_eq!(stepped_value(r(0.5), false), None);
        assert_eq!(stepped_value(None, false), None);
    }
}
