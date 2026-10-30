use std::sync::Arc;

use crate::jellyfin::Item;

pub struct QueuedTrack {
    pub id: usize,
    pub track: Item,
    pub album: Arc<Item>,
}

/// Ordered playback across albums. Play replaces the snapshot; Queue appends.
#[derive(Default)]
pub struct Queue {
    pub tracks: Vec<QueuedTrack>,
    pub position: usize,
    revision: u64,
    next_entry_id: usize,
}

impl Queue {
    pub fn replace(&mut self, album: Item, tracks: Vec<Item>, start: usize) -> bool {
        if start >= tracks.len() {
            return false;
        }
        self.tracks.clear();
        self.append(album, tracks);
        self.position = start;
        true
    }

    pub fn append(&mut self, album: Item, tracks: Vec<Item>) -> bool {
        if tracks.is_empty() {
            return false;
        }
        let album = Arc::new(album);
        for track in tracks {
            self.tracks.push(QueuedTrack {
                id: self.next_entry_id,
                track,
                album: album.clone(),
            });
            self.next_entry_id = self.next_entry_id.wrapping_add(1);
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub fn clear(&mut self) {
        self.tracks.clear();
        self.position = 0;
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Remove one stable entry, retaining the current entry when it survives.
    /// Removing the current entry selects its successor, or the previous last entry.
    pub fn remove(&mut self, entry: usize) -> bool {
        let Some(index) = self.tracks.iter().position(|track| track.id == entry) else {
            return false;
        };
        self.tracks.remove(index);
        if index < self.position {
            self.position -= 1;
        }
        self.position = self.position.min(self.tracks.len().saturating_sub(1));
        self.revision = self.revision.wrapping_add(1);
        true
    }

    /// Move an entry to a boundary in the original queue, including its end.
    /// Keep the same entry selected, even when the same song appears twice.
    pub fn move_track(&mut self, from: usize, before: usize) -> bool {
        if from >= self.tracks.len() || before > self.tracks.len() {
            return false;
        }
        let to = if before > from { before - 1 } else { before };
        if from == to {
            return false;
        }
        let entry = self.tracks.remove(from);
        self.tracks.insert(to, entry);
        if self.position == from {
            self.position = to;
        } else {
            if from < self.position {
                self.position -= 1;
            }
            if to <= self.position {
                self.position += 1;
            }
        }
        self.revision = self.revision.wrapping_add(1);
        true
    }

    pub fn current(&self) -> Option<&Item> {
        self.tracks.get(self.position).map(|entry| &entry.track)
    }

    pub fn current_album(&self) -> Option<&Item> {
        self.tracks
            .get(self.position)
            .map(|entry| entry.album.as_ref())
    }

    pub fn advance(&mut self) -> bool {
        if self.position + 1 >= self.tracks.len() {
            return false;
        }
        self.position += 1;
        true
    }

    pub fn previous(&mut self) -> bool {
        if self.position == 0 || self.current().is_none() {
            return false;
        }
        self.position -= 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str) -> Item {
        serde_json::from_value(serde_json::json!({"Id": id, "Name": id})).unwrap()
    }

    #[test]
    fn selected_track_continues_to_album_end_and_retains_queue() {
        let mut queue = Queue::default();
        assert!(queue.replace(item("album"), vec![item("a"), item("b"), item("c")], 1));
        assert_eq!(queue.current().unwrap().id, "b");
        assert!(queue.advance());
        assert_eq!(queue.current().unwrap().id, "c");
        assert!(!queue.advance());
        assert_eq!(queue.tracks.len(), 3);
        assert_eq!(queue.current().unwrap().id, "c");
        assert!(queue.replace(item("other"), vec![item("d")], 0));
        assert_eq!(queue.tracks.len(), 1);
        assert_eq!(queue.current().unwrap().id, "d");
        assert!(!queue.replace(item("empty"), vec![], 0));
        assert_eq!(queue.current().unwrap().id, "d");
    }

    #[test]
    fn previous_and_next_stay_within_the_queue() {
        let mut queue = Queue::default();
        assert!(!queue.previous());
        assert!(!queue.advance());
        queue.replace(item("album"), vec![item("a"), item("b"), item("c")], 1);
        assert!(queue.previous());
        assert_eq!(queue.current().unwrap().id, "a");
        assert!(!queue.previous());
        assert!(queue.advance());
        assert!(queue.advance());
        assert_eq!(queue.current().unwrap().id, "c");
        assert!(!queue.advance());
        assert!(queue.previous());
        assert_eq!(queue.current().unwrap().id, "b");
        assert_eq!(queue.tracks.len(), 3);
        queue.replace(item("single"), vec![item("only")], 0);
        assert!(!queue.previous());
        assert!(!queue.advance());
    }

    #[test]
    fn appending_preserves_position_and_crosses_album_boundaries_in_order() {
        let mut queue = Queue::default();
        queue.replace(item("first"), vec![item("a"), item("b")], 1);
        assert!(queue.append(item("second"), vec![item("c"), item("d")]));
        assert_eq!(queue.position, 1);
        assert_eq!(queue.current().unwrap().id, "b");
        assert_eq!(queue.current_album().unwrap().id, "first");
        assert!(queue.advance());
        assert_eq!(queue.current().unwrap().id, "c");
        assert_eq!(queue.current_album().unwrap().id, "second");
        assert!(queue.previous());
        assert_eq!(queue.current_album().unwrap().id, "first");
        assert_eq!(
            queue
                .tracks
                .iter()
                .map(|entry| entry.track.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
        assert!(queue.replace(item("replacement"), vec![item("x"), item("y")], 1));
        assert_eq!(queue.position, 1);
        assert_eq!(queue.tracks.len(), 2);
        assert_eq!(queue.current().unwrap().id, "y");
        assert_eq!(queue.current_album().unwrap().id, "replacement");
    }

    #[test]
    fn albums_can_be_queued_into_an_empty_or_exhausted_queue_and_repeated() {
        let mut queue = Queue::default();
        assert!(!queue.append(item("empty"), vec![]));
        assert!(queue.current().is_none());
        assert!(queue.current_album().is_none());
        assert!(queue.append(item("album"), vec![item("a"), item("b")]));
        assert_eq!(queue.position, 0);
        assert_eq!(queue.current().unwrap().id, "a");
        assert!(queue.advance());
        assert!(!queue.advance());
        assert!(queue.append(item("album"), vec![item("a"), item("b")]));
        assert_eq!(queue.current().unwrap().id, "b");
        assert!(queue.advance());
        assert_eq!(queue.current().unwrap().id, "a");
        assert_eq!(queue.tracks.len(), 4);
        assert!(!queue.append(item("empty"), vec![]));
        assert_eq!(queue.position, 2);
        assert_eq!(queue.tracks.len(), 4);
    }

    #[test]
    fn moves_use_original_boundaries_and_support_both_ends() {
        let mut queue = Queue::default();
        queue.replace(
            item("album"),
            vec![item("a"), item("b"), item("c"), item("d")],
            1,
        );
        let order = |queue: &Queue| {
            queue
                .tracks
                .iter()
                .map(|entry| entry.track.id.clone())
                .collect::<Vec<_>>()
        };
        assert!(queue.move_track(1, 4));
        assert_eq!(order(&queue), ["a", "c", "d", "b"]);
        assert_eq!(queue.position, 3);
        assert!(queue.move_track(2, 0));
        assert_eq!(order(&queue), ["d", "a", "c", "b"]);
        assert!(queue.move_track(3, 1));
        assert_eq!(order(&queue), ["d", "b", "a", "c"]);
        assert_eq!(queue.position, 1);

        let revision = queue.revision();
        for (from, before) in [(0, 0), (0, 1), (3, 4), (4, 0), (0, 5), (usize::MAX, 0)] {
            assert!(!queue.move_track(from, before));
        }
        assert_eq!(order(&queue), ["d", "b", "a", "c"]);
        assert_eq!(queue.revision(), revision);
        assert!(!Queue::default().move_track(0, 0));
    }

    #[test]
    fn every_move_keeps_the_same_entry_selected_including_duplicate_songs() {
        for current in 0..5 {
            for from in 0..5 {
                for before in 0..=5 {
                    let mut queue = Queue::default();
                    for index in 0..5 {
                        let mut track = item("duplicate-song-id");
                        track.name = index.to_string();
                        queue.append(item(&format!("album-{index}")), vec![track]);
                    }
                    queue.position = current;
                    let album = queue.tracks[current].album.clone();
                    let revision = queue.revision();
                    let moved = queue.move_track(from, before);
                    assert_eq!(moved, before != from && before != from + 1);
                    assert_eq!(queue.current().unwrap().name, current.to_string());
                    assert!(Arc::ptr_eq(&album, &queue.tracks[queue.position].album));
                    assert_eq!(queue.revision() != revision, moved);
                    let mut names = queue
                        .tracks
                        .iter()
                        .map(|entry| entry.track.name.as_str())
                        .collect::<Vec<_>>();
                    names.sort_unstable();
                    assert_eq!(names, ["0", "1", "2", "3", "4"]);
                }
            }
        }
    }

    #[test]
    fn structural_changes_invalidate_drag_revisions_but_navigation_does_not() {
        let mut queue = Queue::default();
        queue.replace(item("album"), vec![item("a"), item("b")], 0);
        let revision = queue.revision();
        assert!(queue.advance());
        assert!(queue.previous());
        assert_eq!(queue.revision(), revision);
        assert!(!queue.append(item("empty"), vec![]));
        assert!(!queue.replace(item("empty"), vec![], 0));
        assert_eq!(queue.revision(), revision);
        queue.append(item("other"), vec![item("c")]);
        assert!(queue.revision() > revision);
        let revision = queue.revision();
        queue.replace(item("replacement"), vec![item("a"), item("b")], 0);
        assert!(queue.revision() > revision);
        let revision = queue.revision();
        queue.clear();
        assert!(queue.tracks.is_empty());
        assert_eq!(queue.position, 0);
        assert!(queue.revision() > revision);
    }

    #[test]
    fn entry_ids_follow_duplicate_tracks_through_moves_and_are_not_reused() {
        let mut queue = Queue::default();
        queue.append(
            item("album"),
            vec![item("same"), item("same"), item("same")],
        );
        let ids: Vec<_> = queue.tracks.iter().map(|entry| entry.id).collect();
        assert_ne!(ids[0], ids[1]);
        assert_ne!(ids[0], ids[2]);
        assert_ne!(ids[1], ids[2]);
        queue.move_track(0, 3);
        assert_eq!(
            queue
                .tracks
                .iter()
                .map(|entry| entry.id)
                .collect::<Vec<_>>(),
            [ids[1], ids[2], ids[0]]
        );
        queue.clear();
        queue.append(item("album"), vec![item("same")]);
        let appended = queue.tracks[0].id;
        assert!(!ids.contains(&appended));
        queue.replace(item("album"), vec![item("same")], 0);
        assert_ne!(queue.tracks[0].id, appended);
        assert!(!ids.contains(&queue.tracks[0].id));
    }

    #[test]
    fn removing_entries_preserves_the_current_identity_or_selects_its_successor() {
        for current in 0..5 {
            for removed in 0..5 {
                let mut queue = Queue::default();
                for index in 0..5 {
                    queue.append(item(&format!("album-{index}")), vec![item("same-song")]);
                }
                queue.position = current;
                let ids: Vec<_> = queue.tracks.iter().map(|entry| entry.id).collect();
                let revision = queue.revision();
                assert!(queue.remove(ids[removed]));
                let expected_current = if current != removed {
                    current
                } else if current < 4 {
                    current + 1
                } else {
                    current - 1
                };
                assert_eq!(queue.tracks[queue.position].id, ids[expected_current]);
                assert_eq!(
                    queue.current_album().unwrap().id,
                    format!("album-{expected_current}")
                );
                assert_eq!(
                    queue
                        .tracks
                        .iter()
                        .map(|entry| entry.id)
                        .collect::<Vec<_>>(),
                    ids.iter()
                        .copied()
                        .enumerate()
                        .filter_map(|(index, id)| (index != removed).then_some(id))
                        .collect::<Vec<_>>()
                );
                assert_ne!(queue.revision(), revision);
                let revision = queue.revision();
                assert!(!queue.remove(ids[removed]));
                assert_eq!(queue.revision(), revision);
            }
        }
        let mut queue = Queue::default();
        assert!(!queue.remove(0));
        queue.append(item("album"), vec![item("only")]);
        let entry = queue.tracks[0].id;
        assert!(queue.remove(entry));
        assert!(queue.current().is_none());
        assert_eq!(queue.position, 0);
        queue.append(item("album"), vec![item("next")]);
        assert_ne!(queue.tracks[0].id, entry);
    }
}
