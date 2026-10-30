use super::output::{Output, Pcm};
use super::*;
use std::sync::mpsc::SyncSender;
use std::{io::Cursor, time::Instant};
use std::{io::Write, net::Shutdown};
use symphonia::core::io::ReadOnlySource;
use symphonia::core::{
    audio::{AudioSpec, Channels},
    io::MediaSource,
};

#[test]
fn queue_removal_cancels_only_the_selected_entry_and_prevents_a_stale_handoff() {
    let messages = || {
        decoded(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/continuous16.flac"
        )))
    };
    let first = prepared_messages(1, messages());
    let next = prepared_messages(2, messages());
    let (mut stream, handle) = output(first, Some(next));
    assert_eq!(handle.remove_entry(2), Some(1));
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
    let (mut stream, handle) = output(first, Some(next));
    assert_eq!(handle.remove_entry(1), Some(1));
    stream.fill(&mut bytes, 1.).unwrap();
    assert!(
        !handle
            .events
            .try_iter()
            .any(|event| matches!(event, Event::Started { entry: 2, .. })),
        "removing the current entry must prevent its automatic handoff"
    );
}

fn prepared_messages(entry: usize, messages: Vec<Decoded>) -> Prepared {
    let (source, _connection) = crate::infrastructure::cache::audio::tests::stream(Some(0));
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
            decoded(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continuous16.flac"
            ))),
        );
        let next = prepared_messages(
            1,
            decoded(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continuous24.flac"
            ))),
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
    let reference = decoded(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/continuous16.flac"
    )));
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
            decoded(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continuous16.flac"
            ))),
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
            samples(decoded(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continuous16.flac"
            ))))
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
    let messages = || {
        decoded(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/continuous16.flac"
        )))
    };
    let first = prepared_messages(1, messages());
    let old_next = prepared_messages(2, messages());
    let replacement = prepared_messages(3, messages());
    let (mut output, handle) = output(first, Some(old_next));
    assert!(
        handle.events.try_recv().is_err(),
        "preparation must not report a play"
    );
    handle.set_next(1, Some(replacement));
    let mut bytes = vec![0; 174 * 16];
    assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 174);
    assert!(handle.position(1).is_none());
    assert_eq!(handle.position(3).unwrap().1, 1);
    // The UI can still display the preceding track until its next poll.
    handle.set_next(1, Some(prepared_messages(3, messages())));
    assert_eq!(output.fill(&mut bytes, 1.).unwrap(), 172);
    assert!(
        output.finished,
        "a stale update must not replay the current track"
    );
    assert_eq!(
        handle
            .events
            .try_iter()
            .filter_map(|event| match event {
                Event::Started { entry, .. } => Some(entry),
                _ => None,
            })
            .collect::<Vec<_>>(),
        [1, 3]
    );
}

#[test]
fn an_unready_next_decoder_waits_without_finishing_or_reporting_phantom_audio() {
    let first = prepared_messages(
        1,
        decoded(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/continuous16.flac"
        ))),
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
        let messages = decoded(include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/continuous16.flac"
        )));
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
            samples(decoded(include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/continuous16.flac"
            ))))
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
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo-gapless.mp3"
    ));
    for provide_audio in [false, true] {
        let (source, mut connection) =
            crate::infrastructure::cache::audio::tests::stream(Some(bytes.len() as u64));
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
        crate::infrastructure::cache::audio::tests::connect_mode(true);
    let prefix = crate::infrastructure::cache::audio::PREFETCH_BYTES as usize;
    let audio = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo-gapless.mp3"
    ));
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
    let mut messages = decoded(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/continuous16.flac"
    )));
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
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo24.flac"
    ));
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
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/stereo.mp3"
            ))
            .as_slice(),
            50u64,
        ),
        (
            include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/stereo-gapless.mp3"
            ))
            .as_slice(),
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
    let messages = decoded(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo24.flac"
    )));
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
    let messages = decoded(include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo.mp3"
    )));
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
fn failed_flac_integrity_check_does_not_complete_the_track() {
    let mut bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo24.flac"
    ))
    .to_vec();
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
    let (source, mut connection) = crate::infrastructure::cache::audio::tests::stream(Some(4000));
    let control = Control::default();
    let reader = source.reader(control.cancelled.clone()).unwrap();
    connection
        .write_all(
            &include_bytes!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/stereo.mp3"
            ))[..2000],
        )
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
    let bytes = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/stereo-gapless.mp3"
    ));
    let (source, mut connection) =
        crate::infrastructure::cache::audio::tests::stream(Some(bytes.len() as u64));
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
