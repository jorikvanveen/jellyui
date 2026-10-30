use super::{AlbumRequest, Playback, QueueReplacementRequested, SessionRejected};
use crate::domain::playback::AlbumAction;
use crate::{
    domain::{Error, Item},
    infrastructure::jellyfin::Client,
};
use gpui::{Context, Image};
use std::sync::Arc;

impl Playback {
    pub fn play_album(
        &mut self,
        album: Item,
        tracks: Vec<Item>,
        start: usize,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if start >= tracks.len() {
            return;
        }
        self.request_play(
            AlbumRequest {
                action: AlbumAction::Play,
                client,
                album,
                cover,
                tracks: Some(tracks),
                start,
            },
            cx,
        );
    }

    pub(super) fn request_play(&mut self, request: AlbumRequest, cx: &mut Context<Self>) {
        if self.pending_play.is_some() {
            return;
        }
        let album = request.album.name.clone();
        if self.prepare_play(request) {
            cx.emit(QueueReplacementRequested { album });
        } else {
            self.confirm_queue_replacement(true, cx);
        }
    }

    pub(super) fn prepare_play(&mut self, request: AlbumRequest) -> bool {
        let discards_queue = !self.queue.tracks.is_empty()
            || self.album_request.is_some()
            || !self.album_requests.is_empty();
        self.pending_play = Some(request);
        discards_queue
    }

    pub(super) fn take_play_confirmation(&mut self, confirmed: bool) -> Option<AlbumRequest> {
        self.pending_play.take().filter(|_| confirmed)
    }

    pub fn confirm_queue_replacement(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if let Some(request) = self.take_play_confirmation(confirmed) {
            self.cancel_album_requests();
            self.album_requests.push_back(request);
            self.process_album_requests(cx);
        }
    }

    pub(super) fn play_ready(
        &mut self,
        client: Client,
        album: Item,
        tracks: Vec<Item>,
        start: usize,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        self.poll_events(cx);
        if self.queue.replace(album, tracks, start) {
            self.client = Some(client);
            self.reset_cover(cover, cx);
            self.start(false, cx);
        }
    }

    pub fn enqueue(
        &mut self,
        album: Item,
        tracks: Vec<Item>,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        self.album_requests.push_back(AlbumRequest {
            action: AlbumAction::Queue,
            client,
            album,
            cover,
            tracks: Some(tracks),
            start: 0,
        });
        self.process_album_requests(cx);
    }

    pub fn load_album(
        &mut self,
        action: AlbumAction,
        album: Item,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        let request = AlbumRequest {
            action,
            client,
            album,
            cover,
            tracks: None,
            start: 0,
        };
        if action == AlbumAction::Play {
            self.request_play(request, cx);
            return;
        }
        self.album_requests.push_back(request);
        self.process_album_requests(cx);
    }

    pub(super) fn cancel_album_requests(&mut self) {
        self.album_request_revision = self.album_request_revision.wrapping_add(1);
        self.album_request = None;
        self.album_requests.clear();
        self.pending_play = None;
    }

    pub(super) fn process_album_requests(&mut self, cx: &mut Context<Self>) {
        if self.album_request.is_some() {
            return;
        }
        while let Some(mut request) = self.album_requests.pop_front() {
            if let Some(tracks) = request.tracks.take() {
                match request.action {
                    AlbumAction::Play => self.play_ready(
                        request.client,
                        request.album,
                        tracks,
                        request.start,
                        request.cover,
                        cx,
                    ),
                    AlbumAction::Queue => {
                        self.append_tracks(request.client, request.album, tracks, request.cover, cx)
                    }
                }
                continue;
            }
            // Resolve albums in click order. Play cancels earlier requests;
            // later Queue actions, including individual tracks, wait for it.
            let revision = self.album_request_revision;
            let client = request.client.clone();
            let id = request.album.id.clone();
            let load = cx
                .background_executor()
                .spawn(async move { client.tracks(&id) });
            self.album_request = Some(cx.spawn(async move |this, cx| {
                let result = load.await;
                let _ = this.update(cx, |view, cx| {
                    if view.album_request_revision != revision {
                        return;
                    }
                    view.album_request = None;
                    match result {
                        Ok(tracks) if tracks.is_empty() => {
                            view.error =
                                Some("This album has no tracks available to your account.".into());
                        }
                        Ok(tracks) => {
                            request.tracks = Some(tracks);
                            view.album_requests.push_front(request);
                        }
                        Err(Error::Unauthorized) => {
                            view.cancel_album_requests();
                            cx.emit(SessionRejected);
                            return;
                        }
                        Err(error) => {
                            tracing::error!(%error, "Could not load album for playback");
                            view.error = Some(error.to_string());
                        }
                    }
                    view.process_album_requests(cx);
                    cx.notify();
                });
            }));
            break;
        }
    }

    pub(super) fn append_tracks(
        &mut self,
        client: Client,
        album: Item,
        tracks: Vec<Item>,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
        self.poll_events(cx);
        let was_empty = self.queue.tracks.is_empty();
        let added = tracks.len();
        if self.queue.append(album, tracks) {
            if was_empty {
                self.client = Some(client);
                self.reset_cover(cover, cx);
            }
            tracing::info!(
                added,
                total = self.queue.tracks.len(),
                "Tracks appended to queue"
            );
            self.report_current(true);
            self.refresh_prefetch();
            cx.notify();
        }
    }
}
