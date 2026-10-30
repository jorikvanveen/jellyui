//! Library metadata and playback rules, independent of GPUI, HTTP and PipeWire.
mod catalog;
mod error;
pub mod pages;
pub mod queue;
pub mod search;

pub use catalog::{
    AlbumQuery, AlbumSort, Artist, Item, LibraryKind, PAGE_SIZE, Page, artist_credits,
};
pub use error::Error;
pub mod playback;
