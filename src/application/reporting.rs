use std::{
    collections::VecDeque,
    io,
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use crate::infrastructure::jellyfin::reporting::{Kind, QueueItem, Report};

use crate::infrastructure::jellyfin::Client;

const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
pub const QUEUE_WINDOW: usize = 20;

#[derive(Clone)]
pub struct Snapshot {
    pub item_id: String,
    pub position: Duration,
    pub paused: bool,
    pub volume: f32,
    pub can_seek: bool,
    pub queue: Vec<String>,
}

struct Active {
    client: Client,
    snapshot: Snapshot,
    session_id: String,
    last_report: Instant,
}

impl Active {
    fn report(&self, kind: Kind, include_queue: bool) -> Job {
        let snapshot = &self.snapshot;
        Job {
            client: self.client.clone(),
            report: Report {
                kind,
                item_id: snapshot.item_id.clone(),
                play_session_id: self.session_id.clone(),
                position_ticks: (snapshot.position.as_nanos() / 100).min(i64::MAX as u128) as u64,
                can_seek: snapshot.can_seek,
                is_paused: snapshot.paused,
                is_muted: snapshot.volume == 0.,
                volume_level: (snapshot.volume.clamp(0., 1.) * 100.).round() as u8,
                play_method: "DirectPlay",
                now_playing_queue: include_queue.then(|| {
                    snapshot
                        .queue
                        .iter()
                        .take(QUEUE_WINDOW)
                        .cloned()
                        .map(|id| QueueItem { id })
                        .collect()
                }),
                failed: None,
            },
        }
    }
}

struct Job {
    client: Client,
    report: Report,
}

#[derive(Default)]
struct State {
    active: Option<Active>,
    jobs: VecDeque<Job>,
    closing: bool,
    closed: bool,
}

impl State {
    fn enqueue(&mut self, job: Job) {
        // Collapse pending positions/volume changes, but retain pause transitions
        // and every start/stop boundary in their original order.
        if job.report.kind == Kind::Progress
            && let Some(last) = self.jobs.back_mut()
            && last.report.kind == Kind::Progress
            && last.report.play_session_id == job.report.play_session_id
            && last.report.is_paused == job.report.is_paused
        {
            *last = job;
        } else {
            self.jobs.push_back(job);
        }
    }

    fn update(&mut self, client: Client, snapshot: Snapshot, force: bool, now: Instant) {
        if self.closing {
            return;
        }
        if self
            .active
            .as_ref()
            .is_some_and(|active| active.snapshot.item_id != snapshot.item_id)
        {
            self.stop(None, false);
        }
        let job = if let Some(active) = &mut self.active {
            active.snapshot = snapshot;
            if force || now.saturating_duration_since(active.last_report) >= PROGRESS_INTERVAL {
                active.last_report = now;
                Some(active.report(Kind::Progress, force))
            } else {
                None
            }
        } else {
            let active = Active {
                client,
                snapshot,
                session_id: uuid::Uuid::new_v4().simple().to_string(),
                last_report: now,
            };
            let job = active.report(Kind::Start, true);
            self.active = Some(active);
            Some(job)
        };
        if let Some(job) = job {
            self.enqueue(job);
        }
    }

    fn stop(&mut self, position: Option<Duration>, failed: bool) {
        if let Some(mut active) = self.active.take() {
            if let Some(position) = position {
                active.snapshot.position = position;
            }
            let mut job = active.report(Kind::Stop, false);
            job.report.failed = Some(failed);
            self.enqueue(job);
        }
    }
}

#[derive(Default)]
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

/// One serial HTTP worker for the app. Audio-worker replacement during seeking
/// does not replace the logical listening session or issue another start report.
#[derive(Clone)]
pub struct Reporter(Arc<Shared>);

impl Reporter {
    pub fn new() -> io::Result<Self> {
        let shared = Arc::new(Shared::default());
        let worker = shared.clone();
        thread::Builder::new().name("playback-reports".into()).spawn(move || {
            loop {
                let job = {
                    let mut state = worker.state.lock().unwrap();
                    loop {
                        if let Some(job) = state.jobs.pop_front() {
                            break job;
                        }
                        if state.closing {
                            state.closed = true;
                            worker.changed.notify_all();
                            return;
                        }
                        state = worker.changed.wait(state).unwrap();
                    }
                };
                if let Err(error) = job.client.report_playback(&job.report) {
                    tracing::warn!(kind = ?job.report.kind, %error, "Could not report playback to Jellyfin");
                }
            }
        })?;
        Ok(Self(shared))
    }

    pub fn is_active(&self) -> bool {
        self.0.state.lock().unwrap().active.is_some()
    }

    pub fn update(&self, client: Client, snapshot: Snapshot, force: bool) {
        self.0
            .state
            .lock()
            .unwrap()
            .update(client, snapshot, force, Instant::now());
        self.0.changed.notify_one();
    }

    pub fn stop(&self, position: Option<Duration>, failed: bool) {
        self.0.state.lock().unwrap().stop(position, failed);
        self.0.changed.notify_one();
    }

    /// Called by the app's quit handler on a background executor. GPUI bounds
    /// how long shutdown may wait; network reporting remains best-effort.
    pub fn shutdown(&self) {
        let mut state = self.0.state.lock().unwrap();
        state.stop(None, false);
        state.closing = true;
        self.0.changed.notify_one();
        while !state.closed {
            state = self.0.changed.wait(state).unwrap();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client() -> Client {
        Client::restore(
            serde_json::from_value(serde_json::json!({
                "server":"http://localhost/", "user_id":"user", "username":"Listener",
                "token":"test-token", "device_id":"device"
            }))
            .unwrap(),
        )
        .unwrap()
    }

    fn snapshot(id: &str, seconds: u64) -> Snapshot {
        Snapshot {
            item_id: id.into(),
            position: Duration::from_secs(seconds),
            paused: false,
            volume: 1.,
            can_seek: true,
            queue: (0..100).map(|n| n.to_string()).collect(),
        }
    }

    #[test]
    fn progress_is_periodic_and_seek_updates_keep_one_logical_play() {
        let mut state = State::default();
        let now = Instant::now();
        state.update(client(), snapshot("a", 1), false, now);
        let start = state.jobs.pop_front().unwrap().report;
        assert_eq!(start.kind, Kind::Start);
        assert_eq!(
            start.now_playing_queue.as_ref().unwrap().len(),
            QUEUE_WINDOW
        );
        state.update(
            client(),
            snapshot("a", 2),
            false,
            now + Duration::from_secs(1),
        );
        assert!(state.jobs.is_empty());
        state.update(client(), snapshot("a", 11), false, now + PROGRESS_INTERVAL);
        let progress = state.jobs.pop_front().unwrap().report;
        assert_eq!(progress.kind, Kind::Progress);
        assert!(progress.now_playing_queue.is_none());
        assert_eq!(progress.position_ticks, 110_000_000);
        state.update(
            client(),
            snapshot("a", 90),
            true,
            now + Duration::from_secs(11),
        );
        state.update(
            client(),
            snapshot("a", 5),
            true,
            now + Duration::from_secs(12),
        );
        assert_eq!(state.jobs.len(), 1);
        let seek = state.jobs.pop_front().unwrap().report;
        assert_eq!(seek.kind, Kind::Progress);
        assert_eq!(seek.play_session_id, start.play_session_id);
        assert_eq!(seek.position_ticks, 50_000_000);
        state.stop(Some(Duration::from_secs(6)), false);
        let stop = state.jobs.pop_front().unwrap().report;
        assert_eq!(stop.kind, Kind::Stop);
        assert_eq!(stop.play_session_id, start.play_session_id);
        assert_eq!(stop.position_ticks, 60_000_000);
        assert_eq!(stop.failed, Some(false));
    }

    #[test]
    fn track_changes_stop_the_old_identity_and_failure_does_not_double_stop() {
        let mut state = State::default();
        let now = Instant::now();
        state.update(client(), snapshot("old", 1), false, now);
        state.update(client(), snapshot("old", 2), false, now);
        state.update(client(), snapshot("new", 0), false, now);
        assert_eq!(state.jobs[1].report.kind, Kind::Stop);
        assert_eq!(state.jobs[1].report.item_id, "old");
        assert_eq!(state.jobs[1].report.position_ticks, 20_000_000);
        assert_eq!(state.jobs[2].report.item_id, "new");
        state.stop(Some(Duration::from_secs(3)), true);
        state.stop(None, false);
        assert_eq!(state.jobs.len(), 4);
        assert_eq!(state.jobs[3].report.failed, Some(true));
        state.closing = true;
        state.update(client(), snapshot("ignored", 0), true, now);
        assert_eq!(state.jobs.len(), 4);
    }

    #[test]
    fn stop_before_any_play_is_silent_and_serialized_ticks_fit_jellyfin() {
        let mut state = State::default();
        state.stop(None, false);
        assert!(state.jobs.is_empty());
        let mut info = snapshot("a", 0);
        info.position = Duration::MAX;
        state.update(client(), info, false, Instant::now());
        let body = serde_json::to_value(&state.jobs[0].report).unwrap();
        assert_eq!(body["PositionTicks"], i64::MAX);
        assert_eq!(body["ItemId"], "a");
        assert_eq!(body["PlayMethod"], "DirectPlay");
        assert_eq!(body["CanSeek"], true);
        assert_eq!(body["VolumeLevel"], 100);
        assert!(body.get("Kind").is_none());
        assert!(body.get("Failed").is_none());
    }
}
