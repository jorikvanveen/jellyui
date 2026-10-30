use std::{
    ffi::OsStr,
    fs::{self, File, FileTimes, OpenOptions},
    io::{self, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, SystemTime},
};

use sha2::{Digest, Sha256};

pub const MAX_BYTES: u64 = 1_000_000_000;
pub const MAX_COVER_BYTES: u64 = 8 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Persistent encoded covers. Modification time records freshness; access time
/// records LRU order. File locks serialize quota checks across app instances.
pub struct CoverCache {
    root: PathBuf,
    limit: u64,
    initialized: AtomicBool,
}

impl CoverCache {
    pub fn system() -> Option<Self> {
        let xdg = std::env::var_os("XDG_CACHE_HOME");
        let home = std::env::var_os("HOME");
        cache_directory(xdg.as_deref(), home.as_deref()).map(|root| Self::new(root, MAX_BYTES))
    }

    fn new(root: PathBuf, limit: u64) -> Self {
        Self {
            root,
            limit,
            initialized: AtomicBool::new(false),
        }
    }

    pub fn key(server: &str, user: &str, item: &str) -> String {
        let mut hash = Sha256::new();
        for value in ["jellyui-cover-v1-640-png", server, user, item] {
            hash.update((value.len() as u64).to_le_bytes());
            hash.update(value.as_bytes());
        }
        format!("{:x}", hash.finalize())
    }

    pub fn get(&self, key: &str) -> io::Result<Option<Vec<u8>>> {
        let _lock = self.lock()?;
        let path = self.root.join(format!("{key}.png"));
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let metadata = file.metadata()?;
        let stale = SystemTime::now()
            .duration_since(metadata.modified()?)
            .unwrap_or_default()
            >= MAX_AGE;
        if metadata.len() == 0 || metadata.len() > MAX_COVER_BYTES || stale {
            fs::remove_file(path)?;
            return Ok(None);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        Read::by_ref(&mut file)
            .take(MAX_COVER_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.is_empty() || bytes.len() as u64 > MAX_COVER_BYTES {
            fs::remove_file(path)?;
            return Ok(None);
        }
        file.set_times(FileTimes::new().set_accessed(SystemTime::now()))?;
        Ok(Some(bytes))
    }

    pub fn put(&self, key: &str, bytes: &[u8]) -> io::Result<()> {
        let size = bytes.len() as u64;
        if size == 0 || size > self.limit || size > MAX_COVER_BYTES {
            return Ok(());
        }
        let _lock = self.lock()?;
        // Reserve the staging file's size before writing, so even a crash during
        // a download cannot grow this cache beyond its byte budget.
        self.prune(size)?;
        let temporary = self.root.join(format!("{}.part", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(bytes)?;
            file.sync_all()?;
            fs::rename(&temporary, self.root.join(format!("{key}.png")))
        })();
        if result.is_err() {
            let _ = fs::remove_file(temporary);
        }
        result
    }

    fn lock(&self) -> io::Result<File> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(self.root.join(".lock"))?;
        file.lock()?;
        if !self.initialized.load(Ordering::Acquire) {
            self.prune(0)?;
            self.initialized.store(true, Ordering::Release);
        }
        Ok(file)
    }

    fn prune(&self, reserve: u64) -> io::Result<()> {
        let mut entries = Vec::new();
        let mut total = 0u64;
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            let path = entry.path();
            if path.extension() == Some(OsStr::new("part")) {
                // All writes hold the lock, so these can only be interrupted writes.
                fs::remove_file(path)?;
                continue;
            }
            if path.extension() != Some(OsStr::new("png")) {
                continue;
            }
            let metadata = entry.metadata()?;
            total = total.saturating_add(metadata.len());
            let accessed = metadata.accessed().or_else(|_| metadata.modified())?;
            entries.push((accessed, path, metadata.len()));
        }
        let budget = self.limit.saturating_sub(reserve);
        if total <= budget {
            return Ok(());
        }
        entries.sort_unstable();
        for (_, path, size) in entries {
            fs::remove_file(path)?;
            total = total.saturating_sub(size);
            if total <= budget {
                break;
            }
        }
        Ok(())
    }
}

fn cache_directory(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    xdg.map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|path| path.join(".cache"))
        })
        .map(|path| path.join("jellyui/covers"))
}

#[cfg(test)]
pub(crate) struct TestDirectory(pub PathBuf);

#[cfg(test)]
impl TestDirectory {
    pub(crate) fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("jellyui-cover-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&root).unwrap();
        Self(root)
    }

    pub(crate) fn cache(&self, limit: u64) -> CoverCache {
        CoverCache::new(self.0.clone(), limit)
    }
}

#[cfg(test)]
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{path::Path, sync::Arc, thread};

    fn stored_bytes(root: &Path) -> u64 {
        fs::read_dir(root)
            .unwrap()
            .map(|entry| entry.unwrap().metadata().unwrap().len())
            .sum()
    }

    #[test]
    fn keys_separate_servers_users_and_items() {
        let key = CoverCache::key("https://server/", "user", "album");
        for (server, user, item) in [
            ("https://other/", "user", "album"),
            ("https://server/", "other", "album"),
            ("https://server/", "user", "other"),
        ] {
            assert_ne!(key, CoverCache::key(server, user, item));
        }
    }

    #[test]
    fn persists_across_cache_instances_and_uses_private_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let directory = TestDirectory::new();
        directory.cache(100).put("cover", b"image bytes").unwrap();
        assert_eq!(
            directory.cache(100).get("cover").unwrap().unwrap(),
            b"image bytes"
        );
        assert_eq!(
            fs::metadata(directory.0.join("cover.png"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn evicts_least_recently_used_entries_to_stay_within_budget() {
        let directory = TestDirectory::new();
        let cache = directory.cache(8);
        cache.put("first", b"1111").unwrap();
        cache.put("second", b"2222").unwrap();
        File::open(directory.0.join("second.png"))
            .unwrap()
            .set_times(FileTimes::new().set_accessed(SystemTime::UNIX_EPOCH))
            .unwrap();
        assert!(cache.get("first").unwrap().is_some());
        cache.put("third", b"3333").unwrap();
        assert!(cache.get("second").unwrap().is_none());
        assert!(cache.get("first").unwrap().is_some());
        assert!(cache.get("third").unwrap().is_some());
        assert!(stored_bytes(&directory.0) <= 8);
    }

    #[test]
    fn startup_prunes_existing_files_and_interrupted_writes() {
        let directory = TestDirectory::new();
        let old = directory.cache(12);
        for key in ["one", "two", "three"] {
            old.put(key, b"1234").unwrap();
        }
        fs::write(directory.0.join("interrupted.part"), b"partial").unwrap();
        let smaller = directory.cache(8);
        smaller.get("missing").unwrap();
        assert!(!directory.0.join("interrupted.part").exists());
        assert!(stored_bytes(&directory.0) <= 8);
    }

    #[test]
    fn expiration_uses_fetch_time_rather_than_last_access() {
        let directory = TestDirectory::new();
        let cache = directory.cache(100);
        cache.put("old", b"bytes").unwrap();
        File::open(directory.0.join("old.png"))
            .unwrap()
            .set_times(
                FileTimes::new()
                    .set_modified(SystemTime::now() - MAX_AGE - Duration::from_secs(1))
                    .set_accessed(SystemTime::now()),
            )
            .unwrap();
        assert!(cache.get("old").unwrap().is_none());
        assert!(!directory.0.join("old.png").exists());
    }

    #[test]
    fn oversized_and_empty_entries_do_not_evict_valid_covers() {
        let directory = TestDirectory::new();
        let cache = directory.cache(4);
        cache.put("small", b"1234").unwrap();
        cache.put("large", b"12345").unwrap();
        cache.put("empty", b"").unwrap();
        assert_eq!(cache.get("small").unwrap().unwrap(), b"1234");
        assert!(cache.get("large").unwrap().is_none());
        assert!(cache.get("empty").unwrap().is_none());
    }

    #[test]
    fn concurrent_cache_instances_share_the_budget_without_partial_files() {
        let directory = TestDirectory::new();
        let path = Arc::new(directory.0.clone());
        let workers: Vec<_> = (0..8)
            .map(|index| {
                let path = path.clone();
                thread::spawn(move || {
                    CoverCache::new((*path).clone(), 16)
                        .put(&index.to_string(), b"1234")
                        .unwrap();
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(stored_bytes(&directory.0) <= 16);
        for entry in fs::read_dir(&directory.0).unwrap() {
            let entry = entry.unwrap();
            assert_ne!(entry.path().extension(), Some(OsStr::new("part")));
            if entry.path().extension() == Some(OsStr::new("png")) {
                assert_eq!(fs::read(entry.path()).unwrap(), b"1234");
            }
        }
    }
}
