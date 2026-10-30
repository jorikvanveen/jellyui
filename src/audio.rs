//! Blocking HTTP/decoding and the PipeWire loop run on separate worker threads.
//! A progressive disk cache supplies the original file; a bounded channel holds PCM.
use std::{
    cell::RefCell,
    io::Cursor,
    rc::Rc,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread,
    time::{Duration, Instant},
};

use pipewire::{
    self as pw,
    properties::properties,
    spa::{
        self,
        pod::{Object, Pod, Value, serialize::PodSerializer},
    },
};
use symphonia::core::{
    audio::{AudioSpec, Channels},
    formats::{SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    units::{Time, Timestamp},
};

use crate::{audio_file::AudioFile, jellyfin::Error};

// A replacement waits for our previous stream to be destroyed. This affects
// Jellyui's workers only; other applications retain shared access to PipeWire.
static OUTPUT: Mutex<()> = Mutex::new(());

/// Application volume, shared by the UI and each replacement audio worker.
#[derive(Clone)]
pub struct Volume(Arc<AtomicU32>);

impl Default for Volume {
    fn default() -> Self {
        Self(Arc::new(AtomicU32::new(1f32.to_bits())))
    }
}

impl Volume {
    pub fn level(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }

    pub fn set(&self, level: f32) {
        if level.is_finite() {
            self.0
                .store(level.clamp(0., 1.).to_bits(), Ordering::Relaxed);
        }
    }
}

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

enum Decoded {
    Format(AudioSpec, Duration),
    Samples(Vec<f64>),
    End,
    Failed(Error),
}

fn send(sender: &SyncSender<Decoded>, cancelled: &AtomicBool, mut message: Decoded) -> bool {
    while !cancelled.load(Ordering::SeqCst) {
        match sender.try_send(message) {
            Ok(()) => return true,
            Err(TrySendError::Full(value)) => message = value,
            Err(TrySendError::Disconnected(_)) => return false,
        }
        thread::sleep(Duration::from_millis(10));
    }
    false
}

fn decode_at(
    source: Box<dyn MediaSource>,
    sender: &SyncSender<Decoded>,
    cancelled: &AtomicBool,
    position: Duration,
) -> Result<(), Error> {
    let source = MediaSourceStream::new(source, Default::default());
    let mut format = symphonia::default::get_probe()
        .probe(&Hint::new(), source, Default::default(), Default::default())
        .map_err(|e| {
            failure(
                "Could not read this audio file. Only FLAC and MP3 are supported.",
                e,
            )
        })?;
    let track = format
        .default_track(TrackType::Audio)
        .ok_or(Error::Message("This file contains no audio track."))?;
    let params = track
        .codec_params
        .as_ref()
        .and_then(|p| p.audio())
        .ok_or(Error::Message("The audio format is missing."))?;
    let mut decoder = symphonia::default::get_codecs()
        .make_audio_decoder(
            params,
            &symphonia::core::codecs::audio::AudioDecoderOptions::default()
                .verify(position.is_zero()),
        )
        .map_err(|e| {
            failure(
                "Could not decode this audio file. Only FLAC and MP3 are supported.",
                e,
            )
        })?;
    let needs_preroll = params.codec == symphonia::core::codecs::audio::well_known::CODEC_ID_MP3;
    let id = track.id;
    let time_base = track.time_base;
    let mut seek_ts = Timestamp::ZERO;
    if !position.is_zero() {
        let time_base = time_base.ok_or(Error::Message(
            "This track does not provide timing information for seeking.",
        ))?;
        let time = Time::try_from_secs_f64(position.as_secs_f64())
            .ok_or(Error::Message("Invalid seek position."))?;
        let mut target = time_base
            .calc_timestamp(time)
            .ok_or(Error::Message("Invalid seek position."))?;
        if let Some(last) = track
            .duration
            .filter(|duration| duration.get() > 0)
            .and_then(|duration| {
                track
                    .start_ts
                    .checked_add(duration.saturating_sub(1u64.into()))
            })
        {
            target = target.min(last);
        }
        // MP3's bit-reservoir seek position alone does not restore synthesis
        // filter/overlap state. Decode a short lead-in and trim to the real target.
        let seek_target = if needs_preroll {
            let seconds = (time_base.calc_time_saturating(target).as_secs_f64() - 0.5).max(0.);
            time_base
                .calc_timestamp(Time::try_from_secs_f64(seconds).expect("finite seek time"))
                .unwrap_or(Timestamp::ZERO)
        } else {
            target
        };
        format
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts: seek_target,
                    track_id: id,
                },
            )
            .map_err(|error| {
                failure(
                    "Could not seek this track. Check the server connection and retry.",
                    error,
                )
            })?;
        seek_ts = target;
        decoder.reset();
    }
    let seek_seconds = time_base
        .map(|base| base.calc_time_saturating(seek_ts).as_secs_f64())
        .unwrap_or(0.)
        .max(0.);
    let mut spec = None;
    while !cancelled.load(Ordering::SeqCst) {
        let Some(packet) = format.next_packet().map_err(|e| {
            failure(
                "The audio stream was interrupted or is damaged. Retry this track.",
                e,
            )
        })?
        else {
            break;
        };
        if packet.track_id != id {
            continue;
        }
        let buffer = decoder.decode(&packet).map_err(|e| {
            failure(
                "Could not decode this track. The audio file may be damaged.",
                e,
            )
        })?;
        if buffer.is_empty() {
            continue;
        }
        let skip = if position.is_zero() {
            0
        } else {
            let packet_seconds = time_base
                .expect("seek requires a timebase")
                .calc_time_saturating(packet.pts)
                .as_secs_f64();
            let rate = f64::from(buffer.spec().rate());
            ((seek_seconds - packet_seconds) * rate - packet.trim_start.get() as f64)
                .round()
                .max(0.) as usize
        };
        if skip >= buffer.frames() {
            continue;
        }
        if let Some(spec) = &spec {
            if spec != buffer.spec() {
                return Err(Error::Message(
                    "The audio format changed within this track.",
                ));
            }
        } else {
            positions(buffer.spec())?;
            spec = Some(buffer.spec().clone());
            if !send(
                sender,
                cancelled,
                Decoded::Format(buffer.spec().clone(), Duration::from_secs_f64(seek_seconds)),
            ) {
                return Ok(());
            }
        }
        // f64 preserves all integer samples up to 32 bits. No gain, normalization,
        // fades, dithering or resampling is applied during decoding. User volume
        // is applied when packing the output buffer.
        let slice = buffer.slice(skip..);
        let mut samples: Vec<f64> = vec![0.; slice.samples_interleaved()];
        slice.copy_to_slice_interleaved(&mut samples);
        for block in samples.chunks(1024 * buffer.spec().channels().count()) {
            if !send(sender, cancelled, Decoded::Samples(block.to_vec())) {
                return Ok(());
            }
        }
    }
    if spec.is_none() {
        return Err(Error::Message("The audio file contains no samples."));
    }
    if decoder.finalize().verify_ok == Some(false) {
        return Err(Error::Message(
            "The audio file failed its integrity check. Retry or choose another track.",
        ));
    }
    send(sender, cancelled, Decoded::End);
    Ok(())
}

/// Symphonia's positioned channels use WAVE order. Keep that order in PipeWire.
fn positions(spec: &AudioSpec) -> Result<[u32; spa::param::audio::MAX_CHANNELS], Error> {
    use spa::sys::*;
    const WAVE: [u32; 18] = [
        SPA_AUDIO_CHANNEL_FL,
        SPA_AUDIO_CHANNEL_FR,
        SPA_AUDIO_CHANNEL_FC,
        SPA_AUDIO_CHANNEL_LFE,
        SPA_AUDIO_CHANNEL_RL,
        SPA_AUDIO_CHANNEL_RR,
        SPA_AUDIO_CHANNEL_FLC,
        SPA_AUDIO_CHANNEL_FRC,
        SPA_AUDIO_CHANNEL_RC,
        SPA_AUDIO_CHANNEL_SL,
        SPA_AUDIO_CHANNEL_SR,
        SPA_AUDIO_CHANNEL_TC,
        SPA_AUDIO_CHANNEL_TFL,
        SPA_AUDIO_CHANNEL_TFC,
        SPA_AUDIO_CHANNEL_TFR,
        SPA_AUDIO_CHANNEL_TRL,
        SPA_AUDIO_CHANNEL_TRC,
        SPA_AUDIO_CHANNEL_TRR,
    ];
    let Channels::Positioned(mask) = spec.channels() else {
        return Err(Error::Message(
            "This audio channel layout is not supported.",
        ));
    };
    if spec.rate() == 0 || mask.is_empty() || mask.bits() >> WAVE.len() != 0 {
        return Err(Error::Message(
            "This audio rate or channel layout is not supported.",
        ));
    }
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    for (index, bit) in mask.iter().enumerate() {
        positions[index] = WAVE[bit.bits().trailing_zeros() as usize];
    }
    if spec.channels().count() == 1 {
        positions[0] = SPA_AUDIO_CHANNEL_MONO;
    }
    Ok(positions)
}

struct Pcm {
    receiver: Receiver<Decoded>,
    samples: Vec<f64>,
    offset: usize,
    ended: bool,
    error: Option<Error>,
}

impl Pcm {
    fn new(receiver: Receiver<Decoded>) -> Self {
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
    fn fill(&mut self, bytes: &mut [u8], channels: usize, gain: f64) -> Result<usize, Error> {
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

struct Track {
    prepared: Prepared,
    pcm: Pcm,
    spec: Option<AudioSpec>,
    frames: u64,
    started: bool,
    offset: Duration,
}

impl Track {
    fn new(prepared: Prepared) -> Option<Self> {
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

    fn try_format(&mut self) -> Result<bool, Error> {
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

    fn position(&self) -> Duration {
        self.offset.saturating_add(Duration::from_secs_f64(
            self.frames as f64 / f64::from(self.spec.as_ref().unwrap().rate()),
        ))
    }
}

/// Joins prepared tracks within an output buffer. Source bit depth has already
/// been converted to f64; only sample rate and channel layout affect compatibility.
struct Output {
    track: Track,
    spec: AudioSpec,
    control: Arc<Control>,
    events: mpsc::Sender<Event>,
    finished: bool,
    reconfigure: bool,
    error: Option<Error>,
}

impl Output {
    fn ready(&self) {
        let mut state = self.control.state.lock().unwrap();
        state.rate = self.spec.rate();
        state.offset = self.track.offset;
        let _ = self.events.send(Event::Ready {
            entry: self.track.prepared.entry(),
            position: self.track.offset,
        });
    }

    fn fill(&mut self, bytes: &mut [u8], gain: f64) -> Result<usize, Error> {
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

#[derive(Default)]
struct Report {
    error: Option<Error>,
    drained: bool,
    draining: bool,
    last_samples: Option<Instant>,
}

fn run(
    mut current: Track,
    control: Arc<Control>,
    events: &mpsc::Sender<Event>,
    volume: Volume,
) -> Result<(), Error> {
    while !current.try_format()? {
        if control.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(5));
    }
    let _output = loop {
        if control.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        match OUTPUT.try_lock() {
            Ok(guard) => break guard,
            Err(TryLockError::WouldBlock) => thread::sleep(Duration::from_millis(10)),
            Err(TryLockError::Poisoned(_)) => {
                return Err(Error::Message(
                    "The audio worker failed unexpectedly. Restart Jellyui.",
                ));
            }
        }
    };
    let output = Rc::new(RefCell::new(Output {
        spec: current.spec.as_ref().unwrap().clone(),
        track: current,
        control: control.clone(),
        events: events.clone(),
        finished: false,
        reconfigure: false,
        error: None,
    }));
    loop {
        run_stream(output.clone(), control.clone(), volume.clone())?;
        let mut output = output.borrow_mut();
        if let Some(error) = output.error.take() {
            return Err(error);
        }
        if output.finished || control.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        // Drain the old format before opening the new one. The next decoder
        // and its PCM stay intact; same-format boundaries never reach here.
        output.spec = output.track.spec.as_ref().unwrap().clone();
        output.reconfigure = false;
    }
}

fn run_stream(
    output: Rc<RefCell<Output>>,
    control: Arc<Control>,
    volume: Volume,
) -> Result<(), Error> {
    let spec = output.borrow().spec.clone();
    tracing::info!(
        rate = spec.rate(),
        channels = spec.channels().count(),
        "Opening PipeWire playback"
    );
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| failure("Could not create the PipeWire loop.", e))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| failure("Could not create the PipeWire context.", e))?;
    let core = context.connect_rc(None).map_err(|e| {
        failure(
            "Could not connect to PipeWire. Check that it is running.",
            e,
        )
    })?;
    let report = Rc::new(RefCell::new(Report::default()));
    let core_report = report.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, code, message| {
            core_report.borrow_mut().error = Some(failure(
                "PipeWire output failed. Check your audio output, then retry.",
                format_args!("{code}: {message}"),
            ));
        })
        .register();
    let stream = pw::stream::StreamBox::new(
        &core,
        "Jellyui",
        properties! {
            "application.name" => "Jellyui",
            "node.name" => "jellyui-playback",
            "media.type" => "Audio",
            "media.category" => "Playback",
            "media.role" => "Music",
            "node.rate" => format!("1/{}", spec.rate()),
        },
    )
    .map_err(|e| failure("Could not create the PipeWire stream.", e))?;
    let channels = spec.channels().count();
    let stride = channels * 8;
    let rate = spec.rate();
    let process_control = control.clone();
    let process_output = output.clone();
    let _listener = stream
        .add_local_listener_with_user_data(report.clone())
        .state_changed(|_, report, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                report.borrow_mut().error = Some(failure(
                    "PipeWire could not play this track. Check your audio output, then retry.",
                    error,
                ));
            }
        })
        .param_changed(move |_, report, id, pod| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(pod) = pod else {
                return;
            };
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(pod).is_err()
                || info.rate() != rate
                || info.channels() != channels as u32
                || info.format() != spa::param::audio::AudioFormat::F64LE
            {
                report.borrow_mut().error = Some(Error::Message(
                    "PipeWire negotiated an unexpected audio format.",
                ));
            } else {
                tracing::debug!(rate, channels, "PipeWire source format negotiated");
            }
        })
        .process(move |stream, report| {
            if report.borrow().draining
                || process_control.cancelled.load(Ordering::SeqCst)
                || process_control.paused.load(Ordering::SeqCst)
            {
                return;
            }
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let requested = buffer.requested() as usize;
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            *data.chunk_mut().size_mut() = 0;
            let Some(bytes) = data.data() else {
                report.borrow_mut().error = Some(Error::Message(
                    "PipeWire returned an unmapped audio buffer.",
                ));
                return;
            };
            let capacity = bytes.len() / stride;
            let count = if requested == 0 {
                capacity
            } else {
                capacity.min(requested)
            };
            let frames = match process_output
                .borrow_mut()
                .fill(&mut bytes[..count * stride], f64::from(volume.level()))
            {
                Ok(frames) => frames,
                Err(error) => {
                    report.borrow_mut().error = Some(error);
                    return;
                }
            };
            if frames > 0 {
                report.borrow_mut().last_samples = Some(Instant::now());
            }
            let draining = process_output.borrow().finished
                || process_output.borrow().reconfigure
                || process_output.borrow().error.is_some();
            let chunk = data.chunk_mut();
            *chunk.offset_mut() = 0;
            *chunk.stride_mut() = stride as i32;
            *chunk.size_mut() = (if draining { frames } else { count } * stride) as u32;
            drop(buffer);
            if draining {
                report.borrow_mut().draining = true;
                if let Err(error) = stream.flush(true) {
                    report.borrow_mut().error =
                        Some(failure("Could not finish the PipeWire stream.", error));
                }
            }
        })
        .drained(|_, report| report.borrow_mut().drained = true)
        .register()
        .map_err(|e| failure("Could not listen to the PipeWire stream.", e))?;
    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(spa::param::audio::AudioFormat::F64LE);
    info.set_rate(spec.rate());
    info.set_channels(channels as u32);
    info.set_position(positions(&spec)?);
    let pod = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )
    .map_err(|e| {
        failure(
            "Could not describe the audio format to PipeWire.",
            format_args!("{e:?}"),
        )
    })?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&pod).ok_or(Error::Message("Invalid PipeWire audio format."))?;
    // Callbacks run on this worker's loop, never the UI or PipeWire's realtime thread.
    stream
        .connect(
            spa::utils::Direction::Output,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut [pod],
        )
        .map_err(|e| failure("Could not connect the PipeWire stream.", e))?;
    output.borrow().ready();
    let mut paused = false;
    let started = Instant::now();
    while !control.cancelled.load(Ordering::SeqCst) {
        let should_pause = control.paused.load(Ordering::SeqCst);
        if paused != should_pause {
            stream
                .set_active(!should_pause)
                .map_err(|e| failure("Could not pause or resume PipeWire playback.", e))?;
            paused = should_pause;
        }
        {
            let mut report = report.borrow_mut();
            if let Some(error) = report.error.take() {
                return Err(error);
            }
            if report.drained {
                return Ok(());
            }
            if paused {
                report.last_samples = Some(Instant::now());
            }
            if report.last_samples.unwrap_or(started).elapsed() > Duration::from_secs(15) {
                return Err(Error::Message(
                    "Audio playback stalled. Check the server connection and audio output, then retry.",
                ));
            }
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(25)));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{io::Write, net::Shutdown};
    use symphonia::core::io::ReadOnlySource;

    #[test]
    fn queue_removal_cancels_only_the_selected_entry_and_prevents_a_stale_handoff() {
        let messages = || decoded(include_bytes!("../tests/fixtures/continuous16.flac"));
        let first = prepared_messages(1, messages());
        let next = prepared_messages(2, messages());
        let cancelled = next.0.cancelled.clone();
        let (mut stream, handle) = output(first, Some(next));
        assert_eq!(handle.remove_entry(2), Some(1));
        assert!(cancelled.load(Ordering::SeqCst));
        assert!(!handle.control.cancelled.load(Ordering::SeqCst));
        let mut bytes = vec![0; 200 * 16];
        assert_eq!(stream.fill(&mut bytes, 1.).unwrap(), 173);
        assert!(stream.finished);
        assert_eq!(
            handle
                .events
                .try_iter()
                .filter(|event| matches!(event, Event::Started { .. }))
                .count(),
            1
        );

        let first = prepared_messages(1, messages());
        let next = prepared_messages(2, messages());
        let next_receiver = next.clone();
        let (mut stream, handle) = output(first, Some(next));
        assert_eq!(handle.remove_entry(1), Some(1));
        assert!(handle.control.cancelled.load(Ordering::SeqCst));
        stream.fill(&mut bytes, 1.).unwrap();
        assert!(next_receiver.0.receiver.lock().unwrap().is_some());
    }

    fn prepared_messages(entry: usize, messages: Vec<Decoded>) -> Prepared {
        let (source, _connection) = crate::audio_file::tests::stream(Some(0));
        let (sender, receiver) = mpsc::sync_channel(messages.len().max(1));
        for message in messages {
            sender.send(message).unwrap();
        }
        Prepared(Arc::new(Preparation {
            entry,
            source,
            position: Duration::ZERO,
            cancelled: Arc::default(),
            receiver: Mutex::new(Some(receiver)),
        }))
    }

    fn output(first: Prepared, next: Option<Prepared>) -> (Output, Handle) {
        let mut track = Track::new(first.clone()).unwrap();
        assert!(track.try_format().unwrap());
        let control = Arc::new(Control {
            state: Mutex::new(OutputState {
                current: Some(first),
                next,
                ..OutputState::default()
            }),
            ..Control::default()
        });
        let (events, receiver) = mpsc::channel();
        (
            Output {
                spec: track.spec.as_ref().unwrap().clone(),
                track,
                control: control.clone(),
                events,
                finished: false,
                reconfigure: false,
                error: None,
            },
            Handle {
                control,
                events: receiver,
            },
        )
    }

    fn unpack(bytes: &[u8]) -> Vec<f64> {
        bytes
            .as_chunks::<8>()
            .0
            .iter()
            .map(|sample| f64::from_le_bytes(*sample))
            .collect()
    }

    #[test]
    fn continuous_waveform_joins_across_16_and_24_bit_files_without_missing_or_added_samples() {
        for capacity in [1, 7, 173, 174, 441, 1024] {
            let first = prepared_messages(
                0,
                decoded(include_bytes!("../tests/fixtures/continuous16.flac")),
            );
            let next = prepared_messages(
                1,
                decoded(include_bytes!("../tests/fixtures/continuous24.flac")),
            );
            let (mut output, handle) = output(first, Some(next));
            let mut all = Vec::new();
            while !output.finished {
                let mut bytes = vec![255; capacity * 16];
                let frames = output.fill(&mut bytes, 1.).unwrap();
                assert!(!output.reconfigure);
                if !output.finished {
                    assert_eq!(frames, capacity);
                }
                all.extend(unpack(&bytes[..frames * 16]));
            }
            assert_eq!(all.len(), 441 * 2);
            for (frame, samples) in all.as_chunks::<2>().0.iter().enumerate() {
                let sample = ((frame % 17) as f64 - 8.) / 32768.;
                assert_eq!(
                    *samples,
                    [sample, -sample],
                    "buffer {capacity}, frame {frame}"
                );
            }
            let events: Vec<_> = handle.events.try_iter().collect();
            assert!(matches!(
                &events[..],
                [
                    Event::Started { entry: 0, .. },
                    Event::Ended { entry: 0, .. },
                    Event::Started { entry: 1, .. },
                    Event::Ended { entry: 1, .. },
                ]
            ));
            assert!(
                handle.position(1).is_none(),
                "finished audio must not create a new playback report"
            );
        }
    }

    #[test]
    fn rate_and_channel_changes_drain_the_old_format_and_keep_the_prepared_samples() {
        let reference = decoded(include_bytes!("../tests/fixtures/continuous16.flac"));
        let Decoded::Format(spec, _) = &reference[0] else {
            panic!("missing format");
        };
        for next_spec in [
            AudioSpec::new(48000, spec.channels().clone()),
            AudioSpec::new(
                44100,
                Channels::Positioned(symphonia::core::audio::Position::FRONT_LEFT),
            ),
        ] {
            let channels = next_spec.channels().count();
            let next_samples = vec![0.25; channels * 5];
            let first = prepared_messages(
                10,
                decoded(include_bytes!("../tests/fixtures/continuous16.flac")),
            );
            let next = prepared_messages(
                20,
                vec![
                    Decoded::Format(next_spec.clone(), Duration::ZERO),
                    Decoded::Samples(next_samples.clone()),
                    Decoded::End,
                ],
            );
            let (mut output, handle) = output(first, Some(next));
            let mut bytes = vec![0; 1024 * 16];
            assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 173);
            assert_eq!(
                unpack(&bytes[..173 * 16]),
                samples(decoded(include_bytes!(
                    "../tests/fixtures/continuous16.flac"
                )))
            );
            assert!(output.reconfigure);
            assert!(!output.finished);
            assert_eq!(output.track.prepared.entry(), 20);
            assert_eq!(
                handle
                    .events
                    .try_iter()
                    .filter(|event| matches!(event, Event::Started { .. }))
                    .count(),
                1
            );
            // This is the same state retained while PipeWire drains/reopens.
            output.spec = next_spec;
            output.reconfigure = false;
            let mut bytes = vec![0; 10 * channels * 8];
            assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 5);
            assert_eq!(unpack(&bytes[..5 * channels * 8]), next_samples);
            assert!(output.finished);
            assert!(matches!(
                handle.events.try_recv().unwrap(),
                Event::Started { entry: 20, .. }
            ));
        }
    }

    #[test]
    fn reordering_replaces_preparation_and_stale_ui_updates_cannot_requeue_the_current_track() {
        let messages = || decoded(include_bytes!("../tests/fixtures/continuous16.flac"));
        let first = prepared_messages(1, messages());
        let old_next = prepared_messages(2, messages());
        let cancelled = old_next.0.cancelled.clone();
        let replacement = prepared_messages(3, messages());
        let (mut output, handle) = output(first, Some(old_next));
        assert!(
            handle.events.try_recv().is_err(),
            "preparation must not report a play"
        );
        handle.set_next(1, Some(replacement));
        assert!(cancelled.load(Ordering::SeqCst));
        let mut bytes = vec![0; 174 * 16];
        assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 174);
        assert!(handle.position(1).is_none());
        assert_eq!(handle.position(3).unwrap().1, 1);
        // The UI can still display the preceding track until its next poll.
        handle.set_next(1, Some(prepared_messages(3, messages())));
        assert!(handle.control.state.lock().unwrap().next.is_none());
        assert!(matches!(
            handle.events.try_iter().last(),
            Some(Event::Started { entry: 3, .. })
        ));
    }

    #[test]
    fn an_unready_next_decoder_waits_without_finishing_or_reporting_phantom_audio() {
        let first = prepared_messages(
            1,
            decoded(include_bytes!("../tests/fixtures/continuous16.flac")),
        );
        let next = prepared_messages(2, Vec::new());
        let (sender, receiver) = mpsc::sync_channel(4);
        *next.0.receiver.lock().unwrap() = Some(receiver);
        let (mut output, handle) = output(first, Some(next));
        let mut bytes = vec![255; 200 * 16];
        assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 173);
        assert!(!output.finished);
        assert!(!output.reconfigure);
        assert!(handle.position(2).is_none());
        assert_eq!(handle.events.try_iter().count(), 2); // First track's start/end.
        sender
            .send(Decoded::Format(output.spec.clone(), Duration::ZERO))
            .unwrap();
        sender.send(Decoded::Samples(vec![0.25; 4])).unwrap();
        sender.send(Decoded::End).unwrap();
        assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 2);
        assert_eq!(unpack(&bytes[..32]), vec![0.25; 4]);
        assert!(output.finished);
        assert!(matches!(
            handle.events.try_recv().unwrap(),
            Event::Started { entry: 2, .. }
        ));
    }

    #[test]
    fn a_failed_next_track_keeps_the_previous_tracks_final_frames() {
        for format_known in [false, true] {
            let messages = decoded(include_bytes!("../tests/fixtures/continuous16.flac"));
            let Decoded::Format(spec, _) = &messages[0] else {
                panic!("missing format");
            };
            let mut next_messages = Vec::new();
            if format_known {
                next_messages.push(Decoded::Format(spec.clone(), Duration::ZERO));
            }
            next_messages.push(Decoded::Failed(Error::Unauthorized));
            let first = prepared_messages(1, messages);
            let next = prepared_messages(2, next_messages);
            let (mut output, handle) = output(first, Some(next));
            let mut bytes = vec![0; 200 * 16];
            assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 173);
            assert_eq!(
                unpack(&bytes[..173 * 16]),
                samples(decoded(include_bytes!(
                    "../tests/fixtures/continuous16.flac"
                )))
            );
            assert!(matches!(output.error, Some(Error::Unauthorized)));
            assert_eq!(
                handle
                    .control
                    .state
                    .lock()
                    .unwrap()
                    .current
                    .as_ref()
                    .unwrap()
                    .entry(),
                2
            );
            assert_eq!(
                handle
                    .events
                    .try_iter()
                    .filter(|event| matches!(event, Event::Started { .. }))
                    .count(),
                1
            );
        }
    }

    #[test]
    fn dropping_a_preparation_cancels_decoding_while_waiting_for_bytes_or_a_full_pcm_buffer() {
        let bytes = include_bytes!("../tests/fixtures/stereo-gapless.mp3");
        for provide_audio in [false, true] {
            let (source, mut connection) =
                crate::audio_file::tests::stream(Some(bytes.len() as u64));
            let prepared = Prepared::new(1, source, Duration::ZERO).unwrap();
            if provide_audio {
                connection.write_all(bytes).unwrap();
                assert!(matches!(
                    prepared
                        .0
                        .receiver
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(2))
                        .unwrap(),
                    Decoded::Format(_, _)
                ));
            }
            let cancelled = prepared.0.cancelled.clone();
            drop(prepared);
            let deadline = Instant::now() + Duration::from_secs(3);
            while Arc::strong_count(&cancelled) > 1 {
                assert!(Instant::now() < deadline, "discarded decoder did not exit");
                thread::sleep(Duration::from_millis(5));
            }
            assert!(cancelled.load(Ordering::SeqCst));
        }
    }

    #[test]
    fn preparation_decodes_the_cached_prefix_without_resuming_the_download() {
        let (source, mut connection, _directory, listener) =
            crate::audio_file::tests::connect_mode(true);
        let prefix = crate::audio_file::PREFETCH_BYTES as usize;
        let audio = include_bytes!("../tests/fixtures/stereo-gapless.mp3");
        let mut bytes = vec![0; prefix];
        bytes[..audio.len()].copy_from_slice(audio);
        write!(connection, "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes 0-{}/{}\r\nContent-Length: {prefix}\r\nConnection: close\r\n\r\n", prefix - 1, prefix + 100).unwrap();
        connection.write_all(&bytes).unwrap();
        let prepared = Prepared::new(1, source, Duration::ZERO).unwrap();
        let receiver = prepared.0.receiver.lock().unwrap();
        let receiver = receiver.as_ref().unwrap();
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Decoded::Format(_, _)
        ));
        assert!(matches!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            Decoded::Samples(_)
        ));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn paused_output_exposes_the_seek_position_before_any_samples_play() {
        let mut messages = decoded(include_bytes!("../tests/fixtures/continuous16.flac"));
        let seek = Duration::from_secs(42);
        let Decoded::Format(_, offset) = &mut messages[0] else {
            panic!("missing format");
        };
        *offset = seek;
        let (output, handle) = output(prepared_messages(1, messages), None);
        handle.pause(true);
        output.ready();
        assert_eq!(handle.position(1), Some((seek, 0)));
        assert!(
            matches!(handle.events.try_recv().unwrap(), Event::Ready {entry: 1, position} if position == seek)
        );
        assert!(
            handle.events.try_recv().is_err(),
            "ready output must not report a play"
        );
    }

    fn decode(
        source: Box<dyn MediaSource>,
        sender: &SyncSender<Decoded>,
        control: &Control,
    ) -> Result<(), Error> {
        decode_at(source, sender, &control.cancelled, Duration::ZERO)
    }

    fn decoded(bytes: &'static [u8]) -> Vec<Decoded> {
        let (sender, receiver) = mpsc::sync_channel(256);
        decode(
            Box::new(ReadOnlySource::new(Cursor::new(bytes))),
            &sender,
            &Control::default(),
        )
        .unwrap();
        receiver.try_iter().collect()
    }

    fn sought(bytes: &'static [u8], position: Duration) -> Vec<Decoded> {
        let (sender, receiver) = mpsc::sync_channel(256);
        let control = Arc::new(Control::default());
        decode_at(
            Box::new(Cursor::new(bytes)),
            &sender,
            &control.cancelled,
            position,
        )
        .unwrap();
        receiver.try_iter().collect()
    }

    fn samples(messages: Vec<Decoded>) -> Vec<f64> {
        messages
            .into_iter()
            .flat_map(|message| match message {
                Decoded::Samples(samples) => samples,
                _ => Vec::new(),
            })
            .collect()
    }

    #[test]
    fn flac_seeking_trims_to_the_requested_sample_and_plays_the_tail() {
        let bytes = include_bytes!("../tests/fixtures/stereo24.flac");
        let reference = samples(decoded(bytes));
        let seek = Duration::from_nanos(100u64 * 1_000_000_000 / 44_100 + 1);
        let messages = sought(bytes, seek);
        assert!(matches!(messages.last(), Some(Decoded::End)));
        assert_eq!(samples(messages), reference[200..]);
        // A duration rounded up in server metadata must not make an end seek fail.
        assert_eq!(
            samples(sought(bytes, Duration::from_secs(10))),
            reference[reference.len() - 2..]
        );
    }

    #[test]
    fn mp3_seeking_matches_full_decode_after_decoder_preroll() {
        for (bytes, milliseconds) in [
            (
                include_bytes!("../tests/fixtures/stereo.mp3").as_slice(),
                50u64,
            ),
            (
                include_bytes!("../tests/fixtures/stereo-gapless.mp3").as_slice(),
                700u64,
            ),
        ] {
            let reference = samples(decoded(bytes));
            let messages = sought(bytes, Duration::from_millis(milliseconds));
            assert!(matches!(messages.last(), Some(Decoded::End)));
            let result = samples(messages);
            let expected = &reference[(milliseconds * 48 * 2) as usize..];
            assert_eq!(result.len(), expected.len(), "{milliseconds} ms seek");
            assert!(
                result
                    .iter()
                    .zip(expected)
                    .all(|(actual, expected)| (actual - expected).abs() < 0.00001),
                "{milliseconds} ms seek"
            );
        }
    }

    #[test]
    fn flac_preserves_24_bit_samples_channel_order_and_final_frames() {
        let messages = decoded(include_bytes!("../tests/fixtures/stereo24.flac"));
        let Decoded::Format(spec, _) = &messages[0] else {
            panic!("expected format")
        };
        assert_eq!(spec.rate(), 44100);
        assert_eq!(spec.channels().count(), 2);
        let (sender, receiver) = mpsc::sync_channel(64);
        for message in messages.into_iter().skip(1) {
            sender.send(message).unwrap();
        }
        let mut pcm = Pcm::new(receiver);
        let mut result = Vec::new();
        while !pcm.ended {
            // Deliberately split decoder blocks across small output buffers.
            let mut bytes = [0; 7 * 2 * 8];
            let frames = pcm.fill(&mut bytes, 2, 1.).unwrap();
            result.extend(
                bytes[..frames * 2 * 8]
                    .as_chunks::<8>()
                    .0
                    .iter()
                    .map(|s| f64::from_le_bytes(*s)),
            );
        }
        assert_eq!(result.len(), 441 * 2);
        for (frame, samples) in result.as_chunks::<2>().0.iter().enumerate() {
            let expected = ((frame % 17) as f64 - 8.) / 8_388_608.;
            assert_eq!(*samples, [expected, -expected], "frame {frame}");
        }
    }

    #[test]
    fn mp3_decodes_at_its_source_rate_with_both_channels() {
        let messages = decoded(include_bytes!("../tests/fixtures/stereo.mp3"));
        let Decoded::Format(spec, _) = &messages[0] else {
            panic!("expected format")
        };
        assert_eq!(spec.rate(), 48000);
        assert_eq!(spec.channels().count(), 2);
        let samples: Vec<_> = messages
            .iter()
            .filter_map(|m| match m {
                Decoded::Samples(s) => Some(s.as_slice()),
                _ => None,
            })
            .flatten()
            .copied()
            .collect();
        assert!(samples.len() >= 4800 * 2);
        assert!(samples.iter().all(|s| s.is_finite() && s.abs() < 1.));
        assert!(
            samples
                .as_chunks::<2>()
                .0
                .iter()
                .any(|frame| (frame[0] - frame[1]).abs() > 0.01)
        );
        assert!(matches!(messages.last(), Some(Decoded::End)));
    }

    #[test]
    fn underrun_preserves_position_and_a_decoder_failure_is_not_end_of_track() {
        let (sender, receiver) = mpsc::sync_channel(4);
        let mut pcm = Pcm::new(receiver);
        let mut bytes = [255; 2 * 8];
        assert_eq!(pcm.fill(&mut bytes, 2, 1.).unwrap(), 0);
        assert_eq!(bytes, [0; 16]);
        assert!(!pcm.ended);
        sender.send(Decoded::Samples(vec![0.25, -0.5])).unwrap();
        assert_eq!(pcm.fill(&mut bytes, 2, 1.).unwrap(), 1);
        assert_eq!(f64::from_le_bytes(bytes[..8].try_into().unwrap()), 0.25);
        sender.send(Decoded::Failed(Error::Unauthorized)).unwrap();
        assert!(matches!(
            pcm.fill(&mut bytes, 2, 1.),
            Err(Error::Unauthorized)
        ));
        assert!(!pcm.ended);
    }

    #[test]
    fn volume_changes_apply_to_buffered_samples_and_muting_keeps_consuming_frames() {
        let volume = Volume::default();
        let shared = volume.clone();
        let sample = f64::from_bits(0x3fd0_0000_0000_0001);
        let (sender, receiver) = mpsc::sync_channel(2);
        sender
            .send(Decoded::Samples(vec![
                sample, -0.5, 0.5, -0.75, 0.25, -0.25,
            ]))
            .unwrap();
        sender.send(Decoded::End).unwrap();
        let mut pcm = Pcm::new(receiver);
        let mut bytes = [0; 16];
        assert_eq!(
            pcm.fill(&mut bytes, 2, f64::from(shared.level())).unwrap(),
            1
        );
        assert_eq!(&bytes[..8], &sample.to_le_bytes());
        assert_eq!(&bytes[8..], &(-0.5f64).to_le_bytes());
        volume.set(0.5);
        assert_eq!(
            pcm.fill(&mut bytes, 2, f64::from(shared.level())).unwrap(),
            1
        );
        assert_eq!(f64::from_le_bytes(bytes[..8].try_into().unwrap()), 0.25);
        assert_eq!(f64::from_le_bytes(bytes[8..].try_into().unwrap()), -0.375);
        volume.set(-1.);
        assert_eq!(
            pcm.fill(&mut bytes, 2, f64::from(shared.level())).unwrap(),
            1
        );
        assert_eq!(bytes, [0; 16]);
        assert_eq!(
            pcm.fill(&mut bytes, 2, f64::from(shared.level())).unwrap(),
            0
        );
        assert!(pcm.ended);
        volume.set(2.);
        assert_eq!(shared.level(), 1.);
        volume.set(f32::NAN);
        assert_eq!(shared.level(), 1.);
    }

    #[test]
    fn cancelling_releases_a_full_decode_buffer() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        sender.send(Decoded::Samples(vec![0.; 2])).unwrap();
        let control = Arc::new(Control::default());
        let worker_control = control.clone();
        let worker = thread::spawn(move || send(&sender, &worker_control.cancelled, Decoded::End));
        control.cancelled.store(true, Ordering::SeqCst);
        assert!(!worker.join().unwrap());
    }

    #[test]
    fn damaged_audio_returns_an_error_instead_of_completing() {
        let (sender, _receiver) = mpsc::sync_channel(1);
        assert!(
            decode(
                Box::new(Cursor::new(b"not audio")),
                &sender,
                &Control::default()
            )
            .is_err()
        );
    }

    #[test]
    fn failed_flac_integrity_check_does_not_complete_the_track() {
        let mut bytes = include_bytes!("../tests/fixtures/stereo24.flac").to_vec();
        // FLAC STREAMINFO starts after the 8-byte signature/block header.
        // Its last 16 bytes contain the decoded-audio MD5.
        bytes[26] ^= 1;
        let (sender, receiver) = mpsc::sync_channel(64);
        assert!(decode(Box::new(Cursor::new(bytes)), &sender, &Control::default()).is_err());
        assert!(
            !receiver
                .try_iter()
                .any(|message| matches!(message, Decoded::End))
        );
    }

    #[test]
    fn truncated_http_mp3_is_not_mistaken_for_end_of_track() {
        let (source, mut connection) = crate::audio_file::tests::stream(Some(4000));
        let control = Control::default();
        let reader = source.reader(control.cancelled.clone()).unwrap();
        connection
            .write_all(&include_bytes!("../tests/fixtures/stereo.mp3")[..2000])
            .unwrap();
        connection.shutdown(Shutdown::Write).unwrap();
        let (sender, receiver) = mpsc::sync_channel(64);
        assert!(decode(Box::new(reader), &sender, &control).is_err());
        assert!(
            !receiver
                .try_iter()
                .any(|message| matches!(message, Decoded::End))
        );
    }

    #[test]
    fn decodes_while_downloading_and_seeks_reuse_the_same_audio_file() {
        let bytes = include_bytes!("../tests/fixtures/stereo-gapless.mp3");
        let (source, mut connection) = crate::audio_file::tests::stream(Some(bytes.len() as u64));
        let control = Arc::new(Control::default());
        let reader = source.reader(control.cancelled.clone()).unwrap();
        let (sender, receiver) = mpsc::sync_channel(256);
        let decoder = thread::spawn(move || {
            decode_at(
                Box::new(reader),
                &sender,
                &control.cancelled,
                Duration::ZERO,
            )
        });
        connection.write_all(&bytes[..4096]).unwrap();
        let format = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("decoder waited for the entire download");
        assert!(matches!(format, Decoded::Format(_, _)));
        let first = receiver
            .recv_timeout(Duration::from_secs(2))
            .expect("no samples before download completion");
        assert!(matches!(first, Decoded::Samples(_)));
        connection.write_all(&bytes[4096..]).unwrap();
        decoder.join().unwrap().unwrap();
        let mut messages = vec![format, first];
        messages.extend(receiver.try_iter());
        assert!(matches!(messages.last(), Some(Decoded::End)));
        let reference = samples(decoded(bytes));
        assert_eq!(samples(messages), reference);

        let reader = source.reader(Arc::default()).unwrap();
        let (sender, receiver) = mpsc::sync_channel(256);
        decode_at(
            Box::new(reader),
            &sender,
            &Control::default().cancelled,
            Duration::from_millis(700),
        )
        .unwrap();
        let actual = samples(receiver.try_iter().collect());
        let expected = &reference[700 * 48 * 2..];
        assert_eq!(actual.len(), expected.len());
        assert!(
            actual
                .iter()
                .zip(expected)
                .all(|(actual, expected)| (actual - expected).abs() < 0.00001)
        );
    }
}
