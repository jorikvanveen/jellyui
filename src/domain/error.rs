use std::fmt;

#[derive(Clone, Debug)]
pub enum Error {
    Unauthorized,
    Message(&'static str),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "Jellyfin rejected the login. Please sign in again.",
            Self::Message(message) => message,
        })
    }
}

impl std::error::Error for Error {}
