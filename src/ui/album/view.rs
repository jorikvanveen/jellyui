use super::{AlbumEvent, AlbumPage};
use crate::ui::{
    components::{button, duration},
    theme,
};
use gpui::{Context, MouseButton, MouseDownEvent, SharedString, div, prelude::*, px, rgb};

impl AlbumPage {
    pub(super) fn library(&self, cx: &Context<Self>) -> impl IntoElement {
        let mut content = div().flex().flex_col().gap_4().w_full();
        {
            let album = self.album.read(cx).item();
            content = content
                .child(
                    div()
                        .id("back")
                        .focusable()
                        .flex()
                        .items_center()
                        .self_start()
                        .gap_2()
                        .cursor_pointer()
                        .text_color(rgb(theme::SUBTEXT0))
                        .focus(|style| style.text_color(rgb(theme::MAUVE)))
                        .hover(|style| style.text_color(rgb(theme::MAUVE)))
                        .child(div().text_xl().child("←"))
                        .child(self.back_label.clone())
                        .on_click(cx.listener(|_, _, _, cx| {
                            cx.emit(AlbumEvent::Back);
                        })),
                )
                .child(self.album_header(album, cx));
            let mut track_list = div().flex().flex_col().gap_2().w_full();
            for (index, track) in self.album.read(cx).tracks().iter().enumerate() {
                let current = self.playback.read(cx).is_current_track(&track.id);
                if index > 0 {
                    track_list = track_list.child(div().h(px(1.)).mx_3().bg(rgb(theme::SURFACE0)));
                }
                track_list = track_list.child(
                    div()
                        .id(SharedString::from(track.id.clone()))
                        .focusable()
                        .tab_stop(!self.album.read(cx).busy())
                        .flex()
                        .gap_4()
                        .items_center()
                        .px_3()
                        .py_3()
                        .rounded_md()
                        .when(current, |row| {
                            row.bg(rgb(theme::SURFACE0)).text_color(rgb(theme::MAUVE))
                        })
                        .focus(|style| style.bg(rgb(theme::SURFACE0).opacity(0.55)))
                        .hover(|style| style.bg(rgb(theme::SURFACE0).opacity(0.35)))
                        .cursor_pointer()
                        .on_click(cx.listener(move |view, _, _, cx| view.play_album(index, cx)))
                        .on_mouse_down(
                            MouseButton::Right,
                            cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                                view.open_track_menu(index, event.position, window, cx);
                                window.prevent_default();
                                cx.stop_propagation();
                            }),
                        )
                        .child(
                            div()
                                .w(px(50.))
                                .text_color(rgb(if current {
                                    theme::MAUVE
                                } else {
                                    theme::SUBTEXT0
                                }))
                                .child(format!(
                                    "{}.{}",
                                    track.parent_index_number.unwrap_or(1),
                                    track
                                        .index_number
                                        .map(|n| n.to_string())
                                        .unwrap_or_else(|| "–".into())
                                )),
                        )
                        .child(div().flex_1().child(track.name.clone()))
                        .child(
                            div()
                                .text_color(rgb(if current {
                                    theme::MAUVE
                                } else {
                                    theme::SUBTEXT0
                                }))
                                .child(duration(track.run_time_ticks)),
                        ),
                );
            }
            if !self.album.read(cx).tracks().is_empty() {
                content = content.child(track_list);
            }
            if !self.album.read(cx).busy()
                && self.album.read(cx).error().is_none()
                && self.album.read(cx).tracks().is_empty()
            {
                content = content.child("This album has no tracks available to your account.");
            }
        }
        if let Some(error) = self.album.read(cx).error() {
            content = content.child(div().text_color(rgb(theme::RED)).child(error.to_owned()));
        }
        if self.album.read(cx).busy() {
            content = content.child(div().text_color(rgb(theme::SUBTEXT0)).child("Loading…"));
        } else if self.album.read(cx).error().is_some() {
            content = content.child(button("retry", "Retry", false, theme::SURFACE2).on_click(
                cx.listener(|view, _, _, cx| {
                    view.album.update(cx, |album, cx| album.load(cx));
                }),
            ));
        }
        content
    }
}
