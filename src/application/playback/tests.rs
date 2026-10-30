use super::*;

#[test]
fn media_seeks_use_microseconds_and_reject_stale_duplicate_entries_and_invalid_positions() {
    let mut player = Playback {
        state: State::Paused,
        ..Playback::default()
    };
    let track = item(serde_json::json!({"Id":"same", "Name":"Track", "RunTimeTicks":120_000_000}));
    player.queue.append(
        item(serde_json::json!({"Id":"album", "Name":"Album"})),
        vec![track.clone(), track],
    );
    let first = mpris::track_path(player.queue.tracks[0].id);
    let second = mpris::track_path(player.queue.tracks[1].id);
    assert_eq!(
        player.media_seek_target(&first, 1_500_000),
        Some(Duration::from_millis(1500))
    );
    assert_eq!(player.media_seek_target(&first, 0), Some(Duration::ZERO));
    assert!(player.media_seek_target(&first, -1).is_none());
    assert!(player.media_seek_target(&first, 12_000_000).is_none());
    player.queue.advance();
    assert!(player.media_seek_target(&first, 1_000_000).is_none());
    assert!(player.media_seek_target(&second, 1_000_000).is_some());
    player.state = State::Stopped;
    assert!(player.media_seek_target(&second, 1_000_000).is_none());
}

fn item(value: serde_json::Value) -> Item {
    serde_json::from_value(value).unwrap()
}

#[test]
fn prefetch_follows_queue_entry_identity_and_survives_appending() {
    use std::io::{Read, Write};
    let mut player = Playback::default();
    let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
    let track = item(serde_json::json!({"Id":"same-song", "Name":"Same song"}));
    player
        .queue
        .replace(album.clone(), vec![track.clone(), track.clone()], 0);
    let (source, mut connection) = crate::infrastructure::cache::audio::tests::stream(Some(4));
    connection.write_all(b"abcd").unwrap();
    let entry = player.queue.tracks[1].id;
    player.next_audio = Some(PrefetchedAudio {
        entry,
        source,
        decoder: None,
    });
    // Duplicate Jellyfin IDs must not consume the next entry's prefix.
    assert!(player.take_prefetched_audio().is_none());
    player.queue.append(album, vec![track]);
    player.refresh_prefetch();
    assert_eq!(player.next_audio.as_ref().unwrap().entry, entry);
    player.queue.advance();
    let source = player.take_prefetched_audio().unwrap();
    assert!(player.next_audio.is_none());
    let mut bytes = Vec::new();
    source
        .source
        .reader(Arc::default())
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    assert_eq!(bytes, b"abcd");
}

#[test]
fn changing_the_next_queue_entry_discards_its_preload() {
    for replace in [false, true] {
        let mut player = Playback::default();
        let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
        let tracks: Vec<_> = ["first", "second", "third"]
            .into_iter()
            .map(|id| item(serde_json::json!({"Id":id, "Name":id})))
            .collect();
        player.queue.replace(album.clone(), tracks.clone(), 0);
        let (source, _connection) = crate::infrastructure::cache::audio::tests::stream(Some(4));
        player.next_audio = Some(PrefetchedAudio {
            entry: player.queue.tracks[1].id,
            source,
            decoder: None,
        });
        if replace {
            player.queue.replace(album, tracks, 0);
        } else {
            player.queue.move_track(2, 1);
        }
        player.refresh_prefetch();
        // No client in this fixture, so replacement fetching is skipped.
        assert!(player.next_audio.is_none());
    }
}

fn request(album: &str, start: usize) -> AlbumRequest {
    AlbumRequest {
        action: AlbumAction::Play,
        client: Client::restore(
            serde_json::from_value(serde_json::json!({
                "server":"http://localhost/", "user_id":"user", "username":"test",
                "token":"test", "device_id":"test"
            }))
            .unwrap(),
        )
        .unwrap(),
        album: item(serde_json::json!({"Id":album, "Name":album})),
        tracks: Some(vec![
            item(serde_json::json!({"Id":"first", "Name":"First"})),
            item(serde_json::json!({"Id":"second", "Name":"Second"})),
        ]),
        start,
        cover: None,
    }
}

#[test]
fn empty_queues_play_directly_but_existing_or_pending_queues_require_confirmation() {
    let mut player = Playback::default();
    assert!(!player.prepare_play(request("first-album", 0)));
    player.take_play_confirmation(false);
    player.album_requests.push_back(request("pending-album", 0));
    assert!(player.prepare_play(request("replacement", 0)));
    player.cancel_album_requests();
    player.album_request = Some(Task::ready(()));
    assert!(player.prepare_play(request("replacement", 0)));
    player.cancel_album_requests();
    let finished = request("finished", 1);
    player
        .queue
        .replace(finished.album, finished.tracks.unwrap(), 1);
    player.state = State::Finished;
    assert!(player.prepare_play(request("replacement", 0)));
}
