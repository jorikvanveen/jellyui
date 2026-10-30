use super::{Playback, PrefetchedAudio};
use crate::{audio, infrastructure::cache::audio::AudioFile};
use std::time::Duration;

impl Playback {
    pub(super) fn take_prefetched_audio(&mut self) -> Option<PrefetchedAudio> {
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

    pub(super) fn refresh_prefetch(&mut self) {
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

    pub(super) fn update_output_next(&self) {
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
}
