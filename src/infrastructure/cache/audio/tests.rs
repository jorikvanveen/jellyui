use super::*;
use std::{
    io::{BufRead, BufReader},
    net::{Shutdown, TcpListener, TcpStream},
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
fn authentication_failures_keep_their_type() {
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
