use super::Playback;
use crate::{
    application::reporting::{QUEUE_WINDOW, Snapshot},
    domain::playback::PlaybackState as State,
    infrastructure::settings,
};
use gpui::Context;
use std::time::Duration;

impl Playback {
    pub(super) fn report_current(&self, force: bool) {
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

    pub(super) fn report_position(&self, position: Duration, force: bool) {
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

    pub(super) fn stop_reporting(&self, failed: bool) {
        if let Some(reporter) = &self.reporter {
            // The queue may already point at the next track. Only update the
            // position here; the reporter retains the old track's identity.
            reporter.stop(self.current_position(), failed);
        }
    }

    pub(super) fn save_volume(&self, cx: &Context<Self>) {
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
}
