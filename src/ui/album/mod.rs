mod header;
mod view;
use crate::{
    application::{
        album::{Album, SessionRejected},
        playback::Playback,
    },
    domain::{Artist, Item},
    infrastructure::jellyfin::Client,
    ui::components::track_menu::{TrackMenuRequest, TrackTarget},
};
use gpui::{Context, Entity, EventEmitter, Image, Pixels, Point, Window, prelude::*};
use std::sync::Arc;

pub enum AlbumEvent {
    Back,
    Artist(Artist),
    Unauthorized,
}

pub struct AlbumPage {
    album: Entity<Album>,
    playback: Entity<Playback>,
    back_label: String,
}
impl EventEmitter<AlbumEvent> for AlbumPage {}
impl EventEmitter<TrackMenuRequest> for AlbumPage {}
impl AlbumPage {
    pub fn new(
        client: Client,
        item: Item,
        cover: Option<Arc<Image>>,
        back_label: String,
        playback: Entity<Playback>,
        cx: &mut Context<Self>,
    ) -> Self {
        let album = cx.new(|cx| Album::new(client, item, cover, cx));
        cx.observe(&album, |_, _, cx| cx.notify()).detach();
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        cx.subscribe(&album, |_, _, _: &SessionRejected, cx| {
            cx.emit(AlbumEvent::Unauthorized)
        })
        .detach();
        Self {
            album,
            playback,
            back_label,
        }
    }

    fn open_artist(&self, artist: Artist, cx: &mut Context<Self>) {
        cx.emit(AlbumEvent::Artist(artist));
    }

    fn play_album(&mut self, start: usize, cx: &mut Context<Self>) {
        let album = self.album.read(cx);
        if album.busy() {
            return;
        }
        let (item, tracks, cover) = (
            album.item().clone(),
            album.tracks().to_vec(),
            album.cover().cloned(),
        );
        self.playback.update(cx, |player, cx| {
            player.play_album(item, tracks, start, cover, cx)
        });
    }

    fn queue_album(&mut self, cx: &mut Context<Self>) {
        let album = self.album.read(cx);
        if album.busy() {
            return;
        }
        let (item, tracks, cover) = (
            album.item().clone(),
            album.tracks().to_vec(),
            album.cover().cloned(),
        );
        self.playback
            .update(cx, |player, cx| player.enqueue(item, tracks, cover, cx));
    }

    fn open_track_menu(
        &self,
        index: usize,
        position: Point<Pixels>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let album = self.album.read(cx);
        let Some(track) = album.tracks().get(index) else {
            return;
        };
        let request = TrackMenuRequest {
            target: TrackTarget {
                track: track.clone(),
                album: Arc::new(album.item().clone()),
                cover: album.cover().cloned(),
            },
            position,
        };
        cx.emit(request);
    }
}

impl Render for AlbumPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.library(cx)
    }
}
