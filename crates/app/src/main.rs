//! Hardcover Desktop: a Linux client for hardcover.app.

mod app_state;
mod book;
mod format;
mod library;
mod search;
mod shell;
mod sign_in;
mod update;

use gpui_kit::{
    App, AppContext as _, KeyBinding, Size, TitlebarOptions, WindowBounds, WindowOptions, actions,
};

use crate::book::star_rating;

actions!(
    hardcover,
    [
        Quit,
        Refresh,
        FocusSearch,
        ShowCurrentlyReading,
        ShowWantToRead,
        ShowRead,
        ShowPaused,
        ShowDidNotFinish,
    ]
);

const APP_ID: &str = "io.github.alisayyah.HardcoverDesktop";

/// Must run before any menu is built (none yet), and after `gpui_kit::init`
/// so these bindings win over the components' own `ctrl-f`.
fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("ctrl-q", Quit, None),
        KeyBinding::new("ctrl-r", Refresh, None),
        KeyBinding::new("ctrl-f", FocusSearch, None),
        // Input binds ctrl-f to its own search; inside a text field it still
        // means "go to Search".
        KeyBinding::new("ctrl-f", FocusSearch, Some("Input")),
        KeyBinding::new("ctrl-1", ShowCurrentlyReading, None),
        KeyBinding::new("ctrl-2", ShowWantToRead, None),
        KeyBinding::new("ctrl-3", ShowRead, None),
        KeyBinding::new("ctrl-4", ShowPaused, None),
        KeyBinding::new("ctrl-5", ShowDidNotFinish, None),
        KeyBinding::new(
            "right",
            star_rating::IncreaseRating,
            Some(star_rating::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "up",
            star_rating::IncreaseRating,
            Some(star_rating::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "left",
            star_rating::DecreaseRating,
            Some(star_rating::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "down",
            star_rating::DecreaseRating,
            Some(star_rating::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "backspace",
            star_rating::ClearRating,
            Some(star_rating::KEY_CONTEXT),
        ),
        KeyBinding::new(
            "delete",
            star_rating::ClearRating,
            Some(star_rating::KEY_CONTEXT),
        ),
    ]);
    cx.on_action(|_: &Quit, cx| cx.quit());
}

fn main() {
    if std::env::args()
        .skip(1)
        .any(|a| a == "--version" || a == "-V")
    {
        println!("hardcover-desktop {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    // Resolved before anything else: once an update replaces the binary,
    // /proc/self/exe names the deleted file, and restarting must use this path.
    let exe = std::env::current_exe().ok();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .run(move |cx| {
            gpui_kit::init(cx);
            bind_keys(cx);
            if let Some(exe) = &exe {
                cx.set_restart_path(exe.clone());
            }
            update::init(exe.clone(), cx);

            // Window geometry is a platform boundary: logical pixels, not rems.
            let size = Size::new(gpui_kit::px(1200.), gpui_kit::px(800.));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::centered(size, cx)),
                window_min_size: Some(Size::new(gpui_kit::px(800.), gpui_kit::px(560.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Hardcover".into()),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.to_owned()),
                ..Default::default()
            };
            gpui_kit::open_window(options, cx, |window, cx| {
                app_state::follow_appearance(window, cx);
                cx.new(|cx| shell::Shell::new(window, cx))
            })
            .expect("failed to open window");
        });
}
