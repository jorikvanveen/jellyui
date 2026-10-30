use super::AlbumPage;
use crate::{
    domain::{Item, artist_credits},
    ui::{
        components::{album_art_placeholder, album_button},
        theme,
    },
};
use gpui::{Context, Div, FontWeight, ObjectFit, div, prelude::*, px, rgb};

impl AlbumPage {
    pub(super) fn album_artist_links(&self, album: &Item, cx: &Context<Self>) -> Div {
        let mut artists = album
            .album_artists
            .as_ref()
            .filter(|artists| !artists.is_empty())
            .cloned()
            .unwrap_or_else(|| artist_credits(album, album));
        artists.retain(|artist| !artist.name.trim().is_empty());
        let mut row = div()
            .flex()
            .flex_wrap()
            .items_center()
            .text_lg()
            .text_color(rgb(theme::SUBTEXT1));
        if artists.is_empty() {
            return row.child("Unknown artist");
        }
        for (index, artist) in artists.into_iter().enumerate() {
            if index > 0 {
                row = row.child(div().child(",\u{a0}"));
            }
            row = row.child(
                div()
                    .id(("album-artist", index))
                    .focusable()
                    .min_w_0()
                    .max_w_full()
                    .truncate()
                    .cursor_pointer()
                    .hover(|style| style.text_color(rgb(theme::MAUVE)).underline())
                    .focus(|style| style.text_color(rgb(theme::MAUVE)).underline())
                    .child(artist.name.clone())
                    .on_click(
                        cx.listener(move |view, _, _, cx| view.open_artist(artist.clone(), cx)),
                    ),
            );
        }
        row
    }
    pub(super) fn album_header(&self, album: &Item, cx: &Context<Self>) -> impl IntoElement {
        let mut metadata = Vec::new();
        if let Some(date) = album
            .premiere_date
            .as_deref()
            .and_then(|date| date.split('T').next())
            .filter(|date| !date.is_empty())
        {
            metadata.push(date.to_owned());
        } else if let Some(year) = album.production_year {
            metadata.push(year.to_string());
        }
        let count = if self.album.read(cx).busy() {
            album.child_count
        } else {
            Some(self.album.read(cx).tracks().len())
        };
        if let Some(count) = count {
            metadata.push(format!(
                "{count} {}",
                if count == 1 { "track" } else { "tracks" }
            ));
        }
        let ticks = if self.album.read(cx).tracks().is_empty() {
            album.run_time_ticks
        } else {
            self.album
                .read(cx)
                .tracks()
                .iter()
                .try_fold(0u64, |total, track| {
                    total.checked_add(track.run_time_ticks?)
                })
        };
        if let Some(ticks) = ticks {
            let seconds = ticks / 10_000_000;
            metadata.push(if seconds >= 3600 {
                format!(
                    "{}h {}m {}s",
                    seconds / 3600,
                    seconds / 60 % 60,
                    seconds % 60
                )
            } else {
                format!("{}m {}s", seconds / 60, seconds % 60)
            });
        }
        let artwork = div()
            .w(px(280.))
            .max_w_full()
            .aspect_ratio(1.)
            .flex_shrink_0()
            .rounded_lg()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE0))
            .child(if let Some(cover) = self.album.read(cx).cover() {
                crate::ui::components::artwork::image(cover.clone())
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .with_fallback(|| album_art_placeholder().into_any_element())
                    .into_any_element()
            } else {
                album_art_placeholder().into_any_element()
            });
        let disabled = self.album.read(cx).busy() || self.album.read(cx).tracks().is_empty();
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap_6()
            .w_full()
            .pb_6()
            .child(artwork)
            .child(
                div()
                    .flex_1()
                    .min_w(px(240.))
                    .flex()
                    .flex_col()
                    .items_start()
                    .gap_4()
                    .child(
                        div()
                            .w_full()
                            .text_size(px(44.))
                            .line_height(gpui::relative(1.1))
                            .font_weight(FontWeight::BOLD)
                            .child(album.name.clone()),
                    )
                    .when(!metadata.is_empty(), |details| {
                        details.child(
                            div()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(metadata.join(" · ")),
                        )
                    })
                    .child(self.album_artist_links(album, cx))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_3()
                            .mt_2()
                            .child(
                                album_button("play-album", "Play", disabled, true)
                                    .on_click(cx.listener(|view, _, _, cx| view.play_album(0, cx))),
                            )
                            .child(
                                album_button("queue-album", "Queue", disabled, false)
                                    .on_click(cx.listener(|view, _, _, cx| view.queue_album(cx))),
                            ),
                    ),
            )
    }
}
