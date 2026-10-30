use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, FileExt},
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use symphonia::core::io::MediaSource;

use crate::domain::Error;
use crate::infrastructure::jellyfin::{AudioStream, Client};

pub const PREFETCH_BYTES: u64 = 256 * 1024;
const CHUNK_BYTES: usize = 32 * 1024;

/// One original audio file, downloaded independently of playback into an
/// anonymous disk file. Clones and new readers share the same download.
#[derive(Clone)]
pub struct AudioFile(Arc<Download>);

struct Download {
    shared: Arc<Shared>,
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    cancelled: AtomicBool,
}

#[derive(Default)]
struct State {
    file: Option<Arc<File>>,
    length: Option<u64>,
    downloaded: u64,
    finished: bool,
    error: Option<Error>,
    prefetch: bool,
}

/// A private cursor into an AudioFile. Reads wait for missing bytes, never
/// expose unwritten parts of the file, and can be cancelled independently.
pub struct Reader {
    source: AudioFile,
    position: u64,
    cancelled: Arc<AtomicBool>,
}

impl AudioFile {
    /// Starts the download without doing network or filesystem work on the caller.
    pub fn new(client: Client, id: String) -> Result<Self, Error> {
        Self::start(client, id, cache_directory())
    }

    pub fn prefetch(client: Client, id: String) -> Result<Self, Error> {
        Self::start_mode(client, id, cache_directory(), true)
    }

    pub fn promote(&self) {
        let mut state = self.0.shared.state.lock().unwrap();
        state.prefetch = false;
        self.0.shared.changed.notify_all();
    }

    fn start(client: Client, id: String, directory: PathBuf) -> Result<Self, Error> {
        Self::start_mode(client, id, directory, false)
    }

    fn start_mode(
        client: Client,
        id: String,
        directory: PathBuf,
        prefetch: bool,
    ) -> Result<Self, Error> {
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                prefetch,
                ..State::default()
            }),
            ..Shared::default()
        });
        let worker = shared.clone();
        thread::Builder::new()
            .name("audio-download".into())
            .spawn(move || {
                let result = download(client, &id, directory, &worker);
                let mut state = worker.state.lock().unwrap();
                if !worker.cancelled.load(Ordering::SeqCst) {
                    match result {
                        Ok(()) => {
                            tracing::debug!(bytes = state.downloaded, "Audio download complete")
                        }
                        Err(error) => {
                            if state.prefetch {
                                tracing::warn!(%error, "Next-track preload failed");
                            } else {
                                tracing::error!(%error, "Audio download failed");
                            }
                            state.error = Some(error);
                        }
                    }
                }
                state.finished = true;
                worker.changed.notify_all();
            })
            .map_err(|error| cache_error("Could not start the audio download.", error))?;
        Ok(Self(Arc::new(Download { shared })))
    }

    pub fn failed(&self) -> bool {
        self.0.shared.state.lock().unwrap().error.is_some()
    }

    /// Waits only for response headers and the backing file, not its contents.
    /// The caller must be a worker thread. Cancelling this reader leaves the
    /// shared download available for another decoder, for example after a seek.
    /// Reading a prefetched file leaves its download capped until promotion.
    pub fn reader(&self, cancelled: Arc<AtomicBool>) -> Result<Reader, Error> {
        let reader = Reader {
            source: self.clone(),
            position: 0,
            cancelled,
        };
        let shared = &self.0.shared;
        let mut state = shared.state.lock().unwrap();
        loop {
            if reader.is_cancelled() {
                return Err(Error::Message("Playback cancelled."));
            }
            if let Some(error) = &state.error {
                return Err(error.clone());
            }
            if state.file.is_some() {
                break;
            }
            state = reader.wait(state);
        }
        drop(state);
        Ok(reader)
    }
}

fn cache_directory() -> PathBuf {
    std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
        .unwrap_or_else(std::env::temp_dir)
        .join("jellyui/audio")
}

impl Drop for Download {
    fn drop(&mut self) {
        // Synchronize with the parked prefetch worker so it cannot miss this
        // notification between checking cancellation and waiting.
        let _state = self.shared.state.lock().unwrap();
        self.shared.cancelled.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
    }
}

fn download(client: Client, id: &str, directory: PathBuf, shared: &Shared) -> Result<(), Error> {
    if shared.cancelled.load(Ordering::SeqCst) {
        return Ok(());
    }
    let prefetch = shared.state.lock().unwrap().prefetch;
    let response = if prefetch {
        client.audio_range(id, 0, Some(PREFETCH_BYTES - 1))?
    } else {
        let body = client.audio(id)?;
        let length = body.content_length();
        AudioStream {
            body,
            length,
            offset: 0,
            end: length,
        }
    };
    let length = response.length;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .map_err(|error| cache_error("Could not create the audio cache directory.", error))?;
    // Anonymous files disappear when their last handle closes, including on a
    // crash. No partial downloads are left behind or mistaken for complete songs.
    let mut file = tempfile::tempfile_in(directory)
        .map_err(|error| cache_error("Could not create an audio cache file.", error))?;
    {
        let mut state = shared.state.lock().unwrap();
        state.file = Some(Arc::new(file.try_clone().map_err(|error| {
            cache_error("Could not open the audio cache file.", error)
        })?));
        state.length = length;
        shared.changed.notify_all();
    }
    let mut downloaded = 0u64;
    let eof = cache_response(
        response,
        &mut file,
        shared,
        &mut downloaded,
        if prefetch { PREFETCH_BYTES } else { u64::MAX },
    )?;
    if shared.cancelled.load(Ordering::SeqCst) {
        return Ok(());
    }
    let complete = length.is_some_and(|length| downloaded == length) || (eof && length.is_none());
    if prefetch && !complete {
        tracing::debug!(bytes = downloaded, "Next-track prefix cached");
        let mut state = shared.state.lock().unwrap();
        while state.prefetch && !shared.cancelled.load(Ordering::SeqCst) {
            state = shared.changed.wait(state).unwrap();
        }
        if shared.cancelled.load(Ordering::SeqCst) {
            return Ok(());
        }
        drop(state);
        // Open a fresh range on promotion; no idle HTTP connection is held
        // during the current song or a long pause.
        let response = client.audio_range(id, downloaded, None)?;
        if length
            .zip(response.length)
            .is_some_and(|(old, new)| old != new)
        {
            return Err(Error::Message("The audio file changed while preloading."));
        }
        shared.state.lock().unwrap().length = response.length.or(length);
        cache_response(response, &mut file, shared, &mut downloaded, u64::MAX)?;
    }
    if !shared.cancelled.load(Ordering::SeqCst) {
        let mut state = shared.state.lock().unwrap();
        if state.length.is_some_and(|length| length != downloaded) {
            return Err(Error::Message(
                "The audio download was incomplete. Retry this track.",
            ));
        }
        state.length = Some(downloaded);
    }
    Ok(())
}

fn cache_response(
    mut response: AudioStream,
    file: &mut File,
    shared: &Shared,
    downloaded: &mut u64,
    limit: u64,
) -> Result<bool, Error> {
    let mut reader = response.body.as_reader();
    let mut bytes = [0; CHUNK_BYTES];
    let mut offset = response.offset;
    if offset > *downloaded {
        return Err(Error::Message("Jellyfin skipped cached audio bytes."));
    }
    // A server may ignore Range and send 200. Verify its repeated prefix,
    // then append only new bytes to the same file.
    while offset < *downloaded && !shared.cancelled.load(Ordering::SeqCst) {
        let size = (*downloaded - offset).min(CHUNK_BYTES as u64) as usize;
        reader
            .read_exact(&mut bytes[..size])
            .map_err(|error| cache_error("Could not resume the audio download.", error))?;
        let mut cached = [0; CHUNK_BYTES];
        file.read_exact_at(&mut cached[..size], offset)
            .map_err(|error| cache_error("Could not read cached audio.", error))?;
        if bytes[..size] != cached[..size] {
            return Err(Error::Message("The audio file changed while preloading."));
        }
        offset += size as u64;
    }
    while *downloaded < limit && !shared.cancelled.load(Ordering::SeqCst) {
        let size = (limit - *downloaded).min(CHUNK_BYTES as u64) as usize;
        let count = reader.read(&mut bytes[..size]).map_err(|error| {
            cache_error(
                "The audio download was interrupted. Retry this track.",
                error,
            )
        })?;
        if count == 0 {
            if response.end.is_some_and(|end| *downloaded != end) {
                return Err(Error::Message(
                    "The audio download was incomplete. Retry this track.",
                ));
            }
            return Ok(true);
        }
        if shared.cancelled.load(Ordering::SeqCst) {
            return Ok(false);
        }
        file.write_all(&bytes[..count])
            .map_err(|error| cache_error("Could not write the audio cache file.", error))?;
        *downloaded += count as u64;
        let mut state = shared.state.lock().unwrap();
        // Publish only bytes that are already readable from the backing file.
        state.downloaded = *downloaded;
        shared.changed.notify_all();
    }
    Ok(false)
}

fn cache_error(message: &'static str, error: impl std::fmt::Display) -> Error {
    tracing::error!(%error, message, "Audio cache failed");
    Error::Message(message)
}

impl Reader {
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
            || self.source.0.shared.cancelled.load(Ordering::SeqCst)
    }

    fn check(&self, state: &State) -> io::Result<()> {
        if self.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "Playback cancelled.",
            ));
        }
        if let Some(error) = &state.error {
            // Demuxers may mistake UnexpectedEof for normal end-of-track.
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                error.clone(),
            ));
        }
        Ok(())
    }

    fn wait<'a>(&self, state: MutexGuard<'a, State>) -> MutexGuard<'a, State> {
        // Decoder cancellation belongs to an individual reader. A bounded wait
        // observes it promptly without cancelling another reader's download.
        self.source
            .0
            .shared
            .changed
            .wait_timeout(state, Duration::from_millis(50))
            .unwrap()
            .0
    }
}

impl Read for Reader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let mut state = self.source.0.shared.state.lock().unwrap();
        loop {
            self.check(&state)?;
            if self.position < state.downloaded {
                let count = (state.downloaded - self.position).min(bytes.len() as u64) as usize;
                let file = state
                    .file
                    .as_ref()
                    .expect("downloaded bytes have a file")
                    .clone();
                drop(state);
                file.read_exact_at(&mut bytes[..count], self.position)?;
                self.position += count as u64;
                return Ok(count);
            }
            if state.finished || state.length.is_some_and(|length| self.position >= length) {
                return Ok(0);
            }
            state = self.wait(state);
        }
    }
}

impl Seek for Reader {
    fn seek(&mut self, from: SeekFrom) -> io::Result<u64> {
        let mut state = self.source.0.shared.state.lock().unwrap();
        self.check(&state)?;
        let position = match from {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::Current(delta) => i128::from(self.position) + i128::from(delta),
            SeekFrom::End(delta) => {
                while state.length.is_none() {
                    state = self.wait(state);
                    self.check(&state)?;
                }
                i128::from(state.length.unwrap()) + i128::from(delta)
            }
        };
        self.position = u64::try_from(position).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidInput, "Invalid audio seek position.")
        })?;
        Ok(self.position)
    }
}

impl MediaSource for Reader {
    fn is_seekable(&self) -> bool {
        true
    }

    fn byte_len(&self) -> Option<u64> {
        self.source.0.shared.state.lock().unwrap().length
    }
}

#[cfg(test)]
pub(crate) mod tests;
