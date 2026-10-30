use super::{Client, authorization, request_error};
use crate::domain::Error;
use serde::Serialize;
use std::time::Duration;

impl Client {
    pub fn report_playback(&self, report: &Report) -> Result<(), Error> {
        let url = self.base.join(report.kind.path()).unwrap();
        let response = self
            .agent
            .post(url.as_str())
            .config()
            .timeout_global(Some(Duration::from_secs(5)))
            .build()
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            )
            .send_json(report)
            .map_err(request_error)?;
        if !response.status().is_success() {
            return Err(Error::Message(
                "Jellyfin redirected the playback report. Use the final server URL.",
            ));
        }
        tracing::debug!(kind = ?report.kind, status = response.status().as_u16(), "Playback reported to Jellyfin");
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Start,
    Progress,
    Stop,
}

impl Kind {
    pub fn path(self) -> &'static str {
        match self {
            Self::Start => "Sessions/Playing",
            Self::Progress => "Sessions/Playing/Progress",
            Self::Stop => "Sessions/Playing/Stopped",
        }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct Report {
    #[serde(skip)]
    pub kind: Kind,
    pub(crate) item_id: String,
    pub(crate) play_session_id: String,
    pub(crate) position_ticks: u64,
    pub(crate) can_seek: bool,
    pub(crate) is_paused: bool,
    pub(crate) is_muted: bool,
    pub(crate) volume_level: u8,
    pub(crate) play_method: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) now_playing_queue: Option<Vec<QueueItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed: Option<bool>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct QueueItem {
    pub(crate) id: String,
}
