use std::{
    ffi::OsStr,
    fs,
    io::{self, Write},
    os::unix::fs::DirBuilderExt,
    path::{Path, PathBuf},
    sync::Mutex,
};

use serde::{Deserialize, Serialize};

use crate::audio::Volume;

// Each writer reads the shared volume after acquiring this lock, so an older
// queued save cannot overwrite a newer setting from the same app instance.
static SAVE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize, Serialize)]
#[serde(default)]
struct Settings {
    volume: f32,
}

impl Default for Settings {
    fn default() -> Self {
        Self { volume: 1. }
    }
}

pub fn path() -> Option<PathBuf> {
    config_path(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn config_path(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("jellyui/settings.json"))
}

pub fn load_volume(path: &Path) -> io::Result<f32> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(Settings::default().volume);
        }
        Err(error) => return Err(error),
    };
    let settings: Settings = serde_json::from_slice(&bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !settings.volume.is_finite() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Invalid saved volume.",
        ));
    }
    Ok(settings.volume.clamp(0., 1.))
}

pub fn save_volume(path: &Path, volume: &Volume) -> io::Result<()> {
    let _lock = SAVE_LOCK.lock().unwrap();
    let settings = Settings {
        volume: volume.level(),
    };
    let directory = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "Settings path has no parent.")
    })?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let mut file = tempfile::NamedTempFile::new_in(directory)?;
    serde_json::to_writer_pretty(&mut file, &settings).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    tracing::debug!(volume = settings.volume, "Volume setting saved");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{os::unix::fs::PermissionsExt, thread};

    #[test]
    fn settings_respect_xdg_config_home_with_home_fallback() {
        assert_eq!(
            config_path(
                Some(OsStr::new("/config")),
                Some(OsStr::new("/home/listener"))
            )
            .unwrap(),
            PathBuf::from("/config/jellyui/settings.json")
        );
        for xdg in [None, Some(OsStr::new("")), Some(OsStr::new("relative"))] {
            assert_eq!(
                config_path(xdg, Some(OsStr::new("/home/listener"))).unwrap(),
                PathBuf::from("/home/listener/.config/jellyui/settings.json")
            );
        }
        assert!(config_path(None, None).is_none());
    }

    #[test]
    fn volume_and_mute_persist_across_loads_with_atomic_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("jellyui/settings.json");
        assert_eq!(load_volume(&path).unwrap(), 1.);
        let volume = Volume::default();
        for level in [0.37, 0., 1.] {
            volume.set(level);
            save_volume(&path, &volume).unwrap();
            assert_eq!(load_volume(&path).unwrap(), level);
        }
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn incomplete_settings_default_and_invalid_settings_fail_without_panicking() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        for (text, expected) in [
            ("{}", 1.),
            (r#"{"volume":-1}"#, 0.),
            (r#"{"volume":2}"#, 1.),
        ] {
            fs::write(&path, text).unwrap();
            assert_eq!(load_volume(&path).unwrap(), expected);
        }
        for text in ["invalid", r#"{"volume":"loud"}"#, r#"{"volume":1e39}"#] {
            fs::write(&path, text).unwrap();
            assert!(load_volume(&path).is_err());
        }
        assert!(save_volume(&path.join("blocked/settings.json"), &Volume::default()).is_err());
    }

    #[test]
    fn pending_save_reads_the_latest_volume_instead_of_a_stale_snapshot() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("settings.json");
        let volume = Volume::default();
        volume.set(0.2);
        let guard = SAVE_LOCK.lock().unwrap();
        let worker_volume = volume.clone();
        let worker_path = path.clone();
        let worker = thread::spawn(move || save_volume(&worker_path, &worker_volume));
        volume.set(0.7);
        drop(guard);
        worker.join().unwrap().unwrap();
        assert_eq!(load_volume(&path).unwrap(), 0.7);
    }
}
