mod albums;
mod artwork;
mod desktop;
mod prefetch;
mod reporting;
#[cfg(test)]
mod tests;
mod transport;

pub use crate::domain::playback::AlbumAction;
use crate::{
    application::reporting::Reporter,
    audio,
    domain::{Item, playback::PlaybackState as State, queue::Queue},
    infrastructure::{cache::audio::AudioFile, jellyfin::Client, mpris, settings},
};
use gpui::{Context, EventEmitter, Image, Task};
use std::{collections::VecDeque, path::PathBuf, sync::Arc, time::Duration};

pub struct SessionRejected;
pub struct QueueReplacementRequested {
    pub album: String,
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
    transport_revision: u64,
    volume: audio::Volume,
    settings_path: Option<PathBuf>,
    error: Option<String>,
    cover: Option<Arc<Image>>,
    cover_album_id: Option<String>,
    cover_task: Option<Task<()>>,
}

impl EventEmitter<SessionRejected> for Playback {}
impl EventEmitter<QueueReplacementRequested> for Playback {}

impl Playback {
    pub fn connect(&mut self, client: Client, cx: &mut Context<Self>) {
        self.clear(cx);
        self.client = Some(client);
    }

    pub fn queue(&self) -> &Queue {
        &self.queue
    }
    pub fn state(&self) -> State {
        self.state
    }
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
    /// Changes whenever output restarts or moves to another queue entry.
    pub fn transport_revision(&self) -> u64 {
        self.transport_revision
    }
    pub fn volume(&self) -> f32 {
        self.volume.level()
    }
    pub fn cover(&self) -> Option<&Arc<Image>> {
        self.cover.as_ref()
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn set_volume(&mut self, volume: f32, commit: bool, cx: &mut Context<Self>) {
        self.volume.set(volume);
        if commit {
            self.commit_volume(cx);
        }
        cx.notify();
    }
    pub fn commit_volume(&self, cx: &Context<Self>) {
        self.save_volume(cx);
        self.report_current(true);
    }
    pub fn reorder_queue(
        &mut self,
        from: usize,
        before: usize,
        revision: u64,
        cx: &mut Context<Self>,
    ) -> bool {
        self.poll_events(cx);
        if revision != self.queue.revision() || !self.queue.move_track(from, before) {
            return false;
        }
        self.report_current(true);
        self.refresh_prefetch();
        cx.notify();
        true
    }

    pub fn is_current_track(&self, track: &str) -> bool {
        matches!(self.state, State::Playing | State::Paused | State::Loading)
            && self
                .queue
                .current()
                .is_some_and(|current| current.id == track)
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

    fn current_position(&self) -> Option<Duration> {
        let handle = self.handle.as_ref()?;
        let entry = self.queue.tracks.get(self.queue.position)?.id;
        handle.position(entry).map(|(position, _)| position)
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
    }
}
