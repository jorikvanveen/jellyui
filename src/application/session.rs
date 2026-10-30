use crate::infrastructure::jellyfin::{self, Client, Session as SavedSession};
use gpui::{Context, EventEmitter};
const CREDENTIAL_KEY: &str = "jellyui://session";

pub struct SignedIn {
    pub client: Client,
    pub warning: Option<String>,
}

/// Authentication and keyring lifecycle, independent of login form widgets.
pub struct Session {
    busy: bool,
    error: Option<String>,
    warning: Option<String>,
}
impl EventEmitter<SignedIn> for Session {}
impl Session {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let mut session = Self {
            busy: true,
            error: None,
            warning: None,
        };
        session.restore(cx);
        session
    }
    pub fn busy(&self) -> bool {
        self.busy
    }
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }
    pub fn warning(&self) -> Option<&str> {
        self.warning.as_deref()
    }
    pub fn reject_session(&mut self, error: String, cx: &mut Context<Self>) {
        self.warning = None;
        self.error = Some(error);
        self.busy = true;
        let deletion = cx.delete_credentials(CREDENTIAL_KEY);
        cx.spawn(async move |this, cx| {
            let deleted = deletion.await;
            let _ = this.update(cx, |page, cx| {
                page.busy = false;
                if let Err(error) = deleted {
                    tracing::warn!(error = %error, "Could not delete rejected login from desktop credential storage");
                    page.warning = Some("Could not remove the rejected login from desktop credential storage.".into());
                }
                cx.notify();
            });
        }).detach();
        cx.notify();
    }

    fn restore(&mut self, cx: &mut Context<Self>) {
        tracing::info!("Reading saved login from desktop credential storage");
        let credentials = cx.read_credentials(CREDENTIAL_KEY);
        cx.spawn(async move |this, cx| {
            let result = credentials.await;
            let _ =
                this.update(cx, |view, cx| {
                    view.busy = false;
                    match result {
                        Ok(Some((_, secret))) => {
                            match serde_json::from_slice::<SavedSession>(&secret)
                                .inspect_err(|error| tracing::warn!(category = ?error.classify(), line = error.line(), column = error.column(), "Saved login has invalid JSON structure"))
                                .ok()
                                .and_then(|session| Client::restore(session).inspect_err(|error| tracing::warn!(error = %error, "Saved login has an invalid server URL")).ok())
                            {
                                Some(client) => {
                                    tracing::info!("Saved login restored; loading albums");
                                    cx.emit(SignedIn { client, warning: None });
                                }
                                None => {
                                    view.warning = Some(
                                        "The saved login could not be read. Sign in to replace it."
                                            .into(),
                                    )
                                }
                            }
                        }
                        Ok(None) => { tracing::info!("No saved login found"); }
                        Err(error) => {
                            tracing::warn!(error = %error, "Could not read desktop credential storage");
                            view.warning = Some(
                            "Desktop credential storage is unavailable. Sign in for this session."
                                .into(),
                            );
                        }
                    }
                    cx.notify();
                });
        })
        .detach();
    }

    pub fn sign_in(
        &mut self,
        server: String,
        username: String,
        password: String,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.busy {
            tracing::debug!(
                busy = self.busy,
                "Sign-in ignored while another login operation is active"
            );
            return false;
        }
        if let Err(error) = jellyfin::server_url(&server) {
            tracing::warn!(error = %error, "Sign-in rejected by server URL validation");
            self.error = Some(error.to_string());
            cx.notify();
            return false;
        }
        let username = username.trim().to_owned();
        if username.is_empty() {
            tracing::warn!("Sign-in requires a username");
            self.error = Some("Enter your Jellyfin username.".into());
            cx.notify();
            return false;
        }
        // Empty passwords are allowed by some Jellyfin servers.
        self.busy = true;
        self.error = None;
        tracing::info!("Sign-in submitted");
        let request = cx
            .background_executor()
            .spawn(async move { Client::sign_in(&server, &username, &password) });
        cx.spawn(async move |this, cx| {
            match request.await {
                Ok(client) => {
                    tracing::info!("Saving Jellyfin session in desktop credential storage");
                    let secret = serde_json::to_vec(&client.session).expect("session serialization");
                        let save = cx.update(|cx| cx.write_credentials(CREDENTIAL_KEY, "session", &secret));
                    let saved = save.await;
                    match &saved {
                        Ok(()) => tracing::info!("Login saved to desktop credential storage"),
                        Err(error) => tracing::warn!(error = %error, "Could not save login; continuing for this session"),
                    }
                    let _ = this.update(cx, |view, cx| {
                        let warning = saved.err().map(|_| "Signed in, but the desktop keyring could not save your login. You will need to sign in next time.".into());
                        view.busy = false;
                        cx.emit(SignedIn { client, warning });
                        cx.notify();
                    });
                }
                Err(error) => {
                    tracing::error!(error = %error, "Sign-in failed");
                    let _ = this.update(cx, |view, cx| {
                        view.busy = false;
                        view.error = Some(error.to_string());
                        cx.notify();
                    });
                }
            }
        }).detach();
        cx.notify();
        true
    }
}
