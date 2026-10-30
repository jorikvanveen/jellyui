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

use crate::jellyfin::{AudioStream, Client, Error};

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
pub(crate) mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader},
        net::{Shutdown, TcpListener, TcpStream},
        os::unix::fs::MetadataExt,
        sync::mpsc,
        time::Instant,
    };

    /// A real HTTP response whose body the test can deliver incrementally.
    pub(crate) fn stream(length: Option<u64>) -> (AudioFile, TcpStream) {
        let (source, mut connection, _directory) = connect();
        write!(connection, "HTTP/1.1 200 OK\r\nConnection: close\r\n").unwrap();
        if let Some(length) = length {
            write!(connection, "Content-Length: {length}\r\n").unwrap();
        }
        write!(connection, "\r\n").unwrap();
        // The anonymous file stays valid after the temporary directory is removed.
        let _ = source.reader(Arc::default()).unwrap();
        (source, connection)
    }

    fn connect() -> (AudioFile, TcpStream, tempfile::TempDir) {
        let (source, connection, directory, _) = connect_mode(false);
        (source, connection, directory)
    }

    pub(crate) fn connect_mode(
        prefetch: bool,
    ) -> (AudioFile, TcpStream, tempfile::TempDir, TcpListener) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let session = serde_json::from_value(serde_json::json!({
            "server": format!("http://{}/", listener.local_addr().unwrap()),
            "user_id": "test-user",
            "username": "Listener",
            "token": "test-token",
            "device_id": "test-device"
        }))
        .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let source = AudioFile::start_mode(
            Client::restore(session).unwrap(),
            "track".into(),
            directory.path().join("audio"),
            prefetch,
        )
        .unwrap();
        let (connection, request) = next_request(&listener);
        assert!(request.starts_with("GET /Audio/track/stream?static=true "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("accept-encoding: identity")
        );
        assert!(request.contains("Token=\"test-token\""));
        if prefetch {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains(&format!("range: bytes=0-{}\r\n", PREFETCH_BYTES - 1))
            );
        } else {
            assert!(!request.to_ascii_lowercase().contains("range:"));
        }
        (source, connection, directory, listener)
    }

    fn next_request(listener: &TcpListener) -> (TcpStream, String) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut connection = loop {
            match listener.accept() {
                Ok((connection, _)) => break connection,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "audio request did not arrive");
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("{error}"),
            }
        };
        connection
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        connection
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut request = String::new();
        let mut reader = BufReader::new(&mut connection);
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            request.push_str(&line);
        }
        (connection, request)
    }

    fn completed(source: &AudioFile) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut state = source.0.shared.state.lock().unwrap();
        while !state.finished {
            assert!(Instant::now() < deadline, "download did not finish");
            state = source
                .0
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
    }

    fn prefix_ready(source: &AudioFile) {
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut state = source.0.shared.state.lock().unwrap();
        while state.downloaded < PREFETCH_BYTES && !state.finished {
            assert!(Instant::now() < deadline, "prefix did not arrive");
            state = source
                .0
                .shared
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
        assert!(state.error.is_none(), "{:?}", state.error);
        assert_eq!(state.downloaded, PREFETCH_BYTES);
        assert!(state.prefetch);
        assert!(!state.finished);
    }

    fn partial_response(connection: &mut TcpStream, start: usize, total: usize, bytes: &[u8]) {
        write!(connection,
            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{total}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            start + bytes.len() - 1, bytes.len()
        ).unwrap();
        connection.write_all(bytes).unwrap();
    }

    #[test]
    fn prefetch_is_bounded_and_promotion_reads_cached_bytes_before_resuming() {
        let (source, mut first, _directory, listener) = connect_mode(true);
        let prefix = PREFETCH_BYTES as usize;
        let bytes: Vec<u8> = (0..prefix + 100).map(|i| (i % 251) as u8).collect();
        partial_response(&mut first, 0, bytes.len(), &bytes[..prefix]);
        prefix_ready(&source);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        let inode = source
            .0
            .shared
            .state
            .lock()
            .unwrap()
            .file
            .as_ref()
            .unwrap()
            .metadata()
            .unwrap()
            .ino();

        let mut preview = source.reader(Arc::default()).unwrap();
        let mut preview_bytes = [0; 16];
        preview.read_exact(&mut preview_bytes).unwrap();
        assert_eq!(preview_bytes, bytes[..16]);
        prefix_ready(&source);
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(preview);

        // Cached bytes are usable even while the resumed request awaits headers.
        source.promote();
        let mut reader = source.reader(Arc::default()).unwrap();
        let (mut resumed, request) = next_request(&listener);
        assert!(
            request
                .to_ascii_lowercase()
                .contains(&format!("range: bytes={prefix}-\r\n"))
        );
        let mut head = [0; 16];
        reader.read_exact(&mut head).unwrap();
        assert_eq!(head, bytes[..16]);
        assert_eq!(reader.byte_len(), Some(bytes.len() as u64));
        partial_response(&mut resumed, prefix, bytes.len(), &bytes[prefix..]);
        completed(&source);
        assert!(!source.failed());
        let mut all = Vec::new();
        reader.seek(SeekFrom::Start(0)).unwrap();
        reader.read_to_end(&mut all).unwrap();
        assert_eq!(all, bytes);
        assert_eq!(
            source
                .0
                .shared
                .state
                .lock()
                .unwrap()
                .file
                .as_ref()
                .unwrap()
                .metadata()
                .unwrap()
                .ino(),
            inode
        );
    }

    #[test]
    fn servers_ignoring_ranges_do_not_duplicate_the_prefix_or_accept_changed_audio() {
        for changed in [false, true] {
            let (source, mut first, _directory, listener) = connect_mode(true);
            let prefix = PREFETCH_BYTES as usize;
            let mut bytes = vec![b'a'; prefix + 100];
            write!(
                first,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            // A server ignoring the bounded request can send the entire file;
            // only the prefix is saved before the connection is dropped.
            let _ = first.write_all(&bytes);
            prefix_ready(&source);
            source.promote();
            let mut reader = source.reader(Arc::default()).unwrap();
            let (mut resumed, _) = next_request(&listener);
            if changed {
                bytes[0] = b'b';
            }
            write!(
                resumed,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                bytes.len()
            )
            .unwrap();
            let _ = resumed.write_all(&bytes);
            completed(&source);
            assert_eq!(source.failed(), changed);
            let mut all = Vec::new();
            if changed {
                assert!(reader.read_to_end(&mut all).is_err());
            } else {
                reader.read_to_end(&mut all).unwrap();
                assert_eq!(all, bytes);
            }
        }
    }

    #[test]
    fn short_prefetched_files_are_complete_without_a_second_request() {
        let (source, mut first, _directory, listener) = connect_mode(true);
        partial_response(&mut first, 0, 4, b"abcd");
        completed(&source);
        let mut bytes = Vec::new();
        source
            .reader(Arc::default())
            .unwrap()
            .read_to_end(&mut bytes)
            .unwrap();
        assert_eq!(bytes, b"abcd");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn discarding_a_prefetch_wakes_and_cancels_the_parked_download() {
        let (source, mut first, _directory, listener) = connect_mode(true);
        partial_response(
            &mut first,
            0,
            PREFETCH_BYTES as usize + 100,
            &vec![0; PREFETCH_BYTES as usize],
        );
        prefix_ready(&source);
        let shared = source.0.shared.clone();
        drop(source);
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut state = shared.state.lock().unwrap();
        while !state.finished {
            assert!(
                Instant::now() < deadline,
                "cancelled prefetch stayed parked"
            );
            state = shared
                .changed
                .wait_timeout(state, Duration::from_millis(50))
                .unwrap()
                .0;
        }
        assert!(shared.cancelled.load(Ordering::SeqCst));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn reads_before_download_finishes_and_keeps_downloading_without_a_reader() {
        let (source, mut connection) = stream(Some(10));
        connection.write_all(b"abc").unwrap();
        let mut reader = source.reader(Arc::default()).unwrap();
        assert_eq!(reader.byte_len(), Some(10));
        assert!(reader.is_seekable());
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut bytes = [0; 3];
            reader.read_exact(&mut bytes).unwrap();
            sender.send(bytes).unwrap();
        });
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            *b"abc"
        );
        worker.join().unwrap();
        connection.write_all(b"defghij").unwrap();
        completed(&source);
        let state = source.0.shared.state.lock().unwrap();
        assert_eq!(state.downloaded, 10);
        assert_eq!(state.file.as_ref().unwrap().metadata().unwrap().nlink(), 0);
        drop(state);
        let mut reader = source.reader(Arc::default()).unwrap();
        let mut all = Vec::new();
        reader.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdefghij");
        reader.seek(SeekFrom::Start(0)).unwrap();
        let mut again = Vec::new();
        reader.read_to_end(&mut again).unwrap();
        assert_eq!(again, all);
    }

    #[test]
    fn seeks_wait_for_missing_bytes_and_independent_readers_reuse_the_cache() {
        let (source, mut connection) = stream(Some(10));
        connection.write_all(b"abc").unwrap();
        let mut reader = source.reader(Arc::default()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            reader.seek(SeekFrom::Start(7)).unwrap();
            let mut bytes = [0; 2];
            reader.read_exact(&mut bytes).unwrap();
            sender.send((reader, bytes)).unwrap();
        });
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        connection.write_all(b"defghij").unwrap();
        let (mut reader, bytes) = receiver.recv_timeout(Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
        assert_eq!(&bytes, b"hi");
        reader.seek(SeekFrom::End(-2)).unwrap();
        let mut bytes = [0; 2];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ij");
        assert!(reader.seek(SeekFrom::Current(-11)).is_err());
        reader.seek(SeekFrom::Start(100)).unwrap();
        assert_eq!(reader.read(&mut bytes).unwrap(), 0);
        let mut other = source.reader(Arc::default()).unwrap();
        other.read_exact(&mut bytes).unwrap();
        assert_eq!(&bytes, b"ab");
    }

    #[test]
    fn unknown_lengths_stream_immediately_and_end_seeks_wait_for_completion() {
        let (source, mut connection) = stream(None);
        connection.write_all(b"abc").unwrap();
        let mut reader = source.reader(Arc::default()).unwrap();
        assert_eq!(reader.byte_len(), None);
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut bytes = [0; 3];
            reader.read_exact(&mut bytes).unwrap();
            sender.send(bytes.to_vec()).unwrap();
            reader.seek(SeekFrom::End(-2)).unwrap();
            reader.read_exact(&mut bytes[..2]).unwrap();
            sender.send(bytes[..2].to_vec()).unwrap();
            assert_eq!(reader.byte_len(), Some(6));
        });
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            b"abc"
        );
        assert!(matches!(
            receiver.recv_timeout(Duration::from_millis(100)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        connection.write_all(b"def").unwrap();
        connection.shutdown(Shutdown::Write).unwrap();
        assert_eq!(
            receiver.recv_timeout(Duration::from_secs(2)).unwrap(),
            b"ef"
        );
        worker.join().unwrap();
    }

    #[test]
    fn cancelling_a_waiting_decoder_keeps_the_download_for_its_replacement() {
        let (source, mut connection) = stream(Some(10));
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut reader = source.reader(cancelled.clone()).unwrap();
        let (sender, receiver) = mpsc::channel();
        let worker = thread::spawn(move || sender.send(reader.read(&mut [0; 1])).unwrap());
        cancelled.store(true, Ordering::SeqCst);
        assert_eq!(
            receiver
                .recv_timeout(Duration::from_secs(1))
                .unwrap()
                .unwrap_err()
                .kind(),
            io::ErrorKind::ConnectionAborted
        );
        worker.join().unwrap();
        assert!(!source.0.shared.cancelled.load(Ordering::SeqCst));
        connection.write_all(b"abcdefghij").unwrap();
        let mut replacement = source.reader(Arc::default()).unwrap();
        let mut all = Vec::new();
        replacement.read_to_end(&mut all).unwrap();
        assert_eq!(all, b"abcdefghij");
    }

    #[test]
    fn incomplete_downloads_fail_instead_of_reporting_end_of_track() {
        let (source, mut connection) = stream(Some(10));
        let mut reader = source.reader(Arc::default()).unwrap();
        connection.write_all(b"abc").unwrap();
        connection.shutdown(Shutdown::Write).unwrap();
        assert!(reader.read_to_end(&mut Vec::new()).is_err());
        completed(&source);
        assert!(source.failed());
    }

    #[test]
    fn last_owner_cancels_download_and_authentication_failures_keep_their_type() {
        let (source, mut connection) = stream(Some(10));
        let shared = source.0.shared.clone();
        drop(source);
        assert!(shared.cancelled.load(Ordering::SeqCst));
        let _ = connection.write_all(b"abcdefghij");
        connection.shutdown(Shutdown::Write).unwrap();

        let (source, mut connection, _directory) = connect();
        write!(
            connection,
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        assert!(matches!(
            source.reader(Arc::default()),
            Err(Error::Unauthorized)
        ));
        assert!(source.failed());
    }
}
