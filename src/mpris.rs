//! Session-bus integration. D-Bus work stays off GPUI's thread; commands return
//! through an async channel and properties read the latest playback snapshot.
use std::{
    collections::HashMap,
    io::{self, Write},
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use async_channel::{Receiver, Sender};
use zbus::{
    blocking::{Connection, connection::Builder},
    fdo,
    zvariant::{OwnedObjectPath, OwnedValue, Value},
};

const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";

#[derive(Debug)]
pub enum Command {
    Play,
    Pause,
    PlayPause,
    Stop,
    Next,
    Previous,
    Raise,
    Quit,
    Seek(i64),
    SetPosition { track: String, position: i64 },
    Volume(f64),
}

#[derive(Clone, Default, PartialEq)]
pub struct Metadata {
    pub entry: Option<usize>,
    pub title: String,
    pub album: String,
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub length: Option<i64>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
}

#[derive(Clone)]
pub struct Snapshot {
    pub metadata: Metadata,
    pub status: &'static str,
    pub position: i64,
    pub volume: f64,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_seek: bool,
    pub artwork: Option<(u64, Arc<[u8]>)>,
}

impl Default for Snapshot {
    fn default() -> Self {
        Self {
            metadata: Metadata::default(),
            status: "Stopped",
            position: 0,
            volume: 1.,
            can_next: false,
            can_previous: false,
            can_play: false,
            can_pause: false,
            can_seek: false,
            artwork: None,
        }
    }
}

impl Snapshot {
    fn properties_equal(&self, other: &Self) -> bool {
        self.metadata == other.metadata
            && self.status == other.status
            && self.volume == other.volume
            && self.can_next == other.can_next
            && self.can_previous == other.can_previous
            && self.can_play == other.can_play
            && self.can_pause == other.can_pause
            && self.can_seek == other.can_seek
            && self.artwork.as_ref().map(|art| art.0) == other.artwork.as_ref().map(|art| art.0)
    }
}

#[derive(Default)]
struct State {
    snapshot: Snapshot,
    art_url: Option<String>,
}

enum Update {
    Changed,
    Seeked(usize, i64),
}

pub struct Bridge {
    state: Arc<Mutex<State>>,
    updates: mpsc::Sender<Update>,
}

impl Bridge {
    pub fn start() -> io::Result<(Self, Receiver<Command>)> {
        let state = Arc::new(Mutex::new(State::default()));
        let (updates, receiver) = mpsc::channel();
        let (commands, incoming) = async_channel::bounded(64);
        let worker_state = state.clone();
        thread::Builder::new().name("mpris".into()).spawn(move || {
            if let Err(error) = serve(worker_state, commands, receiver, None) {
                tracing::warn!(%error, "MPRIS is unavailable; playback remains available in Jellyui");
            }
        })?;
        Ok((Self { state, updates }, incoming))
    }

    pub fn publish(&self, snapshot: Snapshot) {
        let mut state = self.state.lock().unwrap();
        let changed = !state.snapshot.properties_equal(&snapshot);
        state.snapshot = snapshot;
        drop(state);
        if changed {
            let _ = self.updates.send(Update::Changed);
        }
    }

    pub fn seeked(&self, entry: usize, position: Duration) {
        let _ = self
            .updates
            .send(Update::Seeked(entry, microseconds(position)));
    }
}

pub fn microseconds(position: Duration) -> i64 {
    position.as_micros().min(i64::MAX as u128) as i64
}

pub fn track_path(entry: usize) -> String {
    format!("{PATH}/track/t{entry}")
}

fn send(commands: &Sender<Command>, command: Command) -> fdo::Result<()> {
    commands
        .try_send(command)
        .map_err(|_| fdo::Error::Failed("The player cannot accept this command right now.".into()))
}

struct Root {
    commands: Sender<Command>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2")]
impl Root {
    fn raise(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Raise)
    }
    fn quit(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Quit)
    }
    #[zbus(property)]
    fn can_quit(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn can_raise(&self) -> bool {
        true
    }
    #[zbus(property)]
    fn has_track_list(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn identity(&self) -> &str {
        "Jellyui"
    }
    #[zbus(property)]
    fn desktop_entry(&self) -> &str {
        crate::APP_ID
    }
    #[zbus(property)]
    fn supported_uri_schemes(&self) -> Vec<String> {
        Vec::new()
    }
    #[zbus(property)]
    fn supported_mime_types(&self) -> Vec<String> {
        Vec::new()
    }
}

struct Player {
    state: Arc<Mutex<State>>,
    commands: Sender<Command>,
}

#[zbus::interface(name = "org.mpris.MediaPlayer2.Player")]
impl Player {
    fn next(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Next)
    }
    fn previous(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Previous)
    }
    fn pause(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Pause)
    }
    fn play_pause(&self) -> fdo::Result<()> {
        if !self.state.lock().unwrap().snapshot.can_pause {
            return Err(fdo::Error::NotSupported(
                "There is no track to play.".into(),
            ));
        }
        send(&self.commands, Command::PlayPause)
    }
    fn stop(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Stop)
    }
    fn play(&self) -> fdo::Result<()> {
        send(&self.commands, Command::Play)
    }
    fn seek(&self, offset: i64) -> fdo::Result<()> {
        send(&self.commands, Command::Seek(offset))
    }
    fn set_position(&self, track_id: OwnedObjectPath, position: i64) -> fdo::Result<()> {
        send(
            &self.commands,
            Command::SetPosition {
                track: track_id.to_string(),
                position,
            },
        )
    }
    fn open_uri(&self, _uri: &str) -> fdo::Result<()> {
        Err(fdo::Error::NotSupported(
            "Open music through the Jellyfin library.".into(),
        ))
    }
    #[zbus(signal)]
    async fn seeked(
        emitter: &zbus::object_server::SignalEmitter<'_>,
        position: i64,
    ) -> zbus::Result<()>;
    #[zbus(property)]
    fn playback_status(&self) -> String {
        self.state.lock().unwrap().snapshot.status.into()
    }
    #[zbus(property)]
    fn loop_status(&self) -> &str {
        "None"
    }
    #[zbus(property)]
    fn set_loop_status(&self, value: &str) -> zbus::Result<()> {
        if value == "None" {
            Ok(())
        } else {
            Err(fdo::Error::NotSupported("Repeat is not available.".into()).into())
        }
    }
    #[zbus(property)]
    fn rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn set_rate(&self, value: f64) -> zbus::Result<()> {
        if value == 1. {
            Ok(())
        } else {
            Err(fdo::Error::NotSupported("Playback speed is fixed.".into()).into())
        }
    }
    #[zbus(property)]
    fn shuffle(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn set_shuffle(&self, value: bool) -> zbus::Result<()> {
        if value {
            Err(fdo::Error::NotSupported("Shuffle is not available.".into()).into())
        } else {
            Ok(())
        }
    }
    #[zbus(property)]
    fn metadata(&self) -> HashMap<String, OwnedValue> {
        metadata(&self.state.lock().unwrap())
    }
    #[zbus(property)]
    fn volume(&self) -> f64 {
        self.state.lock().unwrap().snapshot.volume
    }
    #[zbus(property)]
    fn set_volume(&self, value: f64) -> zbus::Result<()> {
        if !value.is_finite() {
            return Err(fdo::Error::InvalidArgs("Volume must be finite.".into()).into());
        }
        let value = value.clamp(0., 1.);
        send(&self.commands, Command::Volume(value))?;
        self.state.lock().unwrap().snapshot.volume = value;
        Ok(())
    }
    #[zbus(property(emits_changed_signal = "false"))]
    fn position(&self) -> i64 {
        self.state.lock().unwrap().snapshot.position
    }
    #[zbus(property)]
    fn minimum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn maximum_rate(&self) -> f64 {
        1.
    }
    #[zbus(property)]
    fn can_go_next(&self) -> bool {
        self.state.lock().unwrap().snapshot.can_next
    }
    #[zbus(property)]
    fn can_go_previous(&self) -> bool {
        self.state.lock().unwrap().snapshot.can_previous
    }
    #[zbus(property)]
    fn can_play(&self) -> bool {
        self.state.lock().unwrap().snapshot.can_play
    }
    #[zbus(property)]
    fn can_pause(&self) -> bool {
        self.state.lock().unwrap().snapshot.can_pause
    }
    #[zbus(property)]
    fn can_seek(&self) -> bool {
        self.state.lock().unwrap().snapshot.can_seek
    }
    #[zbus(property)]
    fn can_control(&self) -> bool {
        true
    }
}

fn value(value: impl Into<Value<'static>>) -> OwnedValue {
    value.into().try_into().expect("owned MPRIS value")
}

fn metadata(state: &State) -> HashMap<String, OwnedValue> {
    let meta = &state.snapshot.metadata;
    let Some(entry) = meta.entry else {
        return HashMap::new();
    };
    let mut result = HashMap::from([
        (
            "mpris:trackid".into(),
            value(OwnedObjectPath::try_from(track_path(entry)).unwrap()),
        ),
        ("xesam:title".into(), value(meta.title.clone())),
        ("xesam:album".into(), value(meta.album.clone())),
        ("xesam:artist".into(), value(meta.artists.clone())),
        (
            "xesam:albumArtist".into(),
            value(meta.album_artists.clone()),
        ),
    ]);
    if let Some(length) = meta.length {
        result.insert("mpris:length".into(), value(length));
    }
    if let Some(number) = meta.track_number {
        result.insert(
            "xesam:trackNumber".into(),
            value(number.min(i32::MAX as u32) as i32),
        );
    }
    if let Some(number) = meta.disc_number {
        result.insert(
            "xesam:discNumber".into(),
            value(number.min(i32::MAX as u32) as i32),
        );
    }
    if let Some(url) = &state.art_url {
        result.insert("mpris:artUrl".into(), value(url.clone()));
    }
    result
}

fn changed_properties(state: &State) -> HashMap<String, OwnedValue> {
    let snapshot = &state.snapshot;
    HashMap::from([
        ("PlaybackStatus".into(), value(snapshot.status.to_owned())),
        ("Metadata".into(), value(metadata(state))),
        ("Volume".into(), value(snapshot.volume)),
        ("CanGoNext".into(), value(snapshot.can_next)),
        ("CanGoPrevious".into(), value(snapshot.can_previous)),
        ("CanPlay".into(), value(snapshot.can_play)),
        ("CanPause".into(), value(snapshot.can_pause)),
        ("CanSeek".into(), value(snapshot.can_seek)),
    ])
}

fn serve(
    state: Arc<Mutex<State>>,
    commands: Sender<Command>,
    updates: mpsc::Receiver<Update>,
    address: Option<&str>,
) -> zbus::Result<()> {
    let name = format!(
        "org.mpris.MediaPlayer2.jellyui.instance{}",
        std::process::id()
    );
    let builder = match address {
        Some(address) => Builder::address(address)?,
        None => Builder::session()?,
    };
    let connection: Connection = builder
        .serve_at(
            PATH,
            Root {
                commands: commands.clone(),
            },
        )?
        .serve_at(
            PATH,
            Player {
                state: state.clone(),
                commands,
            },
        )?
        .name(name.as_str())?
        .build()?;
    tracing::info!(name, "MPRIS media controls registered");
    let directory = tempfile::Builder::new()
        .prefix("jellyui-mpris-")
        .tempdir()
        .inspect_err(|error| tracing::warn!(%error, "Could not create MPRIS artwork directory"))
        .ok();
    let mut artwork: Option<(u64, tempfile::NamedTempFile)> = None;
    while let Ok(update) = updates.recv() {
        match update {
            Update::Changed => {
                let snapshot = state.lock().unwrap().snapshot.clone();
                if artwork.as_ref().map(|art| art.0) != snapshot.artwork.as_ref().map(|art| art.0) {
                    artwork = snapshot.artwork.as_ref().and_then(|(id, bytes)| {
                        let directory = directory.as_ref()?;
                        let mut file = tempfile::Builder::new()
                            .suffix(".png")
                            .tempfile_in(directory.path())
                            .inspect_err(|error| tracing::warn!(%error, "Could not create MPRIS artwork file"))
                            .ok()?;
                        file.write_all(bytes).inspect_err(|error| tracing::warn!(%error, "Could not write MPRIS artwork")).ok()?;
                        Some((*id, file))
                    });
                }
                let properties = {
                    let mut state = state.lock().unwrap();
                    state.art_url = artwork
                        .as_ref()
                        .filter(|art| {
                            state
                                .snapshot
                                .artwork
                                .as_ref()
                                .is_some_and(|current| current.0 == art.0)
                        })
                        .and_then(|(_, file)| url::Url::from_file_path(file.path()).ok())
                        .map(|url| url.to_string());
                    changed_properties(&state)
                };
                connection.emit_signal(
                    None::<&str>,
                    PATH,
                    "org.freedesktop.DBus.Properties",
                    "PropertiesChanged",
                    &(PLAYER, properties, Vec::<String>::new()),
                )?;
            }
            Update::Seeked(entry, position) => {
                if state.lock().unwrap().snapshot.metadata.entry == Some(entry) {
                    connection.emit_signal(None::<&str>, PATH, PLAYER, "Seeked", &(position,))?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
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
        let worker =
            thread::spawn(move || serve(state, commands, receiver, Some(&address)).unwrap());
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

    #[test]
    fn normal_position_updates_do_not_emit_property_changes_and_times_saturate() {
        let before = Snapshot::default();
        let after = Snapshot {
            position: 100_000,
            ..Snapshot::default()
        };
        assert!(before.properties_equal(&after));
        assert!(!changed_properties(&State::default()).contains_key("Position"));
        assert_eq!(microseconds(Duration::MAX), i64::MAX);
        assert_ne!(track_path(1), track_path(2));
    }
}
