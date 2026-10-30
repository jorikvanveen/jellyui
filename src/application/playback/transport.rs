use super::{Playback, SessionRejected};
use crate::{
    application::reporting::Reporter,
    audio::{self, Event},
    domain::{Error, playback::PlaybackState as State},
    infrastructure::cache::audio::AudioFile,
};
use gpui::Context;
use std::time::Duration;

impl Playback {
    pub(super) fn stop(&mut self, cx: &mut Context<Self>) {
        self.transport_revision = self.transport_revision.wrapping_add(1);
        self.mpris_seek_pending = false;
        self.stop_reporting(false);
        self.handle = None;
        self.audio_file = None;
        self.next_audio = None;
        self.poll = None;
        self.state = State::Stopped;
        self.elapsed = Duration::ZERO;
        self.offset = Duration::ZERO;
        self.error = None;
        tracing::info!("Playback stopped");
        cx.notify();
    }

    pub(super) fn start(&mut self, paused: bool, cx: &mut Context<Self>) {
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

    pub(super) fn start_at(
        &mut self,
        paused: bool,
        position: Duration,
        prepared: Option<audio::Prepared>,
        cx: &mut Context<Self>,
    ) {
        // Drop cancels the old worker before another one is started. Its event
        // receiver is dropped too, so stale completion cannot advance this queue.
        self.handle = None;
        self.transport_revision = self.transport_revision.wrapping_add(1);
        self.poll = None;
        self.elapsed = position;
        self.offset = position;
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

    pub(super) fn select_output_entry(&mut self, entry: usize, cx: &mut Context<Self>) {
        if !self.follow_output_entry(entry) {
            return;
        }
        if self.queue.current_album().map(|album| &album.id) != self.cover_album_id.as_ref() {
            self.reset_cover(None, cx);
        }
        self.refresh_prefetch();
    }

    pub(super) fn follow_output_entry(&mut self, entry: usize) -> bool {
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
        self.offset = Duration::ZERO;
        self.elapsed = Duration::ZERO;
        self.mpris_seek_pending = false;
        self.transport_revision = self.transport_revision.wrapping_add(1);
        true
    }

    pub(crate) fn poll_events(&mut self, cx: &mut Context<Self>) -> bool {
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

    pub(crate) fn toggle(&mut self, cx: &mut Context<Self>) {
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

    pub(crate) fn previous(&mut self, cx: &mut Context<Self>) {
        self.poll_events(cx);
        if self.queue.previous() {
            self.start(self.state == State::Paused, cx);
        }
    }

    pub(crate) fn next(&mut self, cx: &mut Context<Self>) {
        self.poll_events(cx);
        if self.queue.advance() {
            self.start(self.state == State::Paused, cx);
        }
    }

    pub(crate) fn jump_to(&mut self, entry: usize, cx: &mut Context<Self>) {
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

    pub(crate) fn remove_from_queue(&mut self, entry: usize, cx: &mut Context<Self>) {
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

    pub(crate) fn seek_to(&mut self, fraction: f32, cx: &mut Context<Self>) {
        self.poll_events(cx);
        let Some(total) = self
            .queue
            .current()
            .and_then(|track| track.run_time_ticks)
            .filter(|&total| total > 0)
        else {
            return;
        };
        let ticks = ((total as f64 * f64::from(fraction.clamp(0., 1.))) as u64).min(total - 1);
        let position = Duration::from_secs(ticks / 10_000_000)
            + Duration::from_nanos((ticks % 10_000_000) * 100);
        self.seek_position(position, cx);
    }

    pub(super) fn seek_position(&mut self, position: Duration, cx: &mut Context<Self>) {
        self.mpris_seek_pending = true;
        tracing::info!(seconds = position.as_secs_f64(), "Seeking current track");
        self.start_at(self.state == State::Paused, position, None, cx);
    }
}
