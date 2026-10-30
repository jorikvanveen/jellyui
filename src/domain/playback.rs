#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Loading,
    Playing,
    Paused,
    Finished,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AlbumAction {
    Play,
    Queue,
}
