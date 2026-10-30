# Jellyui

A Linux GPUI music player for Jellyfin. Sign in, remember one login through the desktop keyring, browse albums and artists, and play original FLAC and MP3 files through native PipeWire.

## Run

```sh
nix develop -c cargo run
```

Run this from a Linux graphical session with a Vulkan-capable graphics driver. Both Wayland and X11 are enabled. Remembered login requires a working desktop Secret Service keyring, such as GNOME Keyring or KWallet's Secret Service integration. If the keyring is unavailable, you can still sign in for the current session.

Enter your Jellyfin server's full URL, including its base path if it has one, then your username and password. Use the final server URL, since sign-in and authenticated requests do not follow redirects. Jellyui stores the access token in the keyring, never the password. Closing the window retains the login.

Use the slim icon sidebar on the left to switch between Albums and Artists. The selected icon is highlighted; hovering shows its label. Switching keeps each overview's search, sorting and scroll position, while playback and the queue remain available.

The album overview is a cover grid with titles, artists and release years. Search applies automatically after 200 ms without text changes. The filter sidebar has a dropdown for sorting by title, artist, release year or date added, and an arrow button to toggle ascending/descending order. The dropdown supports arrow keys, Enter and Escape. Reset restores the defaults. Search and sorting apply across the server library. Scrolling automatically fetches visible and nearby pages, and the scrollbar represents the full filtered result count. Dragging to an unloaded region fetches that region directly. Metadata and artwork caches are bounded, and covers load as they come into view.

Hover over an album to dim its cover and reveal Play and Queue buttons. Play replaces the queue and starts the album; Queue appends it without interrupting playback. Both work directly from the grid, including an artist's albums. Clicking elsewhere on a card opens the album. The buttons are also available through keyboard focus.

The artists overview shows portraits and names, with debounced search and ascending/descending name order. It includes credited performers and album artists. Open an artist to browse their albums, including guest appearances, then open an album to play or queue it. Within an artist's albums, search filters album titles and the album sorting options remain available. Back links return through the artist's albums to the artists overview. Artists use the same paged grid, full-library scrollbar and lazy artwork loading as albums.

Search matches album titles and credited artists, including performer and album-artist credits. Words can span the title and credits in any order, so `Abbey Road Beatles` works. All words must match. Title matches rank first, followed by mixed title/artist matches, then artist-only matches. The selected sort and direction apply within each group. Albums matching both appear once. Searches retain an ordered list of result IDs; full album details and artwork still load as you scroll.

Open an album to see its artwork, metadata, and tracks in disc and track order. Returning to the overview preserves its filters and scroll position. Use Tab and Shift+Tab to move between fields and buttons, and Ctrl+Q to quit. Connection errors have a Retry action; a rejected token returns to sign-in.

Artist names in the album header are individual links to each artist's albums, with hover and keyboard-focus styling. Opening an artist keeps the current playback and queue in place.

Covers and artist portraits persist in `$XDG_CACHE_HOME/jellyui/covers`, or `~/.cache/jellyui/covers` when that variable is unset. The shared disk cache holds up to 1 GB and evicts least-recently-used images. Entries expire 24 hours after fetching so artwork can refresh. Disk cache failures fall back to fetching from Jellyfin. Browser views retain encoded artwork only for visible cards. Decoded pixels and GPU textures belong to visible image elements and are released when clipped out of view or removed, including album headers and old player thumbnails. Switching sections releases the hidden browser's artwork while keeping its metadata and scroll position. Returning to previously viewed artwork loads it from disk instead of a retained image cache. In-flight downloads can finish populating the disk cache, but their results cannot repopulate a hidden view.

Choose Play to replace the queue with an album, or click a track to replace the queue and start there. Queue, beside Play on the album page, appends the album in disc/track order without interrupting playback or changing pause. Playback continues through appended albums. Queueing while idle waits for the player's Play button; if the old queue has finished, Play continues with the newly appended tracks.

If a queue exists, album Play buttons and album track clicks ask before replacing it. The dialog defaults to Keep queue, and Escape cancels. Replace queue continues with the album or track you clicked. Playback continues while the dialog is open. Cancelling preserves the current queue, pause state and pending queue actions. Queue buttons and next/previous do not prompt.

Right-click a track in an album or the queue sidebar and choose Queue to append just that track. This keeps the current playback and pause state. The menu stays attached to the chosen track even if playback advances. Enter activates Queue; Escape or clicking outside dismisses the menu.

The player stays at the bottom while you browse, with a cached album-cover thumbnail and track/artist/album details on the left, and centered previous, play/pause and next controls above the progress bar with elapsed and total time. Click the bar to seek, or drag to preview a position and release to seek there. Seeking preserves pause. Previous/next move through the queue and preserve pause when switching tracks. The play button offers retry after an error. Playback stops on errors without skipping tracks.

Click the album name in the player to open that album, or an artist name to browse their albums. Artist credits have separate links, with keyboard focus and hover styling. Navigation keeps the current playback and queue in place.

Album track lists highlight the current track with the same Catppuccin accent as the queue. The track number stays visible. The highlight follows automatic advancement, queue jumps and pause, and clears when playback stops, finishes or fails.

The current song downloads in the background to a temporary file under `$XDG_CACHE_HOME/jellyui/audio`, or `~/.cache/jellyui/audio`. Playback starts once the decoder has enough data, while the rest keeps downloading, including during pause. Seeking reuses that file. Reads beyond the downloaded portion wait for more data. Switching tracks cancels the old download and releases its cache file; audio files are not retained between tracks or app sessions.

Jellyui also preloads the first 256 KiB of the next queued song into a separate temporary file and prepares its decoder with a bounded 16-block PCM buffer. It requests only that prefix and closes the connection until the audio worker selects the track, then resumes downloading into the same file. Queue changes replace the preload when the next entry changes. Preparation does not report playback, and a failed prefix download falls back to a fresh download when selected.

Automatic advancement happens in the audio worker. Consecutive tracks sharing a sample rate and channel layout use the same PipeWire stream, including boundaries inside an output buffer. Source bit depth can differ because decoded samples use f64 output. A rate or layout change drains the previous stream and opens output in the next track's format, preserving the prepared PCM. Preparation and network stalls can still cause gaps. Track-boundary events update the UI and Jellyfin reports without making audio wait for UI polling. Seeking and manual track changes can restart output.

The right sidebar shows the ordered queue with each track's artist and album, a total track count, and the current track marked. Click a queued track to play it from the beginning and continue through the entries after it. This keeps the queue's contents and order, starts playback even when paused, and does not ask to replace the queue. Clicking the current entry restarts it using its cached audio. Drag a track to reorder it; neighboring tracks animate aside to open a gap where it will land. Dropping settles the track into place, while cancelling returns the rows to their original positions. Reordering keeps the current track playing at the same position. The sidebar scrolls independently of the album browser. The application has no top bar or logout control for now. Playback uses the system's default PipeWire output and requests the source sample rate while allowing PipeWire to convert as needed.

Hovering a queue row reveals an × to remove that entry. Removing another entry keeps the current audio running and updates the upcoming preload. Removing the current entry advances to its successor while preserving pause; removing the current last entry stops playback. Removing an idle selection keeps playback stopped. The button is also accessible by keyboard focus.

The volume slider in the player's bottom-right corner adjusts Jellyui's audio from silence to 100%. Click or drag to change it immediately. When focused, arrow keys adjust by five percentage points, Home mutes, and End restores 100%. Volume stays in place across seeks, track changes and app restarts, including mute. It is saved in `$XDG_CONFIG_HOME/jellyui/settings.json`, or `~/.config/jellyui/settings.json` when that variable is unset. Changes save in the background when you release the slider or use the keyboard; normal quit also saves the latest value. At 100%, Jellyui leaves samples unchanged; lower settings apply software attenuation before sending them to PipeWire.

Jellyui reports playback to Jellyfin after audio starts, then sends progress every ten seconds and updates for pause, resume, seeking, volume and queue changes. Track completion, replacement, errors and normal quit send stopped reports. Seeking keeps the same reported play-through. The server receives a window of up to twenty queued tracks. Reports run in order on a background worker; network failures are logged without stopping audio. Quit reporting is best-effort within the app's shutdown timeout.

External scrobbling requires a configured server plugin, such as the [Jellyfin Last.fm plugin](https://github.com/danielfariati/jellyfin-plugin-lastfm#per-user-settings). Its rules decide which stopped plays qualify. Jellyui reports the actual playback position and does not connect directly to Last.fm. [Feishin's Jellyfin controller](https://github.com/jeffvli/feishin/blob/8d12eae8a1551364442461bac02d99d78c185820/src/renderer/api/jellyfin/jellyfin-controller.ts#L1961) uses these same playback endpoints; its [scrobble hook](https://github.com/jeffvli/feishin/blob/8d12eae8a1551364442461bac02d99d78c185820/src/renderer/features/player/hooks/use-scrobble.ts) also submits a stopped report at its listening threshold to trigger plugin scrobbling before the track ends.

Jellyui exports [MPRIS](https://specifications.freedesktop.org/mpris/latest/) on the session D-Bus as `org.mpris.MediaPlayer2.jellyui.instance<PID>`. Desktop media controls can play, pause, stop, skip, seek, adjust the remembered volume, raise the app and quit. They receive playback status, track/artist/album metadata, duration, position and local artwork. Seek requests include stable queue-entry identities so a stale request cannot seek a different occurrence of the same song. Normal position updates do not emit property-change signals; completed seeks emit `Seeked`. Repeat, shuffle, speed changes and opening arbitrary URIs are unsupported. Missing D-Bus integration is logged and leaves in-app playback available. MPRIS tests use a private bus and generated local artwork, without launching Jellyui or controlling the user's session.

The MPRIS `DesktopEntry` and window app ID both use `jellyui`, matching the packaged `share/applications/jellyui.desktop` launcher and its X11 startup class. KDE uses this association to put artwork and playback controls in the taskbar's window preview. Install the package through a Nix profile, Home Manager or the system configuration to make its launcher visible to the desktop. Running a binary directly does not register the packaged launcher. For development, a local `~/.local/share/applications/jellyui.desktop` can use `nix develop /absolute/project/path --command /absolute/project/path/target/release/jellyui` as its Exec command. This starts the existing release build with its native libraries, without compiling the app. Remove a development launcher when switching to a normally installed package so it does not shadow the packaged entry.

The interface uses [Catppuccin Mocha](https://catppuccin.com/palette/). Palette values live in `src/theme.rs`; UI code refers to names such as `BASE`, `TEXT`, and `MAUVE`.

## Logging

Jellyui writes tracing logs to stderr. By default, application events use the `info` level and dependencies use `warn`. Startup, sign-in, credential storage, playback, and library failures are logged. For request status, timings, and more application detail:

```sh
RUST_LOG=warn,jellyui=debug nix develop -c cargo run
```

For an installed build, use `RUST_LOG=warn,jellyui=debug ./result/bin/jellyui`. To save a diagnostic log, append `2>jellyui.log`. Application logs exclude usernames, passwords, access tokens, request headers, and response bodies.

## Build and check

```sh
nix develop -c cargo fmt --check
nix develop -c cargo test
nix develop -c cargo clippy --all-targets -- -D warnings
nix build
./result/bin/jellyui
```

Nix builds use [Crane](https://crane.dev/) to cache compiled dependencies separately from the application. Once `cargoArtifacts` has been built, changes to Jellyui's Rust source or colors reuse the cached GPUI build. Dependency, toolchain, build profile, or native-library changes can invalidate this cache. The first Crane build populates it once; the previous builder's package output does not contain reusable Cargo artifacts.

Use `nix build .#cargoArtifacts` to build just the dependency cache, or plain `nix build` to build the dependencies and application. Crane vendors from `Cargo.lock`, including its pinned Git commits, without a separate Git hash table in the flake.

GPUI and its platform backend track Zed's GitHub `main` branch. `Cargo.lock` pins their exact commit for reproducible builds. Update them together with `cargo update -p gpui -p gpui_platform`, and use `--locked` in automated builds.

The text input adapts GPUI's input example. Its Apache license is included under `licenses/`.

## Project notes

- [Open work](issue-tracker/open/first-version.md)
- [Completed research](issue-tracker/closed/pipewire-research.md)
- [Archived audio experiment](issue-tracker/closed/audio-transition-demo.md)
- [Domain glossary](CONTEXT.md) and [ADRs](docs/adr/)
