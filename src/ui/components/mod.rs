pub mod artwork;
pub mod dialog;
pub mod input;
pub mod track_menu;

use super::theme;
use gpui::{Context, Div, FontWeight, Stateful, Window, div, prelude::*, px, rgb};

pub fn album_art_placeholder() -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_size(px(64.))
        .text_color(rgb(theme::OVERLAY1))
        .child("♪")
}

pub fn album_button(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    primary: bool,
) -> Stateful<Div> {
    let hover = if primary {
        theme::LAVENDER
    } else {
        theme::SURFACE1
    };
    div()
        .id(id)
        .focusable()
        .tab_stop(!disabled)
        .flex()
        .items_center()
        .justify_center()
        .gap_3()
        .px_6()
        .py_3()
        .rounded_full()
        .bg(rgb(if primary {
            theme::TEXT
        } else {
            theme::SURFACE0
        }))
        .text_color(rgb(if primary { theme::BASE } else { theme::TEXT }))
        .font_weight(FontWeight::SEMIBOLD)
        .cursor_pointer()
        .focus(move |style| style.bg(rgb(hover)))
        .when(disabled, |button| button.opacity(0.5))
        .when(!disabled, |button| {
            button.hover(move |style| style.bg(rgb(hover)))
        })
        .when(primary, |button| button.child("▶"))
        .child(label)
}

pub fn button(
    id: &'static str,
    label: &'static str,
    disabled: bool,
    hover_color: u32,
) -> Stateful<Div> {
    div()
        .id(id)
        .focusable()
        .tab_stop(!disabled)
        .px_4()
        .py_2()
        .rounded_md()
        .bg(rgb(theme::SURFACE1))
        .border_1()
        .border_color(rgb(theme::SURFACE1))
        .focus(|style| style.border_color(rgb(theme::MAUVE)))
        .cursor_pointer()
        .when(disabled, |div| div.opacity(0.5))
        .when(!disabled, |div| {
            div.hover(|style| style.bg(rgb(hover_color)))
        })
        .child(label)
}

pub fn duration(ticks: Option<u64>) -> String {
    match ticks {
        Some(ticks) => {
            let seconds = ticks / 10_000_000;
            format!("{}:{:02}", seconds / 60, seconds % 60)
        }
        None => String::new(),
    }
}

pub(crate) struct Tooltip(pub &'static str);

impl Render for Tooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .rounded_md()
            .bg(rgb(theme::SURFACE0))
            .text_color(rgb(theme::TEXT))
            .text_xs()
            .child(self.0)
    }
}
