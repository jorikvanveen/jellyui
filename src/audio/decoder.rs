use super::{device::positions, failure};
use crate::domain::Error;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{SyncSender, TrySendError},
    },
    thread,
    time::Duration,
};
use symphonia::core::{
    audio::AudioSpec,
    formats::{SeekMode, SeekTo, TrackType, probe::Hint},
    io::{MediaSource, MediaSourceStream},
    units::{Time, Timestamp},
};
pub(super) enum Decoded {
    Format(AudioSpec, Duration),
    Samples(Vec<f64>),
    End,
    Failed(Error),
}

pub(super) fn send(
    sender: &SyncSender<Decoded>,
    cancelled: &AtomicBool,
    mut message: Decoded,
) -> bool {
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

pub(super) fn decode_at(
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
