mod app;
mod application;
mod audio;
mod domain;
mod infrastructure;
mod logging;
mod ui;

const APP_ID: &str = "jellyui";

fn main() {
    app::run();
}
