use super::{Control, Event, Prepared, decoder::Decoded};
use crate::domain::Error;
use std::{
    sync::{
        Arc,
        atomic::Ordering,
        mpsc::{self, Receiver, TryRecvError},
    },
    time::Duration,
};
use symphonia::core::audio::AudioSpec;
pub(super) struct Pcm {
    pub(super) receiver: Receiver<Decoded>,
    pub(super) samples: Vec<f64>,
    pub(super) offset: usize,
    pub(super) ended: bool,
    pub(super) error: Option<Error>,
}

impl Pcm {
    pub(super) fn new(receiver: Receiver<Decoded>) -> Self {
        Self {
            receiver,
            samples: Vec::new(),
            offset: 0,
            ended: false,
            error: None,
        }
    }

    /// Writes only complete frames. A temporary underrun adds silence at output,
    /// without advancing the track position or discarding decoded samples.
    pub(super) fn fill(
        &mut self,
        bytes: &mut [u8],
        channels: usize,
        gain: f64,
    ) -> Result<usize, Error> {
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        bytes.fill(0);
        let mut written = 0;
        while written < bytes.len() / 8 {
            if self.offset == self.samples.len() {
                match self.receiver.try_recv() {
                    Ok(Decoded::Samples(samples)) => {
                        self.samples = samples;
                        self.offset = 0;
                    }
                    Ok(Decoded::End) => {
                        self.ended = true;
                        break;
                    }
                    Ok(Decoded::Failed(error)) => {
                        if written == 0 {
                            return Err(error);
                        }
                        self.error = Some(error);
                        break;
                    }
                    Ok(Decoded::Format(_, _)) => {
                        return Err(Error::Message("Unexpected audio format change."));
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        return Err(Error::Message("The audio decoder stopped unexpectedly."));
                    }
                }
            }
            let count = (self.samples.len() - self.offset).min(bytes.len() / 8 - written);
            for (sample, destination) in self.samples[self.offset..self.offset + count].iter().zip(
                bytes[written * 8..(written + count) * 8]
                    .as_chunks_mut::<8>()
                    .0,
            ) {
                let sample = if gain == 0. {
                    0.
                } else if gain == 1. {
                    *sample
                } else {
                    sample * gain
                };
                destination.copy_from_slice(&sample.to_le_bytes());
            }
            self.offset += count;
            written += count;
        }
        Ok(written / channels)
    }
}

pub(super) struct Track {
    pub(super) prepared: Prepared,
    pub(super) pcm: Pcm,
    pub(super) spec: Option<AudioSpec>,
    pub(super) frames: u64,
    pub(super) started: bool,
    pub(super) offset: Duration,
}

impl Track {
    pub(super) fn new(prepared: Prepared) -> Option<Self> {
        let receiver = prepared.0.receiver.lock().unwrap().take()?;
        Some(Self {
            offset: prepared.0.position,
            prepared,
            pcm: Pcm::new(receiver),
            spec: None,
            frames: 0,
            started: false,
        })
    }

    pub(super) fn try_format(&mut self) -> Result<bool, Error> {
        if self.spec.is_some() {
            return Ok(true);
        }
        match self.pcm.receiver.try_recv() {
            Ok(Decoded::Format(spec, position)) => {
                self.spec = Some(spec);
                self.offset = position;
            }
            Ok(Decoded::Failed(error)) => return Err(error),
            Err(TryRecvError::Empty) => return Ok(false),
            _ => {
                return Err(Error::Message(
                    "The audio decoder returned no audio format.",
                ));
            }
        }
        Ok(true)
    }

    pub(super) fn position(&self) -> Duration {
        self.offset.saturating_add(Duration::from_secs_f64(
            self.frames as f64 / f64::from(self.spec.as_ref().unwrap().rate()),
        ))
    }
}

/// Joins prepared tracks within an output buffer. Source bit depth has already
/// been converted to f64; only sample rate and channel layout affect compatibility.
pub(super) struct Output {
    pub(super) track: Track,
    pub(super) spec: AudioSpec,
    pub(super) control: Arc<Control>,
    pub(super) events: mpsc::Sender<Event>,
    pub(super) finished: bool,
    pub(super) reconfigure: bool,
    pub(super) error: Option<Error>,
}

impl Output {
    pub(super) fn ready(&self) {
        let mut state = self.control.state.lock().unwrap();
        state.rate = self.spec.rate();
        state.offset = self.track.offset;
        let _ = self.events.send(Event::Ready {
            entry: self.track.prepared.entry(),
            position: self.track.offset,
        });
    }

    pub(super) fn fill(&mut self, bytes: &mut [u8], gain: f64) -> Result<usize, Error> {
        bytes.fill(0);
        let channels = self.spec.channels().count();
        let stride = channels * 8;
        let mut frames = 0;
        while frames < bytes.len() / stride {
            match self.track.try_format() {
                Ok(true) => {}
                Ok(false) => break,
                Err(error) => {
                    if frames == 0 {
                        return Err(error);
                    }
                    self.error = Some(error);
                    break;
                }
            }
            if self.track.spec.as_ref() != Some(&self.spec) {
                self.reconfigure = true;
                break;
            }
            let count = match self
                .track
                .pcm
                .fill(&mut bytes[frames * stride..], channels, gain)
            {
                Ok(count) => count,
                Err(error) => {
                    if frames == 0 {
                        return Err(error);
                    }
                    self.error = Some(error);
                    break;
                }
            };
            self.track.frames += count as u64;
            {
                let mut state = self.control.state.lock().unwrap();
                state.rate = self.spec.rate();
                state.frames = self.track.frames;
                state.offset = self.track.offset;
            }
            if count > 0 && !self.track.started {
                self.track.started = true;
                let _ = self.events.send(Event::Started {
                    entry: self.track.prepared.entry(),
                    rate: self.spec.rate(),
                    position: self.track.offset,
                });
            }
            frames += count;
            if let Some(error) = self.track.pcm.error.take() {
                self.error = Some(error);
                break;
            }
            if self.track.pcm.ended {
                let _ = self.events.send(Event::Ended {
                    entry: self.track.prepared.entry(),
                    position: self.track.position(),
                });
                let mut state = self.control.state.lock().unwrap();
                if self.control.cancelled.load(Ordering::SeqCst) {
                    break;
                }
                let next = state
                    .next
                    .take()
                    .filter(|next| !next.source().failed())
                    .and_then(Track::new);
                let Some(next) = next else {
                    state.rate = 0;
                    self.finished = true;
                    break;
                };
                next.prepared.source().promote();
                state.current = Some(next.prepared.clone());
                state.rate = 0;
                state.frames = 0;
                tracing::debug!(entry = next.prepared.entry(), "Advancing to prepared track");
                self.track = next;
            } else {
                break;
            }
        }
        Ok(frames)
    }
}
