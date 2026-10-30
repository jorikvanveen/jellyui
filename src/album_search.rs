use std::collections::HashSet;

use crate::jellyfin::{AlbumQuery, AlbumSort, Item};

/// A complete search order, without retaining album metadata or artwork.
pub struct AlbumSearch {
    pub ids: Vec<String>,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum SortKey {
    Title(String),
    Artist(String, String),
    Year(Option<u32>, String),
    Added(Option<String>, String),
}

pub fn search_words(query: &str) -> Vec<&str> {
    query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect()
}

fn match_rank(item: &Item, words: &[String], source_rank: u8) -> Option<u8> {
    if words.len() <= 1 {
        return Some(source_rank);
    }
    let title = item.name.to_lowercase();
    let artists: Vec<_> = item
        .album_artist
        .iter()
        .chain(item.artists.iter().flatten())
        .chain(
            item.artist_items
                .iter()
                .flatten()
                .map(|artist| &artist.name),
        )
        .chain(
            item.album_artists
                .iter()
                .flatten()
                .map(|artist| &artist.name),
        )
        .map(|name| name.to_lowercase())
        .collect();
    if !words
        .iter()
        .all(|word| title.contains(word) || artists.iter().any(|name| name.contains(word)))
    {
        return None;
    }
    Some(if words.iter().all(|word| title.contains(word)) {
        0
    } else if words.iter().any(|word| title.contains(word)) {
        1
    } else {
        2
    })
}

impl AlbumSearch {
    pub fn new(titles: Vec<Item>, artists: Vec<Item>, query: &AlbumQuery) -> Self {
        let words: Vec<_> = search_words(&query.search)
            .into_iter()
            .map(str::to_lowercase)
            .collect();
        let mut hits: Vec<_> = titles
            .into_iter()
            .map(|item| (0, item))
            .chain(artists.into_iter().map(|item| (1, item)))
            .filter_map(|(rank, item)| {
                let rank = match_rank(&item, &words, rank)?;
                let title = item
                    .sort_name
                    .as_deref()
                    .unwrap_or(&item.name)
                    .to_lowercase();
                let key = match query.sort {
                    AlbumSort::Title => SortKey::Title(title),
                    AlbumSort::Artist => SortKey::Artist(item.artist().to_lowercase(), title),
                    AlbumSort::Year => SortKey::Year(item.production_year, title),
                    AlbumSort::Added => SortKey::Added(item.date_created, title),
                };
                Some((rank, key, item.id))
            })
            .collect();
        hits.sort_unstable_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| {
                    if query.descending {
                        b.1.cmp(&a.1)
                    } else {
                        a.1.cmp(&b.1)
                    }
                })
                .then_with(|| a.2.cmp(&b.2))
        });
        let mut seen = HashSet::new();
        let ids = hits
            .into_iter()
            .filter_map(|(_, _, id)| seen.insert(id.clone()).then_some(id))
            .collect();
        Self { ids }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, name: &str, year: u32, artist: &str) -> Item {
        serde_json::from_value(serde_json::json!({
            "Id": id, "Name": name, "ProductionYear": year, "AlbumArtist": artist,
            "DateCreated": format!("{year}-01-01T00:00:00Z")
        }))
        .unwrap()
    }

    #[test]
    fn title_matches_always_precede_artist_matches_and_duplicates_appear_once() {
        for descending in [false, true] {
            let query = AlbumQuery {
                search: "blue".into(),
                descending,
                ..Default::default()
            };
            let both = album("both", "Blue", 2024, "Blue artist");
            let result = AlbumSearch::new(
                vec![both.clone(), album("title", "Blue skies", 2020, "Other")],
                vec![album("artist", "AAA", 2026, "Blue artist"), both],
                &query,
            );
            assert_eq!(result.ids.len(), 3);
            assert_eq!(result.ids.last().unwrap(), "artist");
            assert_eq!(
                &result.ids[..2],
                if descending {
                    ["title", "both"]
                } else {
                    ["both", "title"]
                }
            );
        }
    }

    #[test]
    fn sort_options_apply_within_each_match_group_in_both_directions() {
        for (sort, ascending) in [
            (AlbumSort::Artist, ["b", "a"]),
            (AlbumSort::Year, ["b", "a"]),
            (AlbumSort::Added, ["b", "a"]),
        ] {
            for descending in [false, true] {
                let query = AlbumQuery {
                    search: "x".into(),
                    sort,
                    descending,
                };
                let result = AlbumSearch::new(
                    vec![
                        album("a", "A", 2024, "Zebra"),
                        album("b", "B", 2020, "Alpha"),
                    ],
                    vec![album("c", "C", 2030, "AAA")],
                    &query,
                );
                let mut expected = ascending.to_vec();
                if descending {
                    expected.reverse();
                }
                expected.push("c");
                assert_eq!(result.ids, expected);
            }
        }
    }

    #[test]
    fn ranking_is_global_across_page_boundaries() {
        let titles = (0..105)
            .map(|n| album(&format!("title-{n:03}"), &format!("Z{n:03}"), 2026, "Other"))
            .collect();
        let result = AlbumSearch::new(
            titles,
            vec![album("artist", "AAA", 2020, "Match")],
            &AlbumQuery::default(),
        );
        assert_eq!(result.ids.len(), 106);
        assert_eq!(result.ids[100], "title-100");
        assert_eq!(result.ids[104], "title-104");
        assert_eq!(result.ids[105], "artist");
    }

    #[test]
    fn album_and_artist_words_match_together_in_any_order() {
        for search in [
            "Abbey Road Beatles",
            "BEATLES road abbey",
            " Abbey-Road / The Beatles ",
        ] {
            let query = AlbumQuery {
                search: search.into(),
                ..Default::default()
            };
            let result = AlbumSearch::new(
                vec![album("wrong-artist", "Abbey Road", 1969, "Someone else")],
                vec![
                    album("correct", "Abbey Road", 1969, "The Beatles"),
                    album("wrong-title", "Let It Be", 1970, "The Beatles"),
                ],
                &query,
            );
            assert_eq!(result.ids, ["correct"], "{search}");
        }
    }

    #[test]
    fn title_matches_rank_before_mixed_matches_then_artist_only_matches() {
        let query = AlbumQuery {
            search: "blue skies".into(),
            ..Default::default()
        };
        let result = AlbumSearch::new(
            vec![album("title", "Blue skies", 2020, "Other")],
            vec![
                album("artist", "AAA", 2020, "Blue skies"),
                album("mixed", "Blue", 2020, "Skies"),
                album("missing-word", "Blue", 2020, "Other"),
            ],
            &query,
        );
        assert_eq!(result.ids, ["title", "mixed", "artist"]);
    }

    #[test]
    fn combined_search_includes_all_credited_artist_fields() {
        let item: Item = serde_json::from_value(serde_json::json!({
            "Id":"album", "Name":"Abbey Road", "AlbumArtist":"Various Artists",
            "Artists":["Track Performer"], "ArtistItems":[{"Name":"Guest Performer"}],
            "AlbumArtists":[{"Name":"Album Credit"}]
        }))
        .unwrap();
        for search in ["road track", "road guest", "road credit"] {
            let query = AlbumQuery {
                search: search.into(),
                ..Default::default()
            };
            assert_eq!(
                AlbumSearch::new(vec![], vec![item.clone()], &query).ids,
                ["album"]
            );
        }
    }
}
