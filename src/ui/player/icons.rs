use crate::ui::theme;
use gpui::{
    AnyElement, Context, Div, ElementId, PathBuilder, Stateful, Window, canvas, div, point,
    prelude::*, px, rgb, svg,
};
pub(super) fn volume_icon(muted: bool) -> impl IntoElement {
    let data: &[u8] = if muted {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/icons/filled-lucide/volume-x.svg"
        ))
    } else {
        include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/assets/icons/filled-lucide/volume-2.svg"
        ))
    };
    svg()
        .data(data)
        .size(px(16.))
        .flex_shrink_0()
        .text_color(rgb(theme::SUBTEXT0))
}

pub(super) fn player_link(id: impl Into<ElementId>, text: String) -> Stateful<Div> {
    div()
        .id(id)
        .focusable()
        .min_w_0()
        .max_w_full()
        .self_start()
        .truncate()
        .cursor_pointer()
        .hover(|style| style.text_color(rgb(theme::MAUVE)).underline())
        .focus(|style| style.text_color(rgb(theme::MAUVE)).underline())
        .child(text)
}

pub(super) fn player_art_placeholder() -> Div {
    div()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .text_xl()
        .text_color(rgb(theme::OVERLAY1))
        .child("♪")
}

#[derive(Clone, Copy)]
pub(super) enum TransportIcon {
    Play,
    Pause,
    Previous,
    Next,
    Retry,
}

pub(super) fn player_button(
    id: &'static str,
    label: &'static str,
    icon: TransportIcon,
    disabled: bool,
    primary: bool,
) -> Stateful<Div> {
    let background = if primary { theme::TEXT } else { theme::MANTLE };
    let hover = if primary {
        theme::LAVENDER
    } else {
        theme::SURFACE0
    };
    div()
        .id(id)
        .focusable()
        .tab_stop(!disabled)
        .size(px(if primary { 36. } else { 32. }))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(rgb(background))
        .focus(move |style| style.bg(rgb(hover)))
        .when(disabled, |button| button.opacity(0.35))
        .when(!disabled, |button| {
            button
                .cursor_pointer()
                .hover(move |style| style.bg(rgb(hover)))
        })
        .tooltip(move |_, cx| cx.new(|_| PlayerTooltip(label)).into())
        .child(transport_icon(
            icon,
            if primary { theme::BASE } else { theme::TEXT },
        ))
}

fn transport_icon(icon: TransportIcon, color: u32) -> AnyElement {
    match icon {
        TransportIcon::Play => canvas(
            |_, _, _| (),
            move |bounds, _, window, _| {
                let mut triangle = PathBuilder::fill();
                triangle.move_to(
                    bounds.origin + point(bounds.size.width * 0.3, bounds.size.height * 0.17),
                );
                triangle.line_to(
                    bounds.origin + point(bounds.size.width * 0.9, bounds.size.height * 0.5),
                );
                triangle.line_to(
                    bounds.origin + point(bounds.size.width * 0.3, bounds.size.height * 0.83),
                );
                triangle.close();
                if let Ok(path) = triangle.build() {
                    window.paint_path(path, rgb(color));
                }
            },
        )
        .size(px(18.))
        .into_any_element(),
        TransportIcon::Pause => div()
            .size(px(18.))
            .flex()
            .items_center()
            .justify_center()
            .gap_1()
            .child(div().w(px(4.)).h(px(12.)).rounded_sm().bg(rgb(color)))
            .child(div().w(px(4.)).h(px(12.)).rounded_sm().bg(rgb(color)))
            .into_any_element(),
        TransportIcon::Previous | TransportIcon::Next => {
            let next = matches!(icon, TransportIcon::Next);
            canvas(
                |_, _, _| (),
                move |bounds, _, window, _| {
                    let at = |x: f32, y: f32| {
                        bounds.origin
                            + point(
                                bounds.size.width * if next { x } else { 1. - x },
                                bounds.size.height * y,
                            )
                    };
                    let mut path = PathBuilder::fill();
                    path.move_to(at(0.18, 0.17));
                    path.line_to(at(0.72, 0.5));
                    path.line_to(at(0.18, 0.83));
                    path.close();
                    path.move_to(at(0.78, 0.17));
                    path.line_to(at(0.9, 0.17));
                    path.line_to(at(0.9, 0.83));
                    path.line_to(at(0.78, 0.83));
                    path.close();
                    if let Ok(path) = path.build() {
                        window.paint_path(path, rgb(color));
                    }
                },
            )
            .size(px(18.))
            .into_any_element()
        }
        TransportIcon::Retry => div()
            .text_xl()
            .text_color(rgb(color))
            .child("↻")
            .into_any_element(),
    }
}

struct PlayerTooltip(&'static str);

impl Render for PlayerTooltip {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_3()
            .py_2()
            .rounded_md()
            .bg(rgb(theme::SURFACE0))
            .text_xs()
            .text_color(rgb(theme::TEXT))
            .child(self.0)
    }
}
