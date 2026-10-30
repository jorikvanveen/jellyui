use crate::{
    application::playback::Playback,
    domain::{Artist, Item, artist_credits, playback::PlaybackState as State},
    ui::{
        components::{artwork, duration},
        theme,
    },
};
use gpui::{
    Bounds, Context, Div, Entity, EventEmitter, FontWeight, Image, ImageFormat, ObjectFit, Pixels,
    Task, Window, div, prelude::*, px, relative, rgb,
};
use std::{io::Cursor, sync::Arc};
mod icons;
mod slider;
use icons::{TransportIcon, player_art_placeholder, player_button, player_link, volume_icon};
use slider::Slider;

const PLAYER_HEIGHT: f32 = 76.;
const TIMELINE_HEIGHT: f32 = 20.;
const TIMELINE_BOTTOM: f32 = 6.;

pub enum PlaybackNavigation {
    Album {
        album: Box<Item>,
        cover: Option<Arc<Image>>,
    },
    Artist(Artist),
}

pub struct Player {
    playback: Entity<Playback>,
    seek_drag: Option<f32>,
    seek_bounds: Bounds<Pixels>,
    volume_dragging: bool,
    volume_bounds: Bounds<Pixels>,
    thumbnail: Option<Arc<Image>>,
    thumbnail_key: Option<(u64, u32)>,
    thumbnail_task: Option<Task<()>>,
    transport_revision: u64,
}

impl EventEmitter<PlaybackNavigation> for Player {}
impl Player {
    pub fn new(playback: Entity<Playback>, cx: &mut Context<Self>) -> Self {
        cx.observe(&playback, |view, playback, cx| {
            let player = playback.read(cx);
            if view.transport_revision != player.transport_revision() {
                view.seek_drag = None;
                if player.state() == State::Stopped {
                    view.volume_dragging = false;
                }
            }
            if view.thumbnail_key.map(|key| key.0) != player.cover().map(|image| image.id()) {
                view.thumbnail = None;
                view.thumbnail_task = None;
                view.thumbnail_key = None;
            }
            view.transport_revision = player.transport_revision();
            cx.notify();
        })
        .detach();
        Self {
            playback,
            seek_drag: None,
            seek_bounds: Bounds::default(),
            volume_dragging: false,
            volume_bounds: Bounds::default(),
            thumbnail: None,
            thumbnail_key: None,
            thumbnail_task: None,
            transport_revision: 0,
        }
    }
    fn open_current_album(&self, cx: &mut Context<Self>) {
        if let Some(album) = self.playback.read(cx).queue().current_album() {
            cx.emit(PlaybackNavigation::Album {
                album: Box::new(album.clone()),
                cover: self.playback.read(cx).cover().cloned(),
            });
        }
    }

    fn artist_links(&self, cx: &Context<Self>) -> Div {
        let mut row = div().flex().items_center().min_w_0().overflow_hidden();
        let (Some(track), Some(album)) = (
            self.playback.read(cx).queue().current(),
            self.playback.read(cx).queue().current_album(),
        ) else {
            return row;
        };
        let artists = artist_credits(track, album);
        if artists.is_empty() {
            return row.child("Unknown artist");
        }
        for (index, artist) in artists.into_iter().enumerate() {
            if index > 0 {
                row = row.child(div().flex_shrink_0().child(",\u{a0}"));
            }
            row = row.child(
                player_link(("player-artist", index), artist.name.clone()).on_click(cx.listener(
                    move |_, _, _, cx| {
                        cx.emit(PlaybackNavigation::Artist(artist.clone()));
                    },
                )),
            );
        }
        row
    }

    fn update_thumbnail(&mut self, scale_factor: f32, cx: &mut Context<Self>) {
        let pixels = (PLAYER_HEIGHT * scale_factor).round().max(1.) as u32;
        let key = self
            .playback
            .read(cx)
            .cover()
            .map(|cover| (cover.id(), pixels));
        if self.thumbnail_key == key {
            return;
        }
        self.thumbnail = None;
        self.thumbnail_task = None;
        self.thumbnail_key = key;
        let (Some(key), Some(cover)) = (key, self.playback.read(cx).cover().cloned()) else {
            return;
        };
        // Resize once at the display's physical resolution. Shrinking the full
        // cover in GPUI's linear sampler aliases fine details at this size.
        let request = cx
            .background_executor()
            .spawn(async move { resize_cover(&cover, pixels) });
        self.thumbnail_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |view, cx| {
                if view.thumbnail_key != Some(key) {
                    return;
                }
                match result {
                    Ok(thumbnail) => view.thumbnail = Some(thumbnail),
                    Err(error) => tracing::warn!(%error, "Could not resize player artwork"),
                }
                cx.notify();
            });
        }));
    }
}

impl Render for Player {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_thumbnail(window.scale_factor(), cx);
        let playback = self.playback.read(cx);
        let queue = playback.queue();
        let track = queue.current();
        let has_track = track.is_some();
        let total_ticks = track.and_then(|track| track.run_time_ticks);
        let elapsed_ticks = if let (Some(preview), Some(total)) = (self.seek_drag, total_ticks) {
            (f64::from(preview) * total as f64) as u64
        } else {
            (playback.elapsed().as_nanos() / 100).min(u128::from(u64::MAX)) as u64
        };
        let shown_elapsed = total_ticks.map_or(elapsed_ticks, |total| elapsed_ticks.min(total));
        let progress = total_ticks
            .filter(|&total| total > 0)
            .map(|total| (elapsed_ticks as f64 / total as f64).clamp(0., 1.) as f32)
            .unwrap_or(0.);
        let (icon, label) = match playback.state() {
            State::Loading | State::Playing => (TransportIcon::Pause, "Pause"),
            State::Paused => (TransportIcon::Play, "Resume"),
            State::Failed => (TransportIcon::Retry, "Retry"),
            State::Finished => (TransportIcon::Play, "Play again"),
            State::Stopped => (TransportIcon::Play, "Play"),
        };
        let album_link = if let Some(album) = queue.current_album() {
            player_link("player-album", album.name.clone())
                .on_click(cx.listener(|view, _, _, cx| view.open_current_album(cx)))
                .into_any_element()
        } else {
            div().into_any_element()
        };
        let details = div()
            .flex_1()
            .min_w_0()
            .flex()
            .flex_col()
            .gap_1()
            .child(
                div()
                    .truncate()
                    .text_sm()
                    .line_height(px(20.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(track.map(|track| track.name.clone()).unwrap_or_default()),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .text_xs()
                    .line_height(px(16.))
                    .text_color(rgb(theme::SUBTEXT0))
                    .child(self.artist_links(cx))
                    .child(album_link),
            );
        let artwork = div()
            .size(px(PLAYER_HEIGHT))
            .flex_shrink_0()
            .overflow_hidden()
            .bg(rgb(theme::SURFACE0))
            .child(
                if let Some(cover) = self.thumbnail.as_ref().or(playback.cover()) {
                    artwork::image(cover.clone())
                        .size_full()
                        .object_fit(ObjectFit::Cover)
                        .with_fallback(|| player_art_placeholder().into_any_element())
                        .into_any_element()
                } else {
                    player_art_placeholder().into_any_element()
                },
            );
        let controls = div()
            .absolute()
            .top_0()
            .left_0()
            .w_full()
            // Center within the space from the player's top to the rail's center.
            .bottom(px(TIMELINE_BOTTOM + TIMELINE_HEIGHT / 2.))
            .flex()
            .items_center()
            .justify_center()
            .gap_4()
            .child(
                player_button(
                    "previous-track",
                    "Previous track",
                    TransportIcon::Previous,
                    !has_track || queue.position == 0,
                    false,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.playback.update(cx, |player, cx| player.previous(cx))
                })),
            )
            .child(
                player_button("play-pause", label, icon, !has_track, true).on_click(cx.listener(
                    |view, _, _, cx| view.playback.update(cx, |player, cx| player.toggle(cx)),
                )),
            )
            .child(
                player_button(
                    "next-track",
                    "Next track",
                    TransportIcon::Next,
                    !has_track || queue.position + 1 >= queue.tracks.len(),
                    false,
                )
                .on_click(cx.listener(|view, _, _, cx| {
                    view.playback.update(cx, |player, cx| player.next(cx))
                })),
            );
        let timeline = div()
            .absolute()
            .bottom(px(TIMELINE_BOTTOM))
            .w_full()
            .h(px(TIMELINE_HEIGHT))
            .flex()
            .items_center()
            .gap_3()
            .text_xs()
            .text_color(rgb(theme::SUBTEXT0))
            .when(!has_track, |timeline| timeline.opacity(0.45))
            .child(
                div()
                    .w(px(40.))
                    .flex_shrink_0()
                    .text_right()
                    .child(duration(Some(shown_elapsed))),
            )
            .child(self.slider(
                Slider::Seek,
                progress,
                total_ticks.is_some_and(|total| total > 0),
                cx,
            ))
            .child(div().w(px(40.)).flex_shrink_0().child(if has_track {
                total_ticks
                    .map(|ticks| duration(Some(ticks)))
                    .unwrap_or_else(|| "--:--".into())
            } else {
                "0:00".into()
            }));
        div()
            .flex()
            .flex_col()
            .flex_shrink_0()
            .bg(rgb(theme::MANTLE))
            .border_t_1()
            .border_color(rgb(theme::SURFACE0))
            .child(
                div()
                    .w_full()
                    .h(px(PLAYER_HEIGHT))
                    .flex()
                    .items_center()
                    .gap_6()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap_3()
                            .child(artwork)
                            .child(details),
                    )
                    .child(
                        div()
                            .w(relative(0.5))
                            .max_w(px(640.))
                            .min_w_0()
                            .flex_shrink_0()
                            .h_full()
                            .relative()
                            .child(controls)
                            .child(timeline),
                    )
                    .child(
                        div().flex_1().min_w_0().h_full().relative().child(
                            div()
                                .absolute()
                                .right(px(24.))
                                .top(px((PLAYER_HEIGHT - TIMELINE_HEIGHT) / 2.))
                                .w(px(152.))
                                .max_w(relative(1.))
                                .h(px(TIMELINE_HEIGHT))
                                .flex()
                                .items_center()
                                .gap_2()
                                .child(volume_icon(playback.volume() == 0.))
                                .child(self.slider(Slider::Volume, playback.volume(), true, cx)),
                        ),
                    ),
            )
            .when_some(playback.error().map(str::to_owned), |view, error| {
                view.child(
                    div()
                        .px_6()
                        .pb_3()
                        .text_sm()
                        .text_color(rgb(theme::RED))
                        .child(error),
                )
            })
    }
}

fn resize_cover(cover: &Image, pixels: u32) -> image::ImageResult<Arc<Image>> {
    let image = image::load_from_memory_with_format(cover.bytes(), image::ImageFormat::Png)?;
    let thumbnail = image.resize_to_fill(pixels, pixels, image::imageops::FilterType::Lanczos3);
    let mut bytes = Cursor::new(Vec::new());
    thumbnail.write_to(&mut bytes, image::ImageFormat::Png)?;
    Ok(Arc::new(Image::from_bytes(
        ImageFormat::Png,
        bytes.into_inner(),
    )))
}
