use serde::Deserialize;

pub const PAGE_SIZE: usize = 100;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum AlbumSort {
    #[default]
    Title,
    Artist,
    Year,
    Added,
}

#[derive(Clone, Default, PartialEq, Eq)]
pub struct AlbumQuery {
    pub search: String,
    pub sort: AlbumSort,
    pub descending: bool,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Artist {
    pub id: Option<String>,
    pub name: String,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Item {
    pub id: String,
    pub name: String,
    pub sort_name: Option<String>,
    pub date_created: Option<String>,
    #[serde(default)]
    pub album_artist: Option<String>,
    #[serde(default)]
    pub artists: Option<Vec<String>>,
    pub artist_items: Option<Vec<Artist>>,
    pub album_artists: Option<Vec<Artist>>,
    pub production_year: Option<u32>,
    pub premiere_date: Option<String>,
    pub child_count: Option<usize>,
    pub index_number: Option<u32>,
    pub parent_index_number: Option<u32>,
    pub run_time_ticks: Option<u64>,
}

impl Item {
    pub fn artist(&self) -> String {
        self.album_artist
            .clone()
            .filter(|s| !s.is_empty())
            .or_else(|| {
                self.artists
                    .as_ref()
                    .filter(|a| !a.is_empty())
                    .map(|a| a.join(", "))
            })
            .unwrap_or_else(|| "Unknown artist".into())
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct Page {
    pub items: Vec<Item>,
    pub total_record_count: usize,
}

pub fn artist_credits(track: &Item, album: &Item) -> Vec<Artist> {
    let named = |name: &String| Artist {
        id: None,
        name: name.clone(),
    };
    // Prefer album-artist credits while retaining separate artist identities.
    // A comma can be part of an artist's name, so never split display strings.
    let mut artists =
        if let Some(name) = track.album_artist.as_ref().filter(|name| !name.is_empty()) {
            track
                .album_artists
                .as_ref()
                .filter(|artists| !artists.is_empty())
                .or_else(|| {
                    album.album_artists.as_ref().filter(|artists| {
                        !artists.is_empty() && album.album_artist.as_ref() == Some(name)
                    })
                })
                .cloned()
                .unwrap_or_else(|| vec![named(name)])
        } else if let Some(names) = track.artists.as_ref().filter(|names| !names.is_empty()) {
            names.iter().map(named).collect()
        } else {
            track.artist_items.clone().unwrap_or_default()
        };
    for artist in &mut artists {
        if artist.id.as_deref().is_none_or(str::is_empty) {
            artist.id = track
                .album_artists
                .iter()
                .flatten()
                .chain(track.artist_items.iter().flatten())
                .chain(album.album_artists.iter().flatten())
                .chain(album.artist_items.iter().flatten())
                .find(|credit| {
                    credit.name.eq_ignore_ascii_case(&artist.name)
                        && credit.id.as_deref().is_some_and(|id| !id.is_empty())
                })
                .and_then(|credit| credit.id.clone());
        }
    }
    artists.retain(|artist| !artist.name.trim().is_empty());
    artists
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(value: serde_json::Value) -> Item {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn player_links_preserve_separate_album_artist_identities() {
        let track = item(serde_json::json!({
            "Id":"song", "Name":"Song", "AlbumArtist":"Two credits",
            "AlbumArtists":[{"Id":"one", "Name":"Same name"}, {"Id":"two", "Name":"Same name"}],
            "ArtistItems":[{"Id":"wrong", "Name":"Same name"}]
        }));
        let album = item(serde_json::json!({"Id":"album", "Name":"Album"}));
        let artists = artist_credits(&track, &album);
        assert_eq!(
            artists
                .iter()
                .map(|artist| artist.name.as_str())
                .collect::<Vec<_>>(),
            ["Same name", "Same name"]
        );
        assert_eq!(
            artists
                .iter()
                .map(|artist| artist.id.as_deref())
                .collect::<Vec<_>>(),
            [Some("one"), Some("two")]
        );
    }

    #[test]
    fn player_links_match_name_only_credits_without_splitting_artist_names() {
        let track = item(serde_json::json!({
            "Id":"song", "Name":"Song", "Artists":["Earth, Wind & Fire", "Guest"],
            "ArtistItems":[{"Id":"band", "Name":"Earth, Wind & Fire"}]
        }));
        let album = item(serde_json::json!({
            "Id":"album", "Name":"Album", "AlbumArtists":[{"Id":"guest", "Name":"guest"}]
        }));
        let artists = artist_credits(&track, &album);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].name, "Earth, Wind & Fire");
        assert_eq!(artists[0].id.as_deref(), Some("band"));
        assert_eq!(artists[1].id.as_deref(), Some("guest"));
    }

    #[test]
    fn player_links_use_album_credits_and_allow_name_lookup_when_ids_are_missing() {
        let track = item(serde_json::json!({"Id":"song", "Name":"Song", "AlbumArtist":"A & B"}));
        let album = item(serde_json::json!({
            "Id":"album", "Name":"Album", "AlbumArtist":"A & B",
            "AlbumArtists":[{"Id":"a", "Name":"A"}, {"Name":"B"}]
        }));
        let artists = artist_credits(&track, &album);
        assert_eq!(artists.len(), 2);
        assert_eq!(artists[0].id.as_deref(), Some("a"));
        assert_eq!(artists[1].name, "B");
        assert!(artists[1].id.is_none());
        let no_credits = item(serde_json::json!({"Id":"song", "Name":"Song"}));
        assert!(artist_credits(&no_credits, &album).is_empty());
    }
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum LibraryKind {
    #[default]
    Albums,
    Artists,
}
