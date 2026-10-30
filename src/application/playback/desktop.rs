use super::Playback;
use crate::{
    domain::{artist_credits, playback::PlaybackState as State},
    infrastructure::mpris,
};
use gpui::Context;
use std::{sync::Arc, time::Duration};

impl Playback {
    pub(super) fn publish_mpris(&mut self, _cx: &mut Context<Self>) {
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

    pub(super) fn media_command(&mut self, command: mpris::Command, cx: &mut Context<Self>) {
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

    pub(super) fn media_step(&mut self, next: bool, cx: &mut Context<Self>) {
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

    pub(super) fn seek_length(&self) -> Option<i64> {
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

    pub(super) fn media_seek_target(&self, track: &str, position: i64) -> Option<Duration> {
        let length = self.seek_length()?;
        let current = self.queue.tracks.get(self.queue.position)?;
        (mpris::track_path(current.id) == track && position >= 0 && position < length)
            .then(|| Duration::from_micros(position as u64))
    }
}
