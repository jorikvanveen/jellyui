use std::{
    cell::{Cell, RefCell},
    error::Error,
    f64::consts::TAU,
    io::Cursor,
    process::Command,
    rc::Rc,
    thread,
    time::{Duration, Instant},
};

use clap::{Parser, ValueEnum};
use pipewire::{self as pw, properties::properties, spa};
use spa::pod::{serialize::PodSerializer, Object, Pod, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Transition {
    Prepared,
    Restart,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Format {
    F32,
    S16,
    S24,
}

impl Format {
    fn bytes(self) -> usize {
        match self {
            Self::F32 => 4,
            Self::S16 => 2,
            Self::S24 => 3,
        }
    }

    fn spa(self) -> spa::param::audio::AudioFormat {
        match self {
            Self::F32 => spa::param::audio::AudioFormat::F32LE,
            Self::S16 => spa::param::audio::AudioFormat::S16LE,
            Self::S24 => spa::param::audio::AudioFormat::S24LE,
        }
    }

    fn write(self, sample: f64, dst: &mut [u8]) {
        match self {
            Self::F32 => dst.copy_from_slice(&(sample as f32).to_le_bytes()),
            Self::S16 => dst.copy_from_slice(
                &((sample * 32768.0).round().clamp(-32768.0, 32767.0) as i16).to_le_bytes(),
            ),
            Self::S24 => dst.copy_from_slice(
                &((sample * 8388608.0).round().clamp(-8388608.0, 8388607.0) as i32).to_le_bytes()
                    [..3],
            ),
        }
    }
}

#[derive(Debug, Parser)]
#[command(about = "Compare a prepared PipeWire stream handover with a cold stream restart")]
struct Args {
    /// Prepared warms both streams with silence; restart connects the second afterward
    #[arg(long, value_enum, default_value_t = Transition::Prepared)]
    transition: Transition,
    /// Warm both streams with silence, verify formats and buffers, then exit without a tone
    #[arg(long)]
    prepare_only: bool,
    /// Sample rate before the transition
    #[arg(long, default_value_t = 44100)]
    from: u32,
    /// Sample rate after the transition
    #[arg(long, default_value_t = 96000)]
    to: u32,
    /// Seconds of audio on EACH side of the transition
    #[arg(long, default_value_t = 5.0)]
    seconds: f64,
    #[arg(long, default_value_t = 440.0)]
    frequency: f64,
    /// Peak amplitude, 0 < amplitude <= 1; 0.05 is about -26 dBFS
    #[arg(long, default_value_t = 0.05)]
    amplitude: f64,
    #[arg(long, value_enum, default_value_t = Format::F32)]
    from_format: Format,
    #[arg(long, value_enum, default_value_t = Format::F32)]
    to_format: Format,
    /// PipeWire output node.name or object.serial; otherwise use the default output
    #[arg(long)]
    target: Option<String>,
    /// Force the graph rate while each stream is active. May affect other apps.
    /// Without this, PipeWire may keep its rate and resample instead.
    #[arg(long)]
    force_rate: bool,
    /// Skip the pw-top snapshots printed during playback
    #[arg(long)]
    no_monitor: bool,
}

impl Args {
    fn validate(&self) -> Result<()> {
        if self.prepare_only && self.transition != Transition::Prepared {
            return Err("--prepare-only requires --transition prepared".into());
        }
        if self.force_rate && self.transition == Transition::Prepared {
            return Err("--force-rate requires --transition restart; prepared streams share the existing graph rate".into());
        }
        if !(8000..=384000).contains(&self.from) || !(8000..=384000).contains(&self.to) {
            return Err("sample rates must be between 8000 and 384000 Hz".into());
        }
        if !self.seconds.is_finite() || !(0.25..=60.0).contains(&self.seconds) {
            return Err("--seconds must be between 0.25 and 60 per segment".into());
        }
        if !self.frequency.is_finite()
            || self.frequency <= 0.0
            || self.frequency >= self.from.min(self.to) as f64 / 2.0
        {
            return Err(
                "frequency must be positive and below both rates' Nyquist frequency".into(),
            );
        }
        if !self.amplitude.is_finite() || self.amplitude <= 0.0 || self.amplitude > 1.0 {
            return Err("amplitude must be greater than 0 and at most 1".into());
        }
        if self
            .target
            .as_ref()
            .is_some_and(|s| s.is_empty() || s.contains('\0'))
        {
            return Err("target must be a nonempty PipeWire node name or serial".into());
        }
        Ok(())
    }
}

struct Oscillator {
    phase: f64,
    frequency: f64,
    amplitude: f64,
}

impl Oscillator {
    fn next(&mut self, rate: u32) -> f64 {
        let value = self.phase.sin() * self.amplitude;
        self.phase = (self.phase + TAU * self.frequency / rate as f64) % TAU;
        value
    }
}

#[derive(Default)]
struct Report {
    first_buffer: Option<Instant>,
    drained: Option<Instant>,
    error: Option<String>,
    node_id: u32,
    negotiated: Option<String>,
    buffers: u32,
}

struct Output<'c> {
    // Remove hooks before destroying the stream. The core outlives this object.
    _listener: pw::stream::StreamListener<Segment>,
    stream: pw::stream::StreamBox<'c>,
    _core_listener: pw::core::Listener,
    report: Rc<RefCell<Report>>,
    enabled: Rc<Cell<bool>>,
}

struct Segment {
    oscillator: Rc<RefCell<Oscillator>>,
    report: Rc<RefCell<Report>>,
    rate: u32,
    format: Format,
    frames: u64,
    total: u64,
    first: bool,
    draining: bool,
    enabled: Rc<Cell<bool>>,
}

impl Segment {
    fn process(&mut self, stream: &pw::stream::Stream) {
        if self.draining {
            return;
        }
        let Some(mut buffer) = stream.dequeue_buffer() else {
            return;
        };
        let requested = buffer.requested() as usize;
        let Some(data) = buffer.datas_mut().first_mut() else {
            self.report.borrow_mut().error = Some("PipeWire returned an empty buffer".into());
            return;
        };
        let stride = self.format.bytes() * 2;
        let Some(bytes) = data.data() else {
            self.report.borrow_mut().error = Some("PipeWire buffer is not mapped".into());
            return;
        };
        let capacity = bytes.len() / stride;
        let enabled = self.enabled.get();
        let count = capacity
            .min(if requested == 0 { capacity } else { requested })
            .min(if enabled {
                (self.total - self.frames) as usize
            } else {
                capacity
            });
        if count == 0 {
            return;
        }
        if enabled {
            let mut report = self.report.borrow_mut();
            report.first_buffer.get_or_insert_with(Instant::now);
            report.node_id = stream.node_id();
        }
        if !enabled {
            bytes[..count * stride].fill(0);
        } else {
            let mut oscillator = self.oscillator.borrow_mut();
            for frame in bytes[..count * stride].chunks_exact_mut(stride) {
                // Ramp only the very beginning and end of the entire demo, never the seam.
                let ramp = (self.rate as f64 * 0.005).max(1.0);
                let envelope = if self.first {
                    (self.frames as f64 / ramp).min(1.0)
                } else {
                    ((self.total - self.frames - 1) as f64 / ramp).min(1.0)
                };
                let sample = oscillator.next(self.rate) * envelope;
                for channel in frame.chunks_exact_mut(self.format.bytes()) {
                    self.format.write(sample, channel);
                }
                self.frames += 1;
            }
        }
        let chunk = data.chunk_mut();
        *chunk.offset_mut() = 0;
        *chunk.stride_mut() = stride as i32;
        *chunk.size_mut() = (count * stride) as u32;
        // Buffer drop queues the final frames BEFORE requesting drain.
        drop(buffer);
        if self.frames == self.total {
            self.draining = true;
            if let Err(error) = stream.flush(true) {
                self.report.borrow_mut().error = Some(format!("drain failed: {error}"));
            }
        }
    }
}

fn connect_segment<'c>(
    core: &'c pw::core::CoreRc,
    args: &Args,
    oscillator: Rc<RefCell<Oscillator>>,
    first: bool,
    silent: bool,
) -> Result<Output<'c>> {
    let (rate, format) = if first {
        (args.from, args.from_format)
    } else {
        (args.to, args.to_format)
    };
    let mut props = properties! {
        "application.name" => "Jellyui rate-switch demo",
        "node.name" => if first { "jellyui-rate-switch-1" } else { "jellyui-rate-switch-2" },
        "media.type" => "Audio",
        "media.category" => "Playback",
        "media.role" => "Music",
        "node.rate" => format!("1/{rate}"),
        "node.latency" => format!("512/{rate}"),
        "stream.dont-remix" => "true",
        "node.dont-fallback" => "true",
    };
    if let Some(target) = &args.target {
        props.insert("target.object", target.as_str());
        props.insert("node.dont-reconnect", "true");
    }
    if args.force_rate {
        props.insert("node.force-rate", rate.to_string());
    }
    let report = Rc::new(RefCell::new(Report::default()));
    let core_report = report.clone();
    let _core_listener = core
        .add_listener_local()
        .error(move |_, _, code, message| {
            core_report.borrow_mut().error = Some(format!("PipeWire error {code}: {message}"));
        })
        .register();
    let stream = pw::stream::StreamBox::new(core, "jellyui-rate-switch", props)?;
    let total = (args.seconds * rate as f64).round() as u64;
    let enabled = Rc::new(Cell::new(!silent));
    let _listener = stream
        .add_local_listener_with_user_data(Segment {
            oscillator,
            report: report.clone(),
            rate,
            format,
            frames: 0,
            total,
            first,
            draining: false,
            enabled: enabled.clone(),
        })
        .add_buffer(|_, data, _| {
            data.report.borrow_mut().buffers += 1;
        })
        .remove_buffer(|_, data, _| {
            let mut report = data.report.borrow_mut();
            report.buffers = report.buffers.saturating_sub(1);
        })
        .state_changed(|_, data, _, state| {
            if let pw::stream::StreamState::Error(error) = state {
                data.report.borrow_mut().error = Some(error.to_string());
            }
        })
        .param_changed(|_, data, id, param| {
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Some(param) = param else {
                return;
            };
            let mut info = spa::param::audio::AudioInfoRaw::new();
            if info.parse(param).is_err() {
                data.report.borrow_mut().error =
                    Some("could not parse negotiated PCM format".into());
                return;
            }
            let mut report = data.report.borrow_mut();
            report.negotiated = Some(format!(
                "{:?}, {} Hz, {} channels",
                info.format(),
                info.rate(),
                info.channels()
            ));
            if info.rate() != data.rate
                || info.channels() != 2
                || info.format() != data.format.spa()
            {
                report.error = Some("PipeWire negotiated a different client format".into());
            }
        })
        .process(|stream, data| data.process(stream))
        .drained(|_, data| {
            data.report.borrow_mut().drained = Some(Instant::now());
        })
        .register()?;

    let mut info = spa::param::audio::AudioInfoRaw::new();
    info.set_format(format.spa());
    info.set_rate(rate);
    info.set_channels(2);
    let mut positions = [0; spa::param::audio::MAX_CHANNELS];
    positions[0] = spa::sys::SPA_AUDIO_CHANNEL_FL;
    positions[1] = spa::sys::SPA_AUDIO_CHANNEL_FR;
    info.set_position(positions);
    let pod = PodSerializer::serialize(
        Cursor::new(Vec::new()),
        &Value::Object(Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        }),
    )?
    .0
    .into_inner();
    let pod = Pod::from_bytes(&pod).ok_or("failed to serialize audio format")?;
    // The local listener runs on this loop. Do not add RT_PROCESS with Rc/RefCell callbacks.
    let flags = pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS;
    stream.connect(spa::utils::Direction::Output, None, flags, &mut [pod])?;

    Ok(Output {
        _listener,
        stream,
        _core_listener,
        report,
        enabled,
    })
}

fn wait_until(
    mainloop: &pw::main_loop::MainLoopRc,
    outputs: &[&Output<'_>],
    interrupted: &Cell<bool>,
    timeout: Duration,
    done: impl Fn() -> bool,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if interrupted.get() {
            return Err("interrupted".into());
        }
        for output in outputs {
            if let Some(error) = output.report.borrow_mut().error.take() {
                return Err(error.into());
            }
        }
        if done() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(
                "timed out waiting for negotiation/playback/drain; check the target and PipeWire links".into(),
            );
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(100)));
    }
}

fn snapshot(label: String, delay: Duration) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        thread::sleep(delay);
        // The first pw-top batch is emitted before node statistics arrive.
        match Command::new("pw-top").args(["-b", "-n", "2"]).output() {
            Ok(output) if output.status.success() => {
                let text = String::from_utf8_lossy(&output.stdout);
                let last_table = text.rfind("S   ID").map_or(text.as_ref(), |i| &text[i..]);
                eprintln!(
                    "\n{label}: pw-top snapshot. Driver RATE is the graph rate; FORMAT is the negotiated stream/device format.\n{last_table}"
                );
            }
            _ => eprintln!(
                "{label}: pw-top unavailable; run it separately to check actual output rates."
            ),
        }
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    args.validate()?;
    pw::init();
    let mainloop = pw::main_loop::MainLoopRc::new(None)?;
    let context = pw::context::ContextRc::new(&mainloop, None)?;
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("cannot connect to PipeWire: {error}"))?;
    let interrupted = Rc::new(Cell::new(false));
    let signal = interrupted.clone();
    let _sigint = mainloop
        .loop_()
        .add_signal_local(pw::loop_::Signal::INT, move || signal.set(true));
    let oscillator = Rc::new(RefCell::new(Oscillator {
        phase: 0.0,
        frequency: args.frequency,
        amplitude: args.amplitude,
    }));
    eprintln!(
        "{} Hz sine, peak {}. {:.2} seconds at {} Hz {:?}, then {:.2} seconds at {} Hz {:?}.\nPhase is preserved. Transition: {:?}.\nRate policy: {}. No exclusive access or persistent configuration changes.\nCheck pw-top FORMAT on the output device to confirm whether its rate actually changes.",
        args.frequency, args.amplitude, args.seconds, args.from, args.from_format,
        args.seconds, args.to, args.to_format, args.transition,
        if args.force_rate { "FORCE graph rate while the stream is active; affects shared clients" } else { "request source rate; PipeWire may resample" },
    );
    let mut monitors = Vec::new();
    let mut reports = Vec::new();
    let timeout = Duration::from_secs_f64(args.seconds + 10.0);
    if args.transition == Transition::Prepared {
        let first = connect_segment(&core, &args, oscillator.clone(), true, true)?;
        let second = connect_segment(&core, &args, oscillator.clone(), false, true)?;
        wait_until(
            &mainloop,
            &[&first, &second],
            &interrupted,
            Duration::from_secs(5),
            || {
                [&first, &second].iter().all(|output| {
                    let report = output.report.borrow();
                    report.negotiated.is_some() && report.buffers > 0
                })
            },
        )?;
        for (index, output) in [&first, &second].iter().enumerate() {
            let report = output.report.borrow();
            if report.first_buffer.is_some() {
                return Err("a prepared stream generated tone samples before activation".into());
            }
            eprintln!(
                "Prepared segment {}: node {}, {}, {} mapped buffers; sending silence, no tone samples yet.",
                index + 1,
                output.stream.node_id(),
                report.negotiated.as_deref().unwrap_or("unknown"),
                report.buffers
            );
        }
        if args.prepare_only {
            eprintln!("Preparation verified. Exiting without playing audio.");
            return Ok(());
        }
        eprintln!(
            "Both streams are ready. Starting playback; handover after {:.2} seconds.",
            args.seconds
        );
        if !args.no_monitor {
            monitors.push(snapshot(
                "Segment 1".into(),
                Duration::from_secs_f64((args.seconds / 2.0).min(0.5)),
            ));
        }
        first.enabled.set(true);
        wait_until(&mainloop, &[&first, &second], &interrupted, timeout, || {
            first.report.borrow().drained.is_some()
        })?;
        // The second stream is already active and sending silence. Switch it to the tone.
        // No new connection, activation, or format update at the seam.
        second.enabled.set(true);
        first.enabled.set(false);
        first.stream.set_active(false)?;
        if !args.no_monitor {
            monitors.push(snapshot(
                "Segment 2".into(),
                Duration::from_secs_f64((args.seconds / 2.0).min(0.5)),
            ));
        }
        wait_until(&mainloop, &[&first, &second], &interrupted, timeout, || {
            second.report.borrow().drained.is_some()
        })?;
        reports.push(std::mem::take(&mut *first.report.borrow_mut()));
        reports.push(std::mem::take(&mut *second.report.borrow_mut()));
    } else {
        for first in [true, false] {
            if !args.no_monitor {
                monitors.push(snapshot(
                    format!("Segment {}", if first { 1 } else { 2 }),
                    Duration::from_secs_f64((args.seconds / 2.0).min(0.5)),
                ));
            }
            let output = connect_segment(&core, &args, oscillator.clone(), first, false)?;
            wait_until(&mainloop, &[&output], &interrupted, timeout, || {
                output.report.borrow().drained.is_some()
            })?;
            reports.push(std::mem::take(&mut *output.report.borrow_mut()));
            // The restart comparison destroys the old stream before connecting the next.
        }
    }
    for monitor in monitors {
        let _ = monitor.join();
    }
    for (index, report) in reports.iter().enumerate() {
        eprintln!(
            "Segment {} client node {}: {}",
            index + 1,
            report.node_id,
            report
                .negotiated
                .as_deref()
                .unwrap_or("format not reported")
        );
    }
    if let (Some(end), Some(start)) = (reports[0].drained, reports[1].first_buffer) {
        eprintln!("Client handover: {:.3} ms from first stream's drained event to second stream's first buffer.\nThis is NOT a measurement of the audible gap or DAC settling time.", start.duration_since(end).as_secs_f64() * 1000.0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase_survives_a_rate_change() {
        let mut tone = Oscillator {
            phase: 0.0,
            frequency: 440.0,
            amplitude: 0.05,
        };
        for _ in 0..1234 {
            tone.next(44100);
        }
        let expected = tone.phase.sin() * tone.amplitude;
        assert_eq!(tone.next(96000), expected);
        assert!((0.0..TAU).contains(&tone.phase));
    }

    #[test]
    fn pcm_encoding_preserves_sign_and_clamps_full_scale() {
        let mut s16 = [0; 2];
        Format::S16.write(-1.0, &mut s16);
        assert_eq!(i16::from_le_bytes(s16), i16::MIN);
        Format::S16.write(1.0, &mut s16);
        assert_eq!(i16::from_le_bytes(s16), i16::MAX);
        let mut s24 = [0; 3];
        Format::S24.write(-1.0, &mut s24);
        assert_eq!(s24, [0, 0, 128]);
        Format::S24.write(1.0, &mut s24);
        assert_eq!(s24, [255, 255, 127]);
    }

    #[test]
    fn rejects_nonfinite_and_aliased_inputs() {
        let mut args = Args::parse_from(["rate-switch"]);
        args.seconds = f64::NAN;
        assert!(args.validate().is_err());
        args.seconds = 1.0;
        args.frequency = args.from as f64 / 2.0;
        assert!(args.validate().is_err());
        args.frequency = 440.0;
        args.amplitude = f64::INFINITY;
        assert!(args.validate().is_err());
    }
}
