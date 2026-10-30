use super::{
    Control, Event, Volume, failure,
    output::{Output, Track},
};
use crate::domain::Error;
use pipewire::{
    self as pw,
    properties::properties,
    spa::{
        self,
        pod::{Object, Pod, Value, serialize::PodSerializer},
    },
};
use std::{
    cell::RefCell,
    io::Cursor,
    rc::Rc,
    sync::{Arc, Mutex, TryLockError, atomic::Ordering, mpsc},
    thread,
    time::{Duration, Instant},
};
use symphonia::core::audio::{AudioSpec, Channels};
// Serialize stream replacement within Jellyui; never claim exclusive device access.
static OUTPUT: Mutex<()> = Mutex::new(());
#[derive(Default)]
struct Report {
    error: Option<Error>,
    drained: bool,
    draining: bool,
    last_samples: Option<Instant>,
}

pub(super) fn run(
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

/// Symphonia's positioned channels use WAVE order. Keep that order in PipeWire.
pub(super) fn positions(spec: &AudioSpec) -> Result<[u32; spa::param::audio::MAX_CHANNELS], Error> {
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
