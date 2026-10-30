use std::io::{self, IsTerminal};

use tracing_subscriber::{EnvFilter, fmt::format::FmtSpan};

pub fn init() {
    let filter = match EnvFilter::try_from_default_env() {
        Ok(filter) => filter,
        Err(error) => {
            if std::env::var_os("RUST_LOG").is_some() {
                eprintln!("Invalid RUST_LOG filter: {error}. Using warn,jellyui=info.");
            }
            EnvFilter::new("warn,jellyui=info")
        }
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .with_ansi(io::stderr().is_terminal())
        .with_span_events(FmtSpan::CLOSE)
        .init();
}
