//! Process startup, key bindings and the application window.
use crate::{
    logging,
    ui::{
        components::{dialog, input},
        login::SignIn,
        shell::Jellyui,
    },
};
use gpui::{
    App, AppContext, Bounds, KeyBinding, TitlebarOptions, WindowBounds, WindowOptions, px, size,
};

use crate::{
    APP_ID,
    ui::actions::{BackTab, Quit, Tab},
};

pub fn run() {
    logging::init();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "Starting Jellyui");
    gpui_platform::application().run(|cx: &mut App| {
        dialog::init(cx);
        input::bind_keys(cx);
        cx.bind_keys([
            KeyBinding::new("enter", SignIn, Some("Login")),
            KeyBinding::new("tab", Tab, None),
            KeyBinding::new("shift-tab", BackTab, None),
            KeyBinding::new("ctrl-q", Quit, None),
        ]);
        cx.on_action(|_: &Quit, cx| cx.quit());
        cx.on_window_closed(|cx, _| {
            if cx.windows().is_empty() {
                tracing::info!("Last window closed; quitting");
                cx.quit();
            }
        })
        .detach();
        let bounds = Bounds::centered(None, size(px(1440.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                window_min_size: Some(size(px(560.), px(480.))),
                titlebar: Some(TitlebarOptions {
                    title: Some("Jellyui".into()),
                    ..Default::default()
                }),
                app_id: Some(APP_ID.into()),
                ..Default::default()
            },
            |window, cx| cx.new(|cx| Jellyui::new(window, cx)),
        )
        .expect("could not open the Jellyui window");
        tracing::info!("Application window opened");
        cx.activate(true);
    });
    tracing::info!("Jellyui stopped");
}
