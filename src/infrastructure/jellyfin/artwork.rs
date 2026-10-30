use super::{Client, authorization, request_error};
use crate::domain::Error;
use crate::infrastructure::cache::covers::{CoverCache, MAX_COVER_BYTES};

impl Client {
    #[tracing::instrument(level = "debug", name = "jellyfin.cover", skip_all)]
    pub fn cover(&self, item_id: &str) -> Result<Option<Vec<u8>>, Error> {
        let cache_key = CoverCache::key(self.base.as_str(), &self.session.user_id, item_id);
        if let Some(cache) = &self.cover_cache {
            match cache.get(&cache_key) {
                Ok(Some(bytes)) => {
                    tracing::debug!("Cover disk cache hit");
                    return Ok(Some(bytes));
                }
                Ok(None) => tracing::debug!("Cover disk cache miss"),
                Err(error) => {
                    tracing::warn!(%error, "Could not read cover disk cache; fetching artwork")
                }
            }
        }
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("validated HTTP URL")
            .pop_if_empty()
            .extend(["Items", item_id, "Images", "Primary"]);
        url.query_pairs_mut().extend_pairs([
            ("maxWidth", "640"),
            ("maxHeight", "640"),
            ("format", "Png"),
        ]);
        let mut response = match self
            .agent
            .get(url.as_str())
            .header(
                "Authorization",
                authorization(&self.session.device_id, Some(&self.session.token))?,
            )
            .call()
        {
            Ok(response) => response,
            Err(ureq::Error::StatusCode(404)) => return Ok(None),
            Err(error) => return Err(request_error(error)),
        };
        if response.status().is_redirection() {
            return Err(Error::Message(
                "Jellyfin redirected the album artwork request.",
            ));
        }
        let bytes = response
            .body_mut()
            .with_config()
            .limit(MAX_COVER_BYTES)
            .read_to_vec()
            .map_err(request_error)?;
        if let Some(cache) = &self.cover_cache
            && let Err(error) = cache.put(&cache_key, &bytes)
        {
            tracing::warn!(%error, "Could not persist album artwork in disk cache");
        }
        Ok(Some(bytes))
    }
}
