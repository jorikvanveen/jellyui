use super::{Client, Session, agent, authorization, read_response, request_error, server_url};
use crate::domain::Error;
use crate::infrastructure::cache::covers::CoverCache;
use serde::Deserialize;
use std::{sync::Arc, time::Instant};

impl Client {
    pub fn restore(session: Session) -> Result<Self, Error> {
        let base = server_url(&session.server)?;
        Ok(Self {
            session,
            agent: agent(),
            base,
            cover_cache: CoverCache::system().map(Arc::new),
        })
    }

    #[tracing::instrument(name = "jellyfin.sign_in", skip_all)]
    pub fn sign_in(server: &str, username: &str, password: &str) -> Result<Self, Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct Authentication {
            access_token: String,
            user: User,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "PascalCase")]
        struct User {
            id: String,
            name: String,
        }

        let base = server_url(server)?;
        tracing::info!(server = %base, "Authenticating with Jellyfin");
        let agent = agent();
        let device_id = uuid::Uuid::new_v4().to_string();
        let started = Instant::now();
        let response = agent
            .post(base.join("Users/AuthenticateByName").unwrap().as_str())
            .header("Authorization", authorization(&device_id, None)?)
            .send_json(serde_json::json!({ "Username": username, "Pw": password }))
            .map_err(request_error)?;
        let auth: Authentication = read_response(
            response,
            started,
            "Jellyfin returned an invalid sign-in response.",
        )?;
        if auth.access_token.is_empty() || auth.user.id.is_empty() {
            tracing::error!("Jellyfin sign-in response is missing an access token or user ID");
            return Err(Error::Message(
                "Jellyfin returned an incomplete sign-in response.",
            ));
        }
        tracing::info!(
            elapsed_ms = started.elapsed().as_millis(),
            "Jellyfin authentication succeeded"
        );
        Ok(Self {
            session: Session {
                server: base.to_string(),
                user_id: auth.user.id,
                username: auth.user.name,
                token: auth.access_token,
                device_id,
            },
            agent,
            base,
            cover_cache: CoverCache::system().map(Arc::new),
        })
    }
}
