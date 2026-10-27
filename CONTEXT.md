# Jellyui music library

Jellyui is a music player for a library managed by Jellyfin.

## Language

**Jellyfin server**:
The Jellyfin instance that hosts the listener's music library.
_Avoid_: Backend, cloud

**Library**:
The music collection available to the signed-in listener on their Jellyfin server. It is not a separate collection imported or managed by Jellyui.
_Avoid_: Local library, catalog

**Original file**:
The audio file held by Jellyfin, streamed without server-side transcoding and decoded by Jellyui.
_Avoid_: Transcoded stream

**Best-effort bit-perfect playback**:
Playback that aims to preserve the original decoded samples and source rate while allowing conversion when shared audio output requires it. It is a playback policy, not a guarantee that the device receives identical samples.
_Avoid_: Guaranteed bit-perfect output

**Album playback**:
An ordered run of tracks from one album, beginning at its first track or a selected track and ending at the album's final track. Starting another album replaces the current run.
_Avoid_: Playlist
