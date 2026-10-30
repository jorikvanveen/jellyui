use super::Jellyui;
use crate::domain::{Artist, Error, Item};
use crate::ui::album::{AlbumEvent, AlbumPage};
use crate::ui::components::track_menu::TrackMenuRequest;
use crate::ui::library::LibraryKind;
use gpui::{Context, Image, prelude::*};
use std::sync::Arc;

impl Jellyui {
    pub(super) fn open_album(
        &mut self,
        album: Item,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.detail_revision += 1;
        let back_label = self
            .artist
            .as_ref()
            .map(|artist| format!("Back to {}", artist.name))
            .unwrap_or_else(|| "Back to albums".into());
        let page = cx
            .new(|cx| AlbumPage::new(client, album, cover, back_label, self.playback.clone(), cx));
        cx.subscribe(&page, |view, _, event: &AlbumEvent, cx| match event {
            AlbumEvent::Back => view.leave_album(cx),
            AlbumEvent::Artist(artist) => view.open_artist(artist.clone(), cx),
            AlbumEvent::Unauthorized => view.request_failed(Error::Unauthorized, cx),
        })
        .detach();
        let window = self.window;
        cx.subscribe(&page, move |view, _, request: &TrackMenuRequest, cx| {
            let request = TrackMenuRequest {
                target: request.target.clone(),
                position: request.position,
            };
            let menu = view.track_menu.clone();
            let _ = window.update(cx, |_, window, cx| {
                menu.update(cx, |menu, cx| menu.open(request, window, cx))
            });
        })
        .detach();
        self.album = Some(page);
        self.error = None;
        cx.notify();
    }

    pub(super) fn leave_album(&mut self, cx: &mut Context<Self>) {
        self.detail_revision += 1;
        self.album = None;
        self.error = None;
        cx.notify();
    }

    pub(super) fn open_artist(&mut self, artist: Artist, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.leave_album(cx);
        self.section = LibraryKind::Artists;
        if artist.id.as_deref().is_none_or(str::is_empty) {
            self.artist = None;
            let revision = self.detail_revision;
            let request = cx
                .background_executor()
                .spawn(async move { client.artist_by_name(&artist.name) });
            cx.spawn(async move |this, cx| {
                let result = request.await;
                let _ = this.update(cx, |view, cx| {
                    if view.detail_revision != revision {
                        return;
                    }
                    match result {
                        Ok(artist) => view.open_artist(artist, cx),
                        Err(error) => view.request_failed(error, cx),
                    }
                    cx.notify();
                });
            })
            .detach();
            return;
        }
        self.artist = Some(artist.clone());
        self.artist_albums
            .update(cx, |page, cx| page.connect_artist(client, artist, cx));
    }

    pub(super) fn switch_library(&mut self, section: LibraryKind, cx: &mut Context<Self>) {
        self.leave_album(cx);
        self.section = section;
        self.artist = None;
    }
}
