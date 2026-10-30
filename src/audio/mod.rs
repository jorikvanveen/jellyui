//! Bounded decoding workers and native-rate PipeWire output.
mod decoder;
mod device;
mod output;
#[cfg(test)]
mod tests;
mod volume;
pub use volume::Volume;

use crate::{domain::Error, infrastructure::cache::audio::AudioFile};
use decoder::{Decoded, decode_at, send};
use device::run;
use output::Track;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread,
    time::Duration,
};

pub enum Event {
    Ready {
        entry: usize,
        position: Duration,
    },
    Started {
        entry: usize,
        rate: u32,
        position: Duration,
    },
    Ended {
        entry: usize,
        position: Duration,
    },
    Finished,
    Failed {
        entry: usize,
        error: Error,
    },
}

#[derive(Default)]
struct Control {
    cancelled: Arc<AtomicBool>,
    paused: AtomicBool,
    state: Mutex<OutputState>,
}

#[derive(Default)]
struct OutputState {
    current: Option<Prepared>,
    next: Option<Prepared>,
    rate: u32,
    frames: u64,
    offset: Duration,
}

/// One decoder and its bounded PCM channel, prepared independently of output.
/// Clones retain the decoder; its last owner cancels blocked reads and sends.
#[derive(Clone)]
pub struct Prepared(Arc<Preparation>);

struct Preparation {
    entry: usize,
    source: AudioFile,
    position: Duration,
    cancelled: Arc<AtomicBool>,
    receiver: Mutex<Option<Receiver<Decoded>>>,
}

impl Prepared {
    pub fn new(entry: usize, source: AudioFile, position: Duration) -> Result<Self, Error> {
        let (sender, receiver) = mpsc::sync_channel(16);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = cancelled.clone();
        let worker_source = source.clone();
        thread::Builder::new()
            .name("audio-decode".into())
            .spawn(move || {
                let result = worker_source
                    .reader(worker_cancelled.clone())
                    .and_then(|reader| {
                        decode_at(Box::new(reader), &sender, &worker_cancelled, position)
                    });
                if let Err(error) = result {
                    send(&sender, &worker_cancelled, Decoded::Failed(error));
                }
            })
            .map_err(|e| failure("Could not start the audio decoder.", e))?;
        Ok(Self(Arc::new(Preparation {
            entry,
            source,
            position,
            cancelled,
            receiver: Mutex::new(Some(receiver)),
        })))
    }

    pub fn entry(&self) -> usize {
        self.0.entry
    }

    pub fn source(&self) -> &AudioFile {
        &self.0.source
    }
}

impl Drop for Preparation {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }
}

pub struct Handle {
    control: Arc<Control>,
    pub events: Receiver<Event>,
}

impl Handle {
    pub fn start(prepared: Prepared, paused: bool, volume: Volume) -> Result<Self, Error> {
        // A manual skip can race an automatic handoff. If the old output
        // already claimed this decoder, start a replacement from the disk cache.
        let current = match Track::new(prepared.clone()) {
            Some(track) => track,
            None => Track::new(Prepared::new(
                prepared.entry(),
                prepared.source().clone(),
                prepared.0.position,
            )?)
            .unwrap(),
        };
        current.prepared.source().promote();
        let control = Arc::new(Control {
            paused: AtomicBool::new(paused),
            state: Mutex::new(OutputState {
                current: Some(current.prepared.clone()),
                ..OutputState::default()
            }),
            ..Control::default()
        });
        let worker_control = control.clone();
        let (events, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("audio-output".into())
            .spawn(move || {
                let result = run(current, worker_control.clone(), &events, volume);
                // Also releases a decoder blocked on a full channel after an output failure.
                if !worker_control.cancelled.swap(true, Ordering::SeqCst) {
                    let _ = events.send(match result {
                        Ok(()) => Event::Finished,
                        Err(error) => {
                            tracing::error!(%error, "Playback stopped");
                            let entry = worker_control
                                .state
                                .lock()
                                .unwrap()
                                .current
                                .as_ref()
                                .unwrap()
                                .entry();
                            Event::Failed { entry, error }
                        }
                    });
                }
            })
            .map_err(|e| failure("Could not start the audio worker.", e))?;
        Ok(Self {
            control,
            events: receiver,
        })
    }

    pub fn pause(&self, paused: bool) {
        self.control.paused.store(paused, Ordering::SeqCst);
    }

    pub fn position(&self, entry: usize) -> Option<(Duration, u64)> {
        let state = self.control.state.lock().unwrap();
        if state.current.as_ref()?.entry() != entry || state.rate == 0 {
            return None;
        }
        Some((
            state.offset.saturating_add(Duration::from_secs_f64(
                state.frames as f64 / f64::from(state.rate),
            )),
            state.frames,
        ))
    }

    pub fn source(&self, entry: usize) -> Option<AudioFile> {
        let state = self.control.state.lock().unwrap();
        state
            .current
            .as_ref()
            .filter(|current| current.entry() == entry)
            .map(|current| current.source().clone())
    }

    pub fn set_next(&self, current: usize, next: Option<Prepared>) {
        let mut state = self.control.state.lock().unwrap();
        if state
            .current
            .as_ref()
            .is_some_and(|track| track.entry() == current)
        {
            state.next = next;
        }
    }

    /// Exclude an entry atomically with automatic handoff and return the actual
    /// current entry. Removing current output cancels it before it can advance.
    pub fn remove_entry(&self, entry: usize) -> Option<usize> {
        let mut state = self.control.state.lock().unwrap();
        let current = state.current.as_ref().map(Prepared::entry);
        if current == Some(entry) {
            self.control.cancelled.store(true, Ordering::SeqCst);
        }
        if state
            .next
            .as_ref()
            .is_some_and(|next| next.entry() == entry)
        {
            state.next = None;
        }
        current
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.control.cancelled.store(true, Ordering::SeqCst);
    }
}

fn failure(message: &'static str, error: impl std::fmt::Display) -> Error {
    tracing::error!(%error, message, "Audio worker failed");
    Error::Message(message)
}
