use std::{collections::VecDeque, io::Cursor, path::PathBuf, sync::Arc, time::Duration};

use gpui::{
    AnimationExt, AnyElement, Bounds, Context, DispatchPhase, Div, DragMoveEvent, ElementId,
    Entity, EntityId, EventEmitter, FontWeight, Image, ImageFormat, KeyDownEvent, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, ObjectFit, PathBuilder, Pixels, Point,
    ScrollHandle, SpringAnimation, SpringConfig, Stateful, Task, Window, canvas, div, point,
    prelude::*, px, relative, rgb, svg,
};

use crate::{
    audio::{self, Event},
    audio_file::AudioFile,
    duration,
    jellyfin::{Artist, Client, Error, Item},
    mpris,
    playback_reporting::{QUEUE_WINDOW, Reporter, Snapshot},
    queue::Queue,
    settings, theme,
    track_menu::{TrackMenuRequest, TrackTarget},
};

const PLAYER_HEIGHT: f32 = 76.;
const TIMELINE_HEIGHT: f32 = 20.;
const TIMELINE_BOTTOM: f32 = 6.;
const QUEUE_ROW_HEIGHT: f32 = 72.;
const QUEUE_ROW_PITCH: f32 = QUEUE_ROW_HEIGHT + 4.;
const QUEUE_PADDING: f32 = 8.;
const QUEUE_SPRING: SpringConfig = SpringConfig::new(700., 50., 1.);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Slider {
    Seek,
    Volume,
}

#[derive(Default, PartialEq)]
enum State {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
    Finished,
    Failed,
}

pub struct SessionRejected;
pub struct QueueReplacementRequested {
    pub album: String,
}

pub enum PlaybackNavigation {
    Album {
        album: Box<Item>,
        cover: Option<Arc<Image>>,
    },
    Artist(Artist),
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlbumAction {
    Play,
    Queue,
}

struct AlbumRequest {
    action: AlbumAction,
    client: Client,
    album: Item,
    cover: Option<Arc<Image>>,
    tracks: Option<Vec<Item>>,
    start: usize,
}

struct PrefetchedAudio {
    entry: usize,
    source: AudioFile,
    decoder: Option<audio::Prepared>,
}

#[derive(Default)]
pub struct Playback {
    queue: Queue,
    queue_scroll: ScrollHandle,
    queue_reorder: Option<QueueReorder>,
    album_requests: VecDeque<AlbumRequest>,
    album_request: Option<Task<()>>,
    album_request_revision: u64,
    pending_play: Option<AlbumRequest>,
    state: State,
    client: Option<Client>,
    handle: Option<audio::Handle>,
    audio_file: Option<AudioFile>,
    next_audio: Option<PrefetchedAudio>,
    mpris: Option<mpris::Bridge>,
    _mpris_commands: Option<Task<()>>,
    mpris_artwork: Option<(u64, Arc<[u8]>)>,
    mpris_seek_pending: bool,
    poll: Option<Task<()>>,
    reporter: Option<Reporter>,
    elapsed: Duration,
    offset: Duration,
    seek_drag: Option<f32>,
    seek_bounds: Bounds<Pixels>,
    volume: audio::Volume,
    settings_path: Option<PathBuf>,
    volume_dragging: bool,
    volume_bounds: Bounds<Pixels>,
    error: Option<String>,
    cover: Option<Arc<Image>>,
    cover_album_id: Option<String>,
    cover_task: Option<Task<()>>,
    thumbnail: Option<Arc<Image>>,
    thumbnail_key: Option<(u64, u32)>,
    thumbnail_task: Option<Task<()>>,
}

impl EventEmitter<SessionRejected> for Playback {}
impl EventEmitter<PlaybackNavigation> for Playback {}
impl EventEmitter<TrackMenuRequest> for Playback {}
impl EventEmitter<QueueReplacementRequested> for Playback {}

impl Playback {
    pub fn is_current_track(&self, track: &str) -> bool {
        matches!(self.state, State::Playing | State::Paused | State::Loading)
            && self
                .queue
                .current()
                .is_some_and(|current| current.id == track)
    }

    fn open_current_album(&self, cx: &mut Context<Self>) {
        if let Some(album) = self.queue.current_album() {
            cx.emit(PlaybackNavigation::Album {
                album: Box::new(album.clone()),
                cover: self.cover.clone(),
            });
        }
    }

    fn artist_links(&self, cx: &Context<Self>) -> Div {
        let mut row = div().flex().items_center().min_w_0().overflow_hidden();
        let (Some(track), Some(album)) = (self.queue.current(), self.queue.current_album()) else {
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
    pub fn new(cx: &mut Context<Self>) -> Self {
        let volume = audio::Volume::default();
        let settings_path = settings::path();
        if let Some(path) = &settings_path {
            match settings::load_volume(path) {
                Ok(level) => {
                    volume.set(level);
                    tracing::debug!(volume = level, "Volume setting restored");
                }
                Err(error) => tracing::warn!(%error, "Could not restore volume; using the default"),
            }
        } else {
            tracing::warn!("No config directory is available; volume cannot be remembered");
        }
        let quit_volume = volume.clone();
        let quit_path = settings_path.clone();
        // Capture the setting independently of the view: the last window can
        // release Playback before GPUI runs the application's quit callbacks.
        gpui::App::on_app_quit(cx, move |cx| {
            let volume = quit_volume.clone();
            let path = quit_path.clone();
            cx.background_executor().spawn(async move {
                if let Some(path) = path
                    && let Err(error) = settings::save_volume(&path, &volume)
                {
                    tracing::warn!(%error, "Could not save volume on quit");
                }
            })
        })
        .detach();
        let reporter = Reporter::new()
            .inspect_err(
                |error| tracing::warn!(%error, "Could not start Jellyfin playback reporting"),
            )
            .ok();
        if let Some(reporter) = reporter.clone() {
            gpui::App::on_app_quit(cx, move |cx| {
                let reporter = reporter.clone();
                cx.background_executor()
                    .spawn(async move { reporter.shutdown() })
            })
            .detach();
        }
        cx.on_release(|view, _| view.stop_reporting(false)).detach();
        let (mpris, mpris_commands) = match mpris::Bridge::start() {
            Ok((bridge, commands)) => {
                let task = cx.spawn(async move |this, cx| {
                    while let Ok(command) = commands.recv().await {
                        if this
                            .update(cx, |view, cx| view.media_command(command, cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                (Some(bridge), Some(task))
            }
            Err(error) => {
                tracing::warn!(%error, "Could not start MPRIS worker");
                (None, None)
            }
        };
        cx.observe_self(|view, cx| view.publish_mpris(cx)).detach();
        let mut view = Self {
            volume,
            settings_path,
            reporter,
            mpris,
            _mpris_commands: mpris_commands,
            ..Self::default()
        };
        view.publish_mpris(cx);
        view
    }

    fn publish_mpris(&mut self, _cx: &mut Context<Self>) {
        let Some(bridge) = &self.mpris else {
            return;
        };
        let image = self.cover.as_ref();
        if self.mpris_artwork.as_ref().map(|art| art.0) != image.map(|image| image.id()) {
            self.mpris_artwork = image.map(|image| (image.id(), Arc::from(image.bytes())));
        }
        let current = self.queue.tracks.get(self.queue.position);
        let active = matches!(self.state, State::Playing | State::Paused | State::Loading);
        let metadata = current
            .map(|entry| mpris::Metadata {
                entry: Some(entry.id),
                title: entry.track.name.clone(),
                album: entry.album.name.clone(),
                artists: artist_credits(&entry.track, &entry.album)
                    .into_iter()
                    .map(|artist| artist.name)
                    .collect(),
                album_artists: entry
                    .album
                    .album_artists
                    .as_ref()
                    .map(|artists| artists.iter().map(|artist| artist.name.clone()).collect())
                    .unwrap_or_else(|| vec![entry.album.artist()]),
                length: entry
                    .track
                    .run_time_ticks
                    .map(|ticks| (ticks / 10).min(i64::MAX as u64) as i64),
                track_number: entry.track.index_number,
                disc_number: entry.track.parent_index_number,
            })
            .unwrap_or_default();
        let can_seek = active && metadata.length.is_some_and(|length| length > 0);
        bridge.publish(mpris::Snapshot {
            metadata,
            status: match self.state {
                State::Playing | State::Loading => "Playing",
                State::Paused => "Paused",
                _ => "Stopped",
            },
            position: mpris::microseconds(self.current_position().unwrap_or(self.elapsed)),
            volume: f64::from(self.volume.level()),
            can_next: current.is_some() && self.queue.position + 1 < self.queue.tracks.len(),
            can_previous: current.is_some() && self.queue.position > 0,
            can_play: current.is_some(),
            can_pause: current.is_some(),
            can_seek,
            artwork: self.mpris_artwork.clone(),
        });
    }

    fn media_command(&mut self, command: mpris::Command, cx: &mut Context<Self>) {
        self.poll_events(cx);
        match command {
            mpris::Command::Play if !matches!(self.state, State::Playing | State::Loading) => {
                self.toggle(cx)
            }
            mpris::Command::Pause if matches!(self.state, State::Playing | State::Loading) => {
                self.toggle(cx)
            }
            mpris::Command::PlayPause => self.toggle(cx),
            mpris::Command::Stop => self.stop(cx),
            mpris::Command::Next => self.media_step(true, cx),
            mpris::Command::Previous => self.media_step(false, cx),
            mpris::Command::Seek(offset) => {
                if let Some(length) = self.seek_length() {
                    let position =
                        mpris::microseconds(self.current_position().unwrap_or(self.elapsed))
                            .saturating_add(offset)
                            .max(0);
                    if position >= length {
                        if self.queue.position + 1 < self.queue.tracks.len() {
                            self.next(cx);
                        } else {
                            self.stop(cx);
                        }
                    } else {
                        self.seek_position(Duration::from_micros(position as u64), cx);
                    }
                }
            }
            mpris::Command::SetPosition { track, position } => {
                if let Some(position) = self.media_seek_target(&track, position) {
                    self.seek_position(position, cx);
                }
            }
            mpris::Command::Volume(volume) => {
                self.volume.set(volume as f32);
                self.report_current(true);
                self.save_volume(cx);
            }
            mpris::Command::Raise => cx.activate(true),
            mpris::Command::Quit => cx.quit(),
            _ => {}
        }
        cx.notify();
    }

    fn media_step(&mut self, next: bool, cx: &mut Context<Self>) {
        if matches!(self.state, State::Playing | State::Paused | State::Loading) {
            if next {
                self.next(cx);
            } else {
                self.previous(cx);
            }
        } else if if next {
            self.queue.advance()
        } else {
            self.queue.previous()
        } {
            self.stop(cx);
            self.reset_cover(None, cx);
            self.refresh_prefetch();
        }
    }

    fn seek_length(&self) -> Option<i64> {
        matches!(self.state, State::Playing | State::Paused | State::Loading)
            .then(|| {
                self.queue
                    .current()?
                    .run_time_ticks
                    .map(|ticks| (ticks / 10).min(i64::MAX as u64) as i64)
            })
            .flatten()
            .filter(|length| *length > 0)
    }

    fn media_seek_target(&self, track: &str, position: i64) -> Option<Duration> {
        let length = self.seek_length()?;
        let current = self.queue.tracks.get(self.queue.position)?;
        (mpris::track_path(current.id) == track && position >= 0 && position < length)
            .then(|| Duration::from_micros(position as u64))
    }

    fn current_position(&self) -> Option<Duration> {
        let handle = self.handle.as_ref()?;
        let entry = self.queue.tracks.get(self.queue.position)?.id;
        handle.position(entry).map(|(position, _)| position)
    }

    fn report_current(&self, force: bool) {
        let (Some(reporter), Some(handle), Some(entry)) = (
            &self.reporter,
            &self.handle,
            self.queue.tracks.get(self.queue.position),
        ) else {
            return;
        };
        let Some((position, frames)) = handle.position(entry.id) else {
            return;
        };
        // Queued/paused selections that have never emitted PCM must not create
        // phantom plays. An existing session can still report a paused seek.
        if frames == 0 && !reporter.is_active() {
            return;
        }
        self.report_position(position, force);
    }

    fn report_position(&self, position: Duration, force: bool) {
        let (Some(reporter), Some(client), Some(track)) =
            (&self.reporter, &self.client, self.queue.current())
        else {
            return;
        };
        reporter.update(
            client.clone(),
            Snapshot {
                item_id: track.id.clone(),
                position,
                paused: self.state == State::Paused,
                volume: self.volume.level(),
                can_seek: track.run_time_ticks.is_some_and(|ticks| ticks > 0),
                queue: self
                    .queue
                    .tracks
                    .iter()
                    .skip(self.queue.position)
                    .take(QUEUE_WINDOW)
                    .map(|entry| entry.track.id.clone())
                    .collect(),
            },
            force,
        );
    }

    fn stop_reporting(&self, failed: bool) {
        if let Some(reporter) = &self.reporter {
            // The queue may already point at the next track. Only update the
            // position here; the reporter retains the old track's identity.
            reporter.stop(self.current_position(), failed);
        }
    }

    fn save_volume(&self, cx: &Context<Self>) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };
        let volume = self.volume.clone();
        cx.background_executor()
            .spawn(async move {
                if let Err(error) = settings::save_volume(&path, &volume) {
                    tracing::warn!(%error, "Could not save volume setting");
                }
            })
            .detach();
    }

    pub fn play_album(
        &mut self,
        client: Client,
        album: Item,
        tracks: Vec<Item>,
        start: usize,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
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

    fn request_play(&mut self, request: AlbumRequest, cx: &mut Context<Self>) {
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

    fn prepare_play(&mut self, request: AlbumRequest) -> bool {
        let discards_queue = !self.queue.tracks.is_empty()
            || self.album_request.is_some()
            || !self.album_requests.is_empty();
        self.pending_play = Some(request);
        discards_queue
    }

    fn take_play_confirmation(&mut self, confirmed: bool) -> Option<AlbumRequest> {
        self.pending_play.take().filter(|_| confirmed)
    }

    pub fn confirm_queue_replacement(&mut self, confirmed: bool, cx: &mut Context<Self>) {
        if let Some(request) = self.take_play_confirmation(confirmed) {
            self.cancel_album_requests();
            self.album_requests.push_back(request);
            self.process_album_requests(cx);
        }
    }

    fn play_ready(
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
        client: Client,
        album: Item,
        tracks: Vec<Item>,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
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
        client: Client,
        album: Item,
        cover: Option<Arc<Image>>,
        cx: &mut Context<Self>,
    ) {
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

    fn cancel_album_requests(&mut self) {
        self.album_request_revision = self.album_request_revision.wrapping_add(1);
        self.album_request = None;
        self.album_requests.clear();
        self.pending_play = None;
    }

    fn process_album_requests(&mut self, cx: &mut Context<Self>) {
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

    fn append_tracks(
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

    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.cancel_album_requests();
        self.stop(cx);
        self.queue.clear();
        self.client = None;
        self.error = None;
        self.cover = None;
        self.cover_album_id = None;
        self.cover_task = None;
        self.update_thumbnail(1., cx);
    }

    fn reset_cover(&mut self, cover: Option<Arc<Image>>, cx: &mut Context<Self>) {
        self.cover_task = None;
        self.cover = cover;
        self.cover_album_id = self.queue.current_album().map(|album| album.id.clone());
        if self.cover.is_none()
            && let Some(client) = self.client.clone()
        {
            self.load_cover(client, cx);
        }
    }

    fn load_cover(&mut self, client: Client, cx: &mut Context<Self>) {
        let Some(album) = self.queue.current_album() else {
            return;
        };
        let id = album.id.clone();
        let requested_id = id.clone();
        let request = cx
            .background_executor()
            .spawn(async move { client.cover(&requested_id) });
        self.cover_task = Some(cx.spawn(async move |this, cx| {
            let result = request.await;
            let _ = this.update(cx, |view, cx| {
                if view
                    .queue
                    .current_album()
                    .is_none_or(|album| album.id != id)
                {
                    return;
                }
                match result {
                    Ok(bytes) => {
                        view.cover =
                            bytes.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Png, bytes)))
                    }
                    Err(Error::Unauthorized) => cx.emit(SessionRejected),
                    Err(error) => tracing::warn!(%error, "Could not load player artwork"),
                }
                cx.notify();
            });
        }));
    }

    fn update_thumbnail(&mut self, scale_factor: f32, cx: &mut Context<Self>) {
        let pixels = (PLAYER_HEIGHT * scale_factor).round().max(1.) as u32;
        let key = self.cover.as_ref().map(|cover| (cover.id(), pixels));
        if self.thumbnail_key == key {
            return;
        }
        self.thumbnail = None;
        self.thumbnail_task = None;
        self.thumbnail_key = key;
        let (Some(key), Some(cover)) = (key, self.cover.clone()) else {
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

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.mpris_seek_pending = false;
        self.stop_reporting(false);
        self.handle = None;
        self.audio_file = None;
        self.next_audio = None;
        self.poll = None;
        self.state = State::Stopped;
        self.elapsed = Duration::ZERO;
        self.offset = Duration::ZERO;
        self.seek_drag = None;
        self.volume_dragging = false;
        self.error = None;
        tracing::info!("Playback stopped");
        cx.notify();
    }

    fn start(&mut self, paused: bool, cx: &mut Context<Self>) {
        self.mpris_seek_pending = false;
        self.stop_reporting(false);
        self.audio_file = None;
        let next = self.take_prefetched_audio();
        self.audio_file = next.as_ref().map(|next| next.source.clone());
        if self.queue.current_album().map(|album| &album.id) != self.cover_album_id.as_ref() {
            self.reset_cover(None, cx);
        }
        self.start_at(
            paused,
            Duration::ZERO,
            next.and_then(|next| next.decoder),
            cx,
        );
        self.refresh_prefetch();
    }

    fn take_prefetched_audio(&mut self) -> Option<PrefetchedAudio> {
        let current = self.queue.tracks.get(self.queue.position)?;
        if self.next_audio.as_ref()?.entry != current.id {
            return None;
        }
        self.next_audio
            .take()
            .filter(|next| !next.source.failed())
            .inspect(|next| {
                tracing::debug!(entry = next.entry, "Playing preloaded queue entry");
                next.source.promote();
            })
    }

    fn refresh_prefetch(&mut self) {
        let next = self.queue.tracks.get(self.queue.position + 1);
        if self.next_audio.as_ref().map(|next| next.entry) == next.map(|entry| entry.id) {
            self.update_output_next();
            return;
        }
        self.next_audio = None;
        if let (Some(entry), Some(client)) = (next, self.client.clone()) {
            match AudioFile::prefetch(client, entry.track.id.clone()) {
                Ok(source) => {
                    tracing::debug!(entry = entry.id, "Preloading next queued track");
                    let decoder =
                        match audio::Prepared::new(entry.id, source.clone(), Duration::ZERO) {
                            Ok(decoder) => Some(decoder),
                            Err(error) => {
                                tracing::warn!(%error, "Could not prepare next-track decoder");
                                None
                            }
                        };
                    self.next_audio = Some(PrefetchedAudio {
                        entry: entry.id,
                        source,
                        decoder,
                    });
                }
                Err(error) => tracing::warn!(%error, "Could not start next-track preload"),
            }
        }
        self.update_output_next();
    }

    fn update_output_next(&self) {
        if let (Some(handle), Some(current)) =
            (&self.handle, self.queue.tracks.get(self.queue.position))
        {
            handle.set_next(
                current.id,
                self.next_audio
                    .as_ref()
                    .and_then(|next| next.decoder.clone()),
            );
        }
    }

    fn start_at(
        &mut self,
        paused: bool,
        position: Duration,
        prepared: Option<audio::Prepared>,
        cx: &mut Context<Self>,
    ) {
        // Drop cancels the old worker before another one is started. Its event
        // receiver is dropped too, so stale completion cannot advance this queue.
        self.handle = None;
        self.poll = None;
        self.elapsed = position;
        self.offset = position;
        self.seek_drag = None;
        self.error = None;
        let (Some(client), Some(track)) = (self.client.clone(), self.queue.current()) else {
            return;
        };
        tracing::info!(
            position = self.queue.position + 1,
            tracks = self.queue.tracks.len(),
            "Starting queued track"
        );
        let result = (|| {
            if self.audio_file.as_ref().is_none_or(AudioFile::failed) {
                self.audio_file = Some(AudioFile::new(client, track.id.clone())?);
            }
            let prepared = match prepared {
                Some(prepared) => prepared,
                None => audio::Prepared::new(
                    self.queue.tracks[self.queue.position].id,
                    self.audio_file.as_ref().unwrap().clone(),
                    position,
                )?,
            };
            audio::Handle::start(prepared, paused, self.volume.clone())
        })();
        match result {
            Ok(handle) => {
                self.handle = Some(handle);
                self.update_output_next();
                self.state = if paused {
                    State::Paused
                } else {
                    State::Loading
                };
            }
            Err(error) => {
                self.stop_reporting(true);
                self.error = Some(error.to_string());
                self.state = State::Failed;
                cx.notify();
                return;
            }
        }
        self.poll = Some(cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(100))
                    .await;
                if !this
                    .update(cx, |view, cx| view.poll_events(cx))
                    .unwrap_or(false)
                {
                    break;
                }
            }
        }));
        cx.notify();
    }

    fn select_output_entry(&mut self, entry: usize, cx: &mut Context<Self>) {
        if !self.follow_output_entry(entry) {
            return;
        }
        if self.queue.current_album().map(|album| &album.id) != self.cover_album_id.as_ref() {
            self.reset_cover(None, cx);
        }
        self.refresh_prefetch();
    }

    fn follow_output_entry(&mut self, entry: usize) -> bool {
        let Some(position) = self.queue.tracks.iter().position(|track| track.id == entry) else {
            return false;
        };
        if self.queue.position == position {
            return false;
        }
        self.queue.position = position;
        self.audio_file = self.handle.as_ref().and_then(|handle| handle.source(entry));
        if self
            .next_audio
            .as_ref()
            .is_some_and(|next| next.entry == entry)
        {
            let next = self.next_audio.take().unwrap();
            if self.audio_file.is_none() {
                self.audio_file = Some(next.source);
            }
        }
        self.seek_drag = None;
        self.offset = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.mpris_seek_pending = false;
        true
    }

    fn poll_events(&mut self, cx: &mut Context<Self>) -> bool {
        if self.handle.is_none() {
            return false;
        }
        let mut finished = false;
        let mut failure = None;
        loop {
            let event = match self.handle.as_ref().unwrap().events.try_recv() {
                Ok(event) => event,
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    if !finished && failure.is_none() {
                        failure = Some((
                            self.queue.tracks[self.queue.position].id,
                            Error::Message(
                                "The audio worker stopped unexpectedly. Retry this track.",
                            ),
                        ));
                    }
                    break;
                }
            };
            match event {
                Event::Ready { entry, position } => {
                    if self
                        .queue
                        .tracks
                        .get(self.queue.position)
                        .is_some_and(|track| track.id == entry)
                    {
                        self.offset = position;
                        self.elapsed = position;
                        if self.mpris_seek_pending {
                            self.mpris_seek_pending = false;
                            self.publish_mpris(cx);
                            if let Some(bridge) = &self.mpris {
                                bridge.seeked(entry, position);
                            }
                        }
                        if self.reporter.as_ref().is_some_and(Reporter::is_active) {
                            self.report_current(true);
                        }
                    }
                }
                Event::Started {
                    entry,
                    rate,
                    position,
                } => {
                    self.select_output_entry(entry, cx);
                    tracing::debug!(entry, rate, "Queued track reached audio output");
                    self.offset = position;
                    self.elapsed = position;
                    if self.state == State::Loading {
                        self.state = State::Playing;
                    }
                    // Use this boundary's position even if a very short next
                    // track has already reached output before the UI polls.
                    self.report_position(position, true);
                }
                Event::Ended { entry, position } => {
                    if self
                        .queue
                        .tracks
                        .get(self.queue.position)
                        .is_some_and(|track| track.id == entry)
                    {
                        self.elapsed = position;
                        if let Some(reporter) = &self.reporter {
                            reporter.stop(Some(position), false);
                        }
                    }
                }
                Event::Finished => finished = true,
                Event::Failed { entry, error } => failure = Some((entry, error)),
            }
        }
        if let Some(position) = self.current_position() {
            self.elapsed = position;
        }
        self.report_current(false);
        if let Some((entry, error)) = failure {
            self.mpris_seek_pending = false;
            self.select_output_entry(entry, cx);
            self.stop_reporting(true);
            let unauthorized = matches!(error, Error::Unauthorized);
            self.error = Some(error.to_string());
            self.state = State::Failed;
            self.handle = None;
            if unauthorized {
                cx.emit(SessionRejected);
            }
        } else if finished {
            self.stop_reporting(false);
            self.handle = None;
            if self.queue.advance() {
                self.start(self.state == State::Paused, cx);
                // start installed a new polling task for the next track.
                return false;
            }
            self.state = State::Finished;
            tracing::info!("Queue playback finished");
        }
        cx.notify();
        self.handle.is_some()
    }

    fn toggle(&mut self, cx: &mut Context<Self>) {
        self.poll_events(cx);
        match self.state {
            State::Loading | State::Playing | State::Paused => {
                let paused = self.state != State::Paused;
                if let Some(handle) = &self.handle {
                    handle.pause(paused);
                }
                self.state = if paused {
                    State::Paused
                } else {
                    State::Playing
                };
                tracing::info!(paused, "Playback pause changed");
            }
            State::Finished => {
                if !self.queue.advance() {
                    self.queue.position = 0;
                }
                self.start(false, cx);
            }
            State::Failed => self.start_at(false, self.offset, None, cx),
            _ => self.start(false, cx),
        }
        self.report_current(true);
        cx.notify();
    }

    fn previous(&mut self, cx: &mut Context<Self>) {
        self.poll_events(cx);
        if self.queue.previous() {
            self.start(self.state == State::Paused, cx);
        }
    }

    fn next(&mut self, cx: &mut Context<Self>) {
        self.poll_events(cx);
        if self.queue.advance() {
            self.start(self.state == State::Paused, cx);
        }
    }

    fn jump_to(&mut self, entry: usize, cx: &mut Context<Self>) {
        self.poll_events(cx);
        let Some(position) = self.queue.tracks.iter().position(|track| track.id == entry) else {
            return;
        };
        self.stop_reporting(false);
        tracing::info!(entry, position = position + 1, "Jumping to queued track");
        if self.queue.position == position {
            self.mpris_seek_pending = true;
            self.start_at(false, Duration::ZERO, None, cx);
        } else {
            self.queue.position = position;
            self.start(false, cx);
        }
    }

    fn remove_from_queue(&mut self, entry: usize, cx: &mut Context<Self>) {
        self.poll_events(cx);
        let Some(index) = self.queue.tracks.iter().position(|track| track.id == entry) else {
            return;
        };
        let has_successor = index + 1 < self.queue.tracks.len();
        let active = matches!(self.state, State::Playing | State::Paused | State::Loading);
        let paused = self.state == State::Paused;
        let selected = self
            .queue
            .tracks
            .get(self.queue.position)
            .map(|track| track.id);
        let actual = self
            .handle
            .as_ref()
            .and_then(|handle| handle.remove_entry(entry))
            .or(selected);
        let removing_current = actual == Some(entry);
        if removing_current || actual != selected {
            self.stop_reporting(false);
        }
        if let Some(actual) = actual {
            self.follow_output_entry(actual);
        }
        self.queue.remove(entry);
        self.queue_reorder = None;
        if removing_current {
            if active && has_successor {
                self.start(paused, cx);
                return;
            }
            self.stop(cx);
        }
        if self.queue.current_album().map(|album| &album.id) != self.cover_album_id.as_ref() {
            self.reset_cover(None, cx);
        }
        self.refresh_prefetch();
        self.report_current(true);
        cx.notify();
    }

    fn seek_to(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.poll_events(cx);
        let Some(total) = self
            .queue
            .current()
            .and_then(|track| track.run_time_ticks)
            .filter(|&total| total > 0)
        else {
            self.seek_drag = None;
            return;
        };
        let ticks = ((total as f64 * f64::from(fraction.clamp(0., 1.))) as u64).min(total - 1);
        let position = Duration::from_secs(ticks / 10_000_000)
            + Duration::from_nanos((ticks % 10_000_000) * 100);
        self.seek_position(position, cx);
    }

    fn seek_position(&mut self, position: Duration, cx: &mut Context<Self>) {
        self.mpris_seek_pending = true;
        tracing::info!(seconds = position.as_secs_f64(), "Seeking current track");
        self.start_at(self.state == State::Paused, position, None, cx);
    }

    fn slider_bounds(&self, slider: Slider) -> Bounds<Pixels> {
        match slider {
            Slider::Seek => self.seek_bounds,
            Slider::Volume => self.volume_bounds,
        }
    }

    fn slider_dragging(&self, slider: Slider) -> bool {
        match slider {
            Slider::Seek => self.seek_drag.is_some(),
            Slider::Volume => self.volume_dragging,
        }
    }

    fn cancel_slider_drag(&mut self, slider: Slider, cx: &Context<Self>) {
        match slider {
            Slider::Seek => self.seek_drag = None,
            Slider::Volume => {
                self.volume_dragging = false;
                self.save_volume(cx);
                self.report_current(true);
            }
        }
    }

    fn update_slider(
        &mut self,
        slider: Slider,
        fraction: f32,
        released: bool,
        cx: &mut Context<Self>,
    ) {
        match slider {
            Slider::Seek if released => self.seek_to(fraction, cx),
            Slider::Seek => self.seek_drag = Some(fraction),
            Slider::Volume => {
                self.volume.set(fraction);
                self.volume_dragging = !released;
                if released {
                    self.save_volume(cx);
                    self.report_current(true);
                }
            }
        }
        cx.notify();
    }

    fn slider(
        &self,
        slider: Slider,
        progress: f32,
        enabled: bool,
        cx: &Context<Self>,
    ) -> Stateful<Div> {
        let entity = cx.entity().downgrade();
        div()
            .id(match slider {
                Slider::Seek => "seek-bar",
                Slider::Volume => "volume-bar",
            })
            .relative()
            .flex_1()
            .min_w_0()
            .h(px(TIMELINE_HEIGHT))
            .flex()
            .items_center()
            .when(enabled, |bar| bar.cursor_pointer())
            .when(slider == Slider::Volume, |bar| {
                bar.focusable().tab_stop(true).on_key_down(cx.listener(
                    |view, event: &KeyDownEvent, window, cx| {
                        let level = view.volume.level();
                        let level = match event.keystroke.key.as_str() {
                            "left" | "down" => level - 0.05,
                            "right" | "up" => level + 0.05,
                            "home" => 0.,
                            "end" => 1.,
                            _ => return,
                        };
                        view.volume.set(level);
                        view.save_volume(cx);
                        view.report_current(true);
                        window.prevent_default();
                        cx.stop_propagation();
                        cx.notify();
                    },
                ))
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |view, event: &MouseDownEvent, _, cx| {
                    if enabled {
                        if let Some(fraction) =
                            pointer_fraction(event.position.x, view.slider_bounds(slider))
                        {
                            view.update_slider(slider, fraction, false, cx);
                        }
                        cx.stop_propagation();
                    }
                }),
            )
            .child(
                div()
                    .w_full()
                    .h(px(if slider == Slider::Volume { 6. } else { 4. }))
                    .rounded_full()
                    .bg(rgb(theme::SURFACE1))
                    .overflow_hidden()
                    .child(
                        div()
                            .h_full()
                            .w(relative(progress))
                            .rounded_full()
                            .bg(rgb(theme::TEXT)),
                    ),
            )
            .when(enabled && slider == Slider::Seek, |bar| {
                bar.child(
                    div()
                        .absolute()
                        .left(relative(progress))
                        .ml(px(-4.))
                        .top(px((TIMELINE_HEIGHT - 8.) / 2.))
                        .size(px(8.))
                        .rounded_full()
                        .bg(rgb(if self.slider_dragging(slider) {
                            theme::LAVENDER
                        } else {
                            theme::TEXT
                        })),
                )
            })
            .child(
                canvas(
                    move |bounds, _, cx| {
                        let _ = entity.update(cx, |view, _| match slider {
                            Slider::Seek => view.seek_bounds = bounds,
                            Slider::Volume => view.volume_bounds = bounds,
                        });
                    },
                    {
                        let entity = cx.entity().downgrade();
                        move |_, _, window, _| {
                            let moved = entity.clone();
                            window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture {
                                    return;
                                }
                                let _ = moved.update(cx, |view, cx| {
                                    if view.slider_dragging(slider) {
                                        if event.dragging() {
                                            if let Some(fraction) = pointer_fraction(
                                                event.position.x,
                                                view.slider_bounds(slider),
                                            ) {
                                                view.update_slider(slider, fraction, false, cx);
                                            }
                                        } else {
                                            view.cancel_slider_drag(slider, cx);
                                        }
                                        cx.notify();
                                    }
                                });
                            });
                            window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                                if phase != DispatchPhase::Capture
                                    || event.button != MouseButton::Left
                                {
                                    return;
                                }
                                let _ = entity.update(cx, |view, cx| {
                                    if view.slider_dragging(slider) {
                                        if let Some(fraction) = pointer_fraction(
                                            event.position.x,
                                            view.slider_bounds(slider),
                                        ) {
                                            view.update_slider(slider, fraction, true, cx);
                                        } else {
                                            view.cancel_slider_drag(slider, cx);
                                            cx.notify();
                                        }
                                    }
                                });
                            });
                        }
                    },
                )
                .absolute()
                .top_0()
                .left_0()
                .size_full(),
            )
    }

    fn move_queue_drag(&mut self, event: &DragMoveEvent<QueueDrag>, cx: &mut Context<Self>) {
        let payload = event.drag(cx);
        if payload.owner != cx.entity_id() || payload.revision != self.queue.revision() {
            return;
        }
        self.update_queue_gap(event.event.position, event.bounds, cx);
    }

    fn update_queue_gap(
        &mut self,
        position: Point<Pixels>,
        bounds: Bounds<Pixels>,
        cx: &mut Context<Self>,
    ) {
        let Some(reorder) = self.queue_reorder.as_mut() else {
            return;
        };
        if reorder.revision != self.queue.revision() {
            return;
        }
        let old = *reorder;
        reorder.inside = bounds.contains(&position);
        if reorder.inside {
            let y = f32::from(position.y - bounds.origin.y - self.queue_scroll.offset().y)
                - QUEUE_PADDING;
            reorder.before =
                queue_drop_before(reorder.from, reorder.before, y, self.queue.tracks.len());
        } else {
            reorder.before = reorder.from;
        }
        if *reorder != old {
            cx.notify();
        }
    }

    fn queue_view(&mut self, cx: &Context<Self>) -> impl IntoElement + use<> {
        if self.queue_reorder.is_some_and(|reorder| {
            !cx.has_active_drag() || reorder.revision != self.queue.revision()
        }) {
            self.queue_reorder = None;
        }
        let mut content = div().relative().w_full();
        if !self.queue.tracks.is_empty() {
            content = content.h(px(self.queue.tracks.len() as f32 * QUEUE_ROW_PITCH - 4.));
        }
        if self.queue.tracks.is_empty() {
            content = content.child(
                div()
                    .p_4()
                    .text_sm()
                    .text_color(rgb(theme::SUBTEXT0))
                    .child("Play or queue an album to add tracks."),
            );
        }
        for (index, entry) in self.queue.tracks.iter().enumerate() {
            let entry_id = entry.id;
            let track = &entry.track;
            let current = index == self.queue.position && self.state != State::Stopped;
            let entity = cx.entity().downgrade();
            let reorder = self.queue_reorder.filter(|reorder| reorder.inside);
            let destination = reorder.map_or(index, |reorder| {
                queue_preview_index(index, reorder.from, reorder.before)
            });
            let lifted = reorder.is_some_and(|reorder| reorder.from == index);
            let row = div()
                .id("queue-track-content")
                .group("queue-row")
                .absolute()
                .left_0()
                .w_full()
                .h(px(QUEUE_ROW_HEIGHT))
                .flex()
                .items_center()
                .gap_3()
                .px_3()
                .py_3()
                .rounded_md()
                .cursor_pointer()
                .on_click(cx.listener(move |view, _, _, cx| view.jump_to(entry_id, cx)))
                .on_mouse_down(
                    MouseButton::Right,
                    cx.listener(move |view, event: &MouseDownEvent, window, cx| {
                        if let Some(entry) =
                            view.queue.tracks.iter().find(|entry| entry.id == entry_id)
                        {
                            cx.emit(TrackMenuRequest {
                                target: TrackTarget {
                                    track: entry.track.clone(),
                                    album: entry.album.clone(),
                                    cover: None,
                                },
                                position: event.position,
                            });
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                    }),
                )
                .on_drag(
                    QueueDrag {
                        owner: cx.entity_id(),
                        revision: self.queue.revision(),
                        index,
                        title: track.name.clone(),
                        subtitle: format!("{} · {}", track.artist(), entry.album.name),
                    },
                    move |drag, _, _, cx| {
                        let playback = entity.clone();
                        let _ = entity.update(cx, |view, cx| {
                            view.queue_reorder = Some(QueueReorder {
                                from: drag.index,
                                before: drag.index,
                                revision: drag.revision,
                                inside: true,
                            });
                            cx.notify();
                        });
                        cx.new(|cx| {
                            cx.on_release(move |_, cx| {
                                let _ = playback.update(cx, |view, cx| {
                                    view.queue_reorder = None;
                                    cx.notify();
                                });
                            })
                            .detach();
                            drag.clone()
                        })
                    },
                )
                .when(current, |row| {
                    row.bg(rgb(theme::SURFACE0)).text_color(rgb(theme::MAUVE))
                })
                .child(
                    div()
                        .w(px(20.))
                        .flex_shrink_0()
                        .text_sm()
                        .child((index + 1).to_string()),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .gap_1()
                        .child(div().truncate().child(track.name.clone()))
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(format!("{} · {}", track.artist(), entry.album.name)),
                        ),
                )
                .child(
                    div()
                        .flex_shrink_0()
                        .text_sm()
                        .text_color(rgb(theme::SUBTEXT0))
                        .child(duration(track.run_time_ticks)),
                )
                .child(
                    div()
                        .id("remove-queue-entry")
                        .focusable()
                        .aria_label("Remove from queue")
                        .size(px(24.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .text_lg()
                        .text_color(rgb(theme::SUBTEXT0))
                        .opacity(0.)
                        .group_hover("queue-row", |style| style.opacity(1.))
                        .focus(|style| style.opacity(1.).text_color(rgb(theme::RED)))
                        .hover(|style| style.text_color(rgb(theme::RED)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .on_click(cx.listener(move |view, _, _, cx| {
                            view.remove_from_queue(entry_id, cx);
                            cx.stop_propagation();
                        }))
                        .child("×"),
                )
                .with_spring(
                    "queue-position",
                    SpringAnimation::new(QUEUE_SPRING)
                        .to(px(destination as f32 * QUEUE_ROW_PITCH))
                        .with_epsilon(0.25),
                    move |row, top| row.top(top - px(index as f32 * QUEUE_ROW_PITCH)),
                )
                .with_spring(
                    "queue-lift",
                    SpringAnimation::new(QUEUE_SPRING).to(if lifted { 0. } else { 1. }),
                    |row, opacity| row.map_element(|row| row.opacity(opacity.clamp(0., 1.))),
                );
            content = content.child(
                div()
                    .id(("queue-track", entry.id))
                    .absolute()
                    .left_0()
                    .w_full()
                    .top(px(index as f32 * QUEUE_ROW_PITCH))
                    .h(px(QUEUE_ROW_HEIGHT))
                    .child(row),
            );
        }
        div()
            .size_full()
            .min_h_0()
            .flex()
            .flex_col()
            .when(!self.queue.tracks.is_empty(), |sidebar| {
                sidebar.child(
                    div()
                        .p_4()
                        .border_b_1()
                        .border_color(rgb(theme::SURFACE0))
                        .child(
                            div()
                                .truncate()
                                .text_sm()
                                .text_color(rgb(theme::SUBTEXT0))
                                .child(format!("{} tracks", self.queue.tracks.len())),
                        ),
                )
            })
            .child(
                div()
                    .id("queue-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.queue_scroll)
                    .on_scroll(cx.listener(|view, _, window, cx| {
                        if view.queue_reorder.is_some() && cx.has_active_drag() {
                            cx.defer_in(window, |view, window, cx| {
                                view.update_queue_gap(
                                    window.mouse_position(),
                                    view.queue_scroll.bounds(),
                                    cx,
                                );
                            });
                        }
                    }))
                    .on_drag_move(
                        cx.listener(|view, event: &DragMoveEvent<QueueDrag>, _, cx| {
                            view.move_queue_drag(event, cx)
                        }),
                    )
                    .can_drop({
                        let owner = cx.entity_id();
                        let revision = self.queue.revision();
                        move |value, _, _| {
                            value.downcast_ref::<QueueDrag>().is_some_and(|drag| {
                                drag.owner == owner && drag.revision == revision
                            })
                        }
                    })
                    .on_drop(cx.listener(|view, drag: &QueueDrag, window, cx| {
                        view.poll_events(cx);
                        view.update_queue_gap(
                            window.mouse_position(),
                            view.queue_scroll.bounds(),
                            cx,
                        );
                        let reorder = view.queue_reorder.take();
                        if drag.owner == cx.entity_id()
                            && drag.revision == view.queue.revision()
                            && let Some(reorder) = reorder.filter(|reorder| {
                                reorder.inside && reorder.revision == drag.revision
                            })
                            && view.queue.move_track(drag.index, reorder.before)
                        {
                            tracing::debug!(
                                from = drag.index,
                                before = reorder.before,
                                "Queue reordered"
                            );
                            view.report_current(true);
                            view.refresh_prefetch();
                        }
                        cx.notify();
                    }))
                    .child(div().p_2().child(content)),
            )
    }
}

impl Render for Playback {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.update_thumbnail(window.scale_factor(), cx);
        let track = self.queue.current();
        let has_track = track.is_some();
        let total_ticks = track.and_then(|track| track.run_time_ticks);
        let elapsed_ticks = if let (Some(preview), Some(total)) = (self.seek_drag, total_ticks) {
            (f64::from(preview) * total as f64) as u64
        } else {
            (self.elapsed.as_nanos() / 100).min(u128::from(u64::MAX)) as u64
        };
        let shown_elapsed = total_ticks.map_or(elapsed_ticks, |total| elapsed_ticks.min(total));
        let progress = total_ticks
            .filter(|&total| total > 0)
            .map(|total| (elapsed_ticks as f64 / total as f64).clamp(0., 1.) as f32)
            .unwrap_or(0.);
        let (icon, label) = match self.state {
            State::Loading | State::Playing => (TransportIcon::Pause, "Pause"),
            State::Paused => (TransportIcon::Play, "Resume"),
            State::Failed => (TransportIcon::Retry, "Retry"),
            State::Finished => (TransportIcon::Play, "Play again"),
            State::Stopped => (TransportIcon::Play, "Play"),
        };
        let album_link = if let Some(album) = self.queue.current_album() {
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
                if let Some(cover) = self.thumbnail.as_ref().or(self.cover.as_ref()) {
                    crate::artwork::image(cover.clone())
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
                    !has_track || self.queue.position == 0,
                    false,
                )
                .on_click(cx.listener(|view, _, _, cx| view.previous(cx))),
            )
            .child(
                player_button("play-pause", label, icon, !has_track, true)
                    .on_click(cx.listener(|view, _, _, cx| view.toggle(cx))),
            )
            .child(
                player_button(
                    "next-track",
                    "Next track",
                    TransportIcon::Next,
                    !has_track || self.queue.position + 1 >= self.queue.tracks.len(),
                    false,
                )
                .on_click(cx.listener(|view, _, _, cx| view.next(cx))),
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
                                .child(volume_icon(self.volume.level() == 0.))
                                .child(self.slider(Slider::Volume, self.volume.level(), true, cx)),
                        ),
                    ),
            )
            .when_some(self.error.clone(), |view, error| {
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

fn pointer_fraction(x: Pixels, bounds: Bounds<Pixels>) -> Option<f32> {
    (bounds.size.width > px(0.)).then(|| ((x - bounds.origin.x) / bounds.size.width).clamp(0., 1.))
}

fn volume_icon(muted: bool) -> impl IntoElement {
    let data: &[u8] = if muted {
        include_bytes!("../assets/icons/filled-lucide/volume-x.svg")
    } else {
        include_bytes!("../assets/icons/filled-lucide/volume-2.svg")
    };
    svg()
        .data(data)
        .size(px(16.))
        .flex_shrink_0()
        .text_color(rgb(theme::SUBTEXT0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_seeks_use_microseconds_and_reject_stale_duplicate_entries_and_invalid_positions() {
        let mut player = Playback {
            state: State::Paused,
            ..Playback::default()
        };
        let track =
            item(serde_json::json!({"Id":"same", "Name":"Track", "RunTimeTicks":120_000_000}));
        player.queue.append(
            item(serde_json::json!({"Id":"album", "Name":"Album"})),
            vec![track.clone(), track],
        );
        let first = mpris::track_path(player.queue.tracks[0].id);
        let second = mpris::track_path(player.queue.tracks[1].id);
        assert_eq!(
            player.media_seek_target(&first, 1_500_000),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(player.media_seek_target(&first, 0), Some(Duration::ZERO));
        assert!(player.media_seek_target(&first, -1).is_none());
        assert!(player.media_seek_target(&first, 12_000_000).is_none());
        player.queue.advance();
        assert!(player.media_seek_target(&first, 1_000_000).is_none());
        assert!(player.media_seek_target(&second, 1_000_000).is_some());
        player.state = State::Stopped;
        assert!(player.media_seek_target(&second, 1_000_000).is_none());
    }

    #[test]
    fn album_track_highlight_follows_active_playback_and_clears_on_stop_or_failure() {
        let mut player = Playback::default();
        player.queue.append(
            item(serde_json::json!({"Id":"album", "Name":"Album"})),
            vec![
                item(serde_json::json!({"Id":"a", "Name":"A"})),
                item(serde_json::json!({"Id":"b", "Name":"B"})),
            ],
        );
        for state in [State::Playing, State::Paused, State::Loading] {
            player.state = state;
            assert!(player.is_current_track("a"));
            assert!(!player.is_current_track("b"));
        }
        player.queue.advance();
        assert!(!player.is_current_track("a"));
        assert!(player.is_current_track("b"));
        for state in [State::Stopped, State::Finished, State::Failed] {
            player.state = state;
            assert!(!player.is_current_track("b"));
        }
    }

    #[test]
    fn animated_queue_positions_match_every_possible_committed_move() {
        for from in 0..5 {
            for before in 0..=5 {
                let mut queue = Queue::default();
                queue.append(item(serde_json::json!({"Id":"album", "Name":"Album"})),
                    (0..5).map(|index| item(serde_json::json!({"Id":index.to_string(), "Name":index.to_string()}))).collect());
                queue.move_track(from, before);
                for index in 0..5 {
                    let preview_index = queue_preview_index(index, from, before);
                    assert_eq!(queue.tracks[preview_index].track.id, index.to_string());
                }
            }
        }
    }

    #[test]
    fn queue_gap_stays_open_until_the_pointer_leaves_it_and_supports_both_ends() {
        for fraction in [0., 0.25, 0.5, 0.99] {
            assert_eq!(
                queue_drop_before(2, 4, (3. + fraction) * QUEUE_ROW_PITCH, 5),
                4
            );
            assert_eq!(queue_drop_before(3, 0, fraction * QUEUE_ROW_PITCH, 5), 0);
        }
        assert_eq!(queue_drop_before(2, 4, 4.25 * QUEUE_ROW_PITCH, 5), 4);
        assert_eq!(queue_drop_before(2, 4, 4.75 * QUEUE_ROW_PITCH, 5), 5);
        assert_eq!(queue_drop_before(2, 2, -10., 5), 0);
        assert_eq!(queue_drop_before(2, 2, 10_000., 5), 5);
        assert_eq!(queue_drop_before(2, 2, 0.75 * QUEUE_ROW_PITCH, 5), 1);
        assert_eq!(queue_drop_before(2, 2, 1.25 * QUEUE_ROW_PITCH, 5), 1);
        assert_eq!(queue_drop_before(0, 0, 0., 0), 0);
    }

    fn item(value: serde_json::Value) -> Item {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn prefetch_follows_queue_entry_identity_and_survives_appending() {
        use std::io::{Read, Write};
        let mut player = Playback::default();
        let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
        let track = item(serde_json::json!({"Id":"same-song", "Name":"Same song"}));
        player
            .queue
            .replace(album.clone(), vec![track.clone(), track.clone()], 0);
        let (source, mut connection) = crate::audio_file::tests::stream(Some(4));
        connection.write_all(b"abcd").unwrap();
        let entry = player.queue.tracks[1].id;
        player.next_audio = Some(PrefetchedAudio {
            entry,
            source,
            decoder: None,
        });
        // Duplicate Jellyfin IDs must not consume the next entry's prefix.
        assert!(player.take_prefetched_audio().is_none());
        player.queue.append(album, vec![track]);
        player.refresh_prefetch();
        assert_eq!(player.next_audio.as_ref().unwrap().entry, entry);
        player.queue.advance();
        let source = player.take_prefetched_audio().unwrap();
        assert!(player.next_audio.is_none());
        let mut bytes = Vec::new();
        source
            .source
            .reader(Arc::default())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"abcd");
    }

    #[test]
    fn changing_the_next_queue_entry_discards_its_preload() {
        for replace in [false, true] {
            let mut player = Playback::default();
            let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
            let tracks: Vec<_> = ["first", "second", "third"]
                .into_iter()
                .map(|id| item(serde_json::json!({"Id":id, "Name":id})))
                .collect();
            player.queue.replace(album.clone(), tracks.clone(), 0);
            let (source, _connection) = crate::audio_file::tests::stream(Some(4));
            player.next_audio = Some(PrefetchedAudio {
                entry: player.queue.tracks[1].id,
                source,
                decoder: None,
            });
            if replace {
                player.queue.replace(album, tracks, 0);
            } else {
                player.queue.move_track(2, 1);
            }
            player.refresh_prefetch();
            // No client in this fixture, so replacement fetching is skipped.
            assert!(player.next_audio.is_none());
        }
    }

    fn request(album: &str, start: usize) -> AlbumRequest {
        AlbumRequest {
            action: AlbumAction::Play,
            client: Client::restore(
                serde_json::from_value(serde_json::json!({
                    "server":"http://localhost/", "user_id":"user", "username":"test",
                    "token":"test", "device_id":"test"
                }))
                .unwrap(),
            )
            .unwrap(),
            album: item(serde_json::json!({"Id":album, "Name":album})),
            tracks: Some(vec![
                item(serde_json::json!({"Id":"first", "Name":"First"})),
                item(serde_json::json!({"Id":"second", "Name":"Second"})),
            ]),
            start,
            cover: None,
        }
    }

    #[test]
    fn declining_replacement_preserves_playback_and_pending_queue_work() {
        let mut player = Playback {
            state: State::Paused,
            elapsed: Duration::from_secs(42),
            ..Playback::default()
        };
        let original = request("original", 1);
        player
            .queue
            .replace(original.album, original.tracks.unwrap(), 1);
        let mut queued = request("queued", 0);
        queued.action = AlbumAction::Queue;
        player.album_requests.push_back(queued);
        player.album_request = Some(Task::ready(()));
        let revision = player.album_request_revision;
        assert!(player.prepare_play(request("replacement", 1)));
        assert!(player.take_play_confirmation(false).is_none());
        assert!(player.pending_play.is_none());
        assert_eq!(player.queue.current().unwrap().id, "second");
        assert_eq!(player.queue.current_album().unwrap().id, "original");
        assert_eq!(player.queue.tracks.len(), 2);
        assert_eq!(player.album_request_revision, revision);
        assert!(player.album_request.is_some());
        assert_eq!(player.album_requests.front().unwrap().album.id, "queued");
        assert!(player.state == State::Paused);
        assert_eq!(player.elapsed, Duration::from_secs(42));
    }

    #[test]
    fn confirmed_replacement_keeps_the_clicked_selection_and_can_only_be_taken_once() {
        let mut player = Playback::default();
        let original = request("original", 0);
        player
            .queue
            .replace(original.album, original.tracks.unwrap(), 0);
        assert!(player.prepare_play(request("selected-album", 1)));
        // The current track can advance while the dialog is open.
        player.queue.advance();
        let confirmed = player.take_play_confirmation(true).unwrap();
        assert_eq!(confirmed.album.id, "selected-album");
        assert_eq!(confirmed.start, 1);
        assert_eq!(confirmed.tracks.unwrap()[1].id, "second");
        assert!(player.take_play_confirmation(true).is_none());
        assert_eq!(player.queue.current_album().unwrap().id, "original");
        assert!(player.prepare_play(request("stale-selection", 0)));
        player.cancel_album_requests();
        assert!(player.take_play_confirmation(true).is_none());
    }

    #[test]
    fn empty_queues_play_directly_but_existing_or_pending_queues_require_confirmation() {
        let mut player = Playback::default();
        assert!(!player.prepare_play(request("first-album", 0)));
        player.take_play_confirmation(false);
        player.album_requests.push_back(request("pending-album", 0));
        assert!(player.prepare_play(request("replacement", 0)));
        player.cancel_album_requests();
        player.album_request = Some(Task::ready(()));
        assert!(player.prepare_play(request("replacement", 0)));
        player.cancel_album_requests();
        let finished = request("finished", 1);
        player
            .queue
            .replace(finished.album, finished.tracks.unwrap(), 1);
        player.state = State::Finished;
        assert!(player.prepare_play(request("replacement", 0)));
    }

    #[test]
    fn cancelling_album_requests_preserves_the_current_queue_and_playback_state() {
        let client = Client::restore(
            serde_json::from_value(serde_json::json!({
                "server":"http://localhost/", "user_id":"user", "username":"test",
                "token":"test", "device_id":"test"
            }))
            .unwrap(),
        )
        .unwrap();
        let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
        let track = item(serde_json::json!({"Id":"track", "Name":"Track"}));
        let mut player = Playback {
            state: State::Paused,
            elapsed: Duration::from_secs(42),
            ..Playback::default()
        };
        player.volume.set(0.5);
        player
            .queue
            .replace(album.clone(), vec![track.clone(), track.clone()], 1);
        for action in [AlbumAction::Play, AlbumAction::Queue] {
            player.album_requests.push_back(AlbumRequest {
                action,
                client: client.clone(),
                album: album.clone(),
                cover: None,
                tracks: Some(vec![track.clone()]),
                start: 0,
            });
        }
        player.album_request = Some(Task::ready(()));
        let revision = player.album_request_revision;
        player.cancel_album_requests();
        assert!(player.album_request.is_none());
        assert!(player.album_requests.is_empty());
        assert_ne!(player.album_request_revision, revision);
        assert_eq!(player.queue.tracks.len(), 2);
        assert_eq!(player.queue.position, 1);
        assert_eq!(player.queue.current_album().unwrap().id, "album");
        assert!(player.state == State::Paused);
        assert_eq!(player.elapsed, Duration::from_secs(42));
        assert_eq!(player.volume.level(), 0.5);
    }

    #[test]
    fn player_links_preserve_separate_album_artist_identities() {
        let track = item(serde_json::json!({
            "Id":"song", "Name":"Song", "AlbumArtist":"Two credits",
            "AlbumArtists":[{"Id":"one", "Name":"Same name"}, {"Id":"two", "Name":"Same name"}],
            "ArtistItems":[{"Id":"wrong", "Name":"Same name"}]
        }));
        let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
        let artists = artist_credits(&track, &album);
        assert_eq!(
            artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>(),
            ["Same name", "Same name"]
        );
        assert_eq!(
            artists
                .iter()
                .map(|artist| artist.id.as_deref())
                .collect::<Vec<_>>(),
            [Some("one"), Some("two")]
        );
    }

    #[test]
    fn player_links_match_name_only_credits_without_splitting_artist_names() {
        let track = item(serde_json::json!({
            "Id":"song", "Name":"Song", "Artists":["Earth, Wind & Fire", "Guest"],
            "ArtistItems":[{"Id":"band", "Name":"Earth, Wind & Fire"}]
        }));
        let album = item(serde_json::json!({
            "Id":"album", "Name":"Album", "AlbumArtists":[{"Id":"guest", "Name":"guest"}]
        }));
        let artists = artist_credits(&track, &album);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].name, "Earth, Wind & Fire");
        assert_eq!(artists[0].id.as_deref(), Some("band"));
        assert_eq!(artists[1].id.as_deref(), Some("guest"));
    }

    #[test]
    fn player_links_use_album_credits_and_allow_name_lookup_when_ids_are_missing() {
        let track = item(serde_json::json!({"Id":"song", "Name":"Song", "AlbumArtist":"A & B"}));
        let album = item(serde_json::json!({
            "Id":"album", "Name":"Album", "AlbumArtist":"A & B",
            "AlbumArtists":[{"Id":"a", "Name":"A"}, {"Name":"B"}]
        }));
        let artists = artist_credits(&track, &album);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].id.as_deref(), Some("a"));
        assert_eq!(artists[1].name, "B");
        assert!(artists[1].id.is_none());
        let no_credits = item(serde_json::json!({"Id":"song", "Name":"Song"}));
        assert!(artist_credits(&no_credits, &album).is_empty());
    }

    #[test]
    fn seeking_uses_the_bar_bounds_and_clamps_dragging_outside_it() {
        let bounds = Bounds::new(point(px(200.), px(10.)), gpui::size(px(400.), px(20.)));
        assert_eq!(pointer_fraction(px(100.), bounds), Some(0.));
        assert_eq!(pointer_fraction(px(400.), bounds), Some(0.5));
        assert_eq!(pointer_fraction(px(800.), bounds), Some(1.));
        assert_eq!(pointer_fraction(px(0.), Bounds::default()), None);
    }
}

pub(crate) fn artist_credits(track: &Item, album: &Item) -> Vec<Artist> {
    let named = |name: &String| Artist {
        id: None,
        name: name.clone(),
    };
    // Prefer album-artist credits while retaining separate artist identities.
    // A comma can be part of an artist's name, so never split display strings.
    let mut artists =
        if let Some(name) = track.album_artist.as_ref().filter(|name| !name.is_empty()) {
            track
                .album_artists
                .as_ref()
                .filter(|artists| !artists.is_empty())
                .or_else(|| {
                    album.album_artists.as_ref().filter(|artists| {
                        !artists.is_empty() && album.album_artist.as_ref() == Some(name)
                    })
                })
                .cloned()
                .unwrap_or_else(|| vec![named(name)])
        } else if let Some(names) = track.artists.as_ref().filter(|names| !names.is_empty()) {
            names.iter().map(named).collect()
        } else {
            track.artist_items.clone().unwrap_or_default()
        };
    for artist in &mut artists {
        if artist.id.as_deref().is_none_or(str::is_empty) {
            artist.id = track
                .album_artists
                .iter()
                .flatten()
                .chain(track.artist_items.iter().flatten())
                .chain(album.album_artists.iter().flatten())
                .chain(album.artist_items.iter().flatten())
                .find(|credit| {
                    credit.name.eq_ignore_ascii_case(&artist.name)
                        && credit.id.as_deref().is_some_and(|id| !id.is_empty())
                })
                .and_then(|credit| credit.id.clone());
        }
    }
    artists.retain(|artist| !artist.name.trim().is_empty());
    artists
}

fn player_link(id: impl Into<ElementId>, text: String) -> Stateful<Div> {
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

fn player_art_placeholder() -> Div {
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
enum TransportIcon {
    Play,
    Pause,
    Previous,
    Next,
    Retry,
}

fn player_button(
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

pub struct QueueSidebar {
    playback: Entity<Playback>,
}

impl QueueSidebar {
    pub fn new(playback: Entity<Playback>, cx: &mut Context<Self>) -> Self {
        cx.observe(&playback, |_, _, cx| cx.notify()).detach();
        Self { playback }
    }
}

impl Render for QueueSidebar {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .h_full()
            .w(px(300.))
            .max_w(relative(0.4))
            .min_w_0()
            .flex_shrink_0()
            .bg(rgb(theme::MANTLE))
            .border_l_1()
            .border_color(rgb(theme::SURFACE0))
            .child(self.playback.update(cx, |view, cx| view.queue_view(cx)))
    }
}

#[derive(Clone)]
struct QueueDrag {
    owner: EntityId,
    revision: u64,
    index: usize,
    title: String,
    subtitle: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct QueueReorder {
    from: usize,
    before: usize,
    revision: u64,
    inside: bool,
}

fn queue_preview_index(index: usize, from: usize, before: usize) -> usize {
    let to = if before > from { before - 1 } else { before };
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

fn queue_drop_before(from: usize, before: usize, y: f32, count: usize) -> usize {
    if y < 0. {
        return 0;
    }
    if y >= count as f32 * QUEUE_ROW_PITCH {
        return count;
    }
    let slot = (y / QUEUE_ROW_PITCH) as usize;
    // Keep the current target throughout the open gap. Animated rows never
    // change these hit areas, so a stationary pointer cannot flip the target.
    if slot == queue_preview_index(from, from, before) {
        return before;
    }
    slot + usize::from(y % QUEUE_ROW_PITCH >= QUEUE_ROW_PITCH / 2.)
}

impl Render for QueueDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(280.))
            .p_3()
            .rounded_md()
            .bg(rgb(theme::SURFACE0))
            .border_1()
            .border_color(rgb(theme::MAUVE))
            .shadow_md()
            .text_color(rgb(theme::TEXT))
            .flex()
            .flex_col()
            .gap_1()
            .child(div().truncate().child(self.title.clone()))
            .child(
                div()
                    .truncate()
                    .text_sm()
                    .text_color(rgb(theme::SUBTEXT0))
                    .child(self.subtitle.clone()),
            )
    }
}
