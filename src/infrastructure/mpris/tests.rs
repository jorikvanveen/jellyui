use super::*;
use std::{
    io::{BufRead, BufReader},
    process::{Child, Command as Process, Stdio},
    time::Instant,
};
use zbus::blocking::Proxy;

struct Bus {
    process: Child,
    address: String,
}
impl Bus {
    fn new() -> Self {
        let mut process = Process::new("dbus-daemon")
            .args(["--session", "--nofork", "--nopidfile", "--print-address=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .spawn()
            .expect("dbus-daemon is a test dependency");
        let mut address = String::new();
        BufReader::new(process.stdout.take().unwrap())
            .read_line(&mut address)
            .unwrap();
        Self {
            process,
            address: address.trim().into(),
        }
    }
}
impl Drop for Bus {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[test]
fn desktop_clients_read_metadata_control_playback_and_receive_seek_signals_on_a_private_bus() {
    let mut bus = Bus::new();
    let state = Arc::new(Mutex::new(State::default()));
    let (updates, receiver) = mpsc::channel();
    let bridge = Bridge {
        state: state.clone(),
        updates,
    };
    let (commands, incoming) = async_channel::bounded(64);
    let address = bus.address.clone();
    let worker = thread::spawn(move || serve(state, commands, receiver, Some(&address)).unwrap());
    let connection = Builder::address(bus.address.as_str())
        .unwrap()
        .method_timeout(Duration::from_secs(2))
        .build()
        .unwrap();
    let name = format!(
        "org.mpris.MediaPlayer2.jellyui.instance{}",
        std::process::id()
    );
    let dbus = Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !dbus
        .call::<_, _, bool>("NameHasOwner", &(name.as_str(),))
        .unwrap()
    {
        assert!(Instant::now() < deadline, "MPRIS did not register");
        thread::sleep(Duration::from_millis(5));
    }
    let root = Proxy::new(&connection, name.as_str(), PATH, "org.mpris.MediaPlayer2").unwrap();
    assert_eq!(root.get_property::<String>("Identity").unwrap(), "Jellyui");
    assert_eq!(
        root.get_property::<String>("DesktopEntry").unwrap(),
        crate::APP_ID
    );
    assert!(!root.get_property::<bool>("HasTrackList").unwrap());
    let player = Proxy::new(&connection, name.as_str(), PATH, PLAYER).unwrap();
    let introspection = Proxy::new(
        &connection,
        name.as_str(),
        PATH,
        "org.freedesktop.DBus.Introspectable",
    )
    .unwrap();
    let xml: String = introspection.call("Introspect", &()).unwrap();
    assert!(xml.contains("<signal name=\"Seeked\">"));
    assert_eq!(
        player.get_property::<String>("PlaybackStatus").unwrap(),
        "Stopped"
    );
    assert!(
        player
            .get_property::<HashMap<String, OwnedValue>>("Metadata")
            .unwrap()
            .is_empty()
    );
    assert!(player.call::<_, _, ()>("PlayPause", &()).is_err());
    assert!(incoming.try_recv().is_err());
    let snapshot = Snapshot {
        metadata: Metadata {
            entry: Some(42),
            title: "Track".into(),
            album: "Album".into(),
            artists: vec!["Artist A".into(), "Artist B".into()],
            length: Some(120_000_000),
            ..Metadata::default()
        },
        status: "Paused",
        position: 3_000_000,
        volume: 0.5,
        can_play: true,
        can_pause: true,
        can_seek: true,
        artwork: Some((1, Arc::from(b"local cover".as_slice()))),
        ..Snapshot::default()
    };
    bridge.publish(snapshot.clone());
    let deadline = Instant::now() + Duration::from_secs(3);
    let meta = loop {
        let meta = player
            .get_property::<HashMap<String, OwnedValue>>("Metadata")
            .unwrap();
        if meta.contains_key("mpris:artUrl") {
            break meta;
        }
        assert!(Instant::now() < deadline, "artwork was not published");
        thread::sleep(Duration::from_millis(5));
    };
    assert_eq!(
        OwnedObjectPath::try_from(meta["mpris:trackid"].try_clone().unwrap())
            .unwrap()
            .as_str(),
        track_path(42)
    );
    assert_eq!(i64::try_from(&meta["mpris:length"]).unwrap(), 120_000_000);
    assert_eq!(
        Vec::<String>::try_from(meta["xesam:artist"].try_clone().unwrap()).unwrap(),
        ["Artist A", "Artist B"]
    );
    let art_url = <&str>::try_from(&meta["mpris:artUrl"]).unwrap();
    let art_path = url::Url::parse(art_url).unwrap().to_file_path().unwrap();
    assert_eq!(std::fs::read(&art_path).unwrap(), b"local cover");
    // Position has no PropertiesChanged notifications; query it directly
    // rather than reading the generic proxy's initial property cache.
    let properties = Proxy::new(
        &connection,
        name.as_str(),
        PATH,
        "org.freedesktop.DBus.Properties",
    )
    .unwrap();
    let position: OwnedValue = properties.call("Get", &(PLAYER, "Position")).unwrap();
    assert_eq!(i64::try_from(position).unwrap(), 3_000_000);
    for (method, expected) in [
        ("Play", "Play"),
        ("Pause", "Pause"),
        ("PlayPause", "PlayPause"),
        ("Stop", "Stop"),
        ("Next", "Next"),
        ("Previous", "Previous"),
    ] {
        player.call::<_, _, ()>(method, &()).unwrap();
        assert_eq!(format!("{:?}", incoming.try_recv().unwrap()), expected);
    }
    player.call::<_, _, ()>("Seek", &(-1_000_000i64,)).unwrap();
    assert!(matches!(
        incoming.try_recv().unwrap(),
        Command::Seek(-1_000_000)
    ));
    let path = OwnedObjectPath::try_from(track_path(42)).unwrap();
    player
        .call::<_, _, ()>("SetPosition", &(path, 8_000_000i64))
        .unwrap();
    assert!(
        matches!(incoming.try_recv().unwrap(), Command::SetPosition {track, position: 8_000_000} if track == track_path(42))
    );
    player.set_property("Volume", 0.25f64).unwrap();
    assert!(matches!(incoming.try_recv().unwrap(), Command::Volume(value) if value == 0.25));
    assert!(player.set_property("Shuffle", true).is_err());
    assert!(player.set_property("Rate", 2f64).is_err());
    assert!(player.set_property("LoopStatus", "Track").is_err());
    assert!(player.set_property("Volume", f64::NAN).is_err());
    for (method, expected) in [("Raise", "Raise"), ("Quit", "Quit")] {
        root.call::<_, _, ()>(method, &()).unwrap();
        assert_eq!(format!("{:?}", incoming.try_recv().unwrap()), expected);
    }
    assert!(
        player
            .call::<_, _, ()>("OpenUri", &("https://example.invalid/",))
            .is_err()
    );
    let mut signals = player.receive_signal("Seeked").unwrap();
    let (result, received) = mpsc::channel();
    let listener = thread::spawn(move || {
        let signal = signals.next().unwrap();
        result
            .send(signal.body().deserialize::<(i64,)>().unwrap().0)
            .unwrap();
    });
    bridge.publish(Snapshot {
        position: 8_000_000,
        ..snapshot
    });
    bridge.seeked(41, Duration::from_secs(9)); // A previous queue entry is stale.
    bridge.seeked(42, Duration::from_secs(8));
    let signal = received.recv_timeout(Duration::from_secs(3));
    if signal.is_err() {
        let _ = bus.process.kill();
    }
    assert_eq!(signal.unwrap(), 8_000_000);
    listener.join().unwrap();
    drop(bridge);
    worker.join().unwrap();
    assert!(
        !art_path.exists(),
        "temporary artwork should be removed on shutdown"
    );
}
