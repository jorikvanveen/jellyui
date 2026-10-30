use super::{AudioStream, Client, authorization, parse_audio_range, request_error};
use crate::domain::Error;
use std::time::Duration;

impl Client {
    #[tracing::instrument(name = "jellyfin.audio", skip_all)]
    pub fn audio(&self, item_id: &str) -> Result<ureq::Body, Error> {
        self.audio_request(item_id, None).map(|stream| stream.body)
    }

    #[tracing::instrument(name = "jellyfin.audio_range", skip(self, item_id))]
    pub fn audio_range(
        &self,
        item_id: &str,
        start: u64,
        end: Option<u64>,
    ) -> Result<AudioStream, Error> {
        self.audio_request(item_id, Some((start, end)))
    }

    pub(super) fn audio_request(
        &self,
        item_id: &str,
        range: Option<(u64, Option<u64>)>,
    ) -> Result<AudioStream, Error> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated HTTP URL")
            .pop_if_empty()
            .extend(["Audio", item_id, "stream"]);
        url.query_pairs_mut().append_pair("static", "true");
        tracing::debug!("Requesting original audio file");
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .max_redirects(0)
            .timeout_connect(Some(Duration::from_secs(10)))
            .timeout_recv_response(Some(Duration::from_secs(10)))
            .timeout_per_read(Some(Duration::from_secs(10)))
            .build()
            .into();
        let mut request = agent
            .get(url.as_str())
            .header("Accept-Encoding", "identity")
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            );
        if let Some((start, end)) = range {
            request = request.header(
                "Range",
                format!(
                    "bytes={start}-{}",
                    end.map(|end| end.to_string()).unwrap_or_default()
                ),
            );
        }
        let mut response = request.call().map_err(request_error)?;
        if response.status().is_redirection() {
            return Err(Error::Message(
                "Jellyfin redirected the audio request. Use the final server URL.",
            ));
        }
        tracing::debug!(
            status = response.status().as_u16(),
            "Original audio stream opened"
        );
        if response
            .headers()
            .get("Content-Encoding")
            .is_some_and(|value| value != "identity")
        {
            return Err(Error::Message(
                "Jellyfin compressed the audio stream; the original file is required for playback.",
            ));
        }
        let (offset, length, end) = match (response.status().as_u16(), range) {
            (200, _) => {
                let length = response.body_mut().content_length();
                (0, length, length)
            }
            (206, Some((start, requested_end))) => {
                let value = response
                    .headers()
                    .get("Content-Range")
                    .and_then(|value| value.to_str().ok());
                let (offset, last, length) = parse_audio_range(value)
                    .ok_or(Error::Message("Jellyfin returned an invalid audio range."))?;
                if offset != start
                    || requested_end.is_some_and(|end| last > end)
                    || (requested_end.is_none() && last != length - 1)
                    || response
                        .body_mut()
                        .content_length()
                        .is_some_and(|size| size != last - offset + 1)
                {
                    return Err(Error::Message(
                        "Jellyfin returned an unexpected audio range.",
                    ));
                }
                (offset, Some(length), Some(last + 1))
            }
            _ => {
                return Err(Error::Message(
                    "Jellyfin returned an unexpected partial audio response.",
                ));
            }
        };
        Ok(AudioStream {
            body: response.into_body(),
            offset,
            length,
            end,
        })
    }
}
