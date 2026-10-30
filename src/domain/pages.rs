use std::{
    collections::{HashMap, VecDeque},
    ops::Range,
};

use crate::domain::{Item, PAGE_SIZE, Page};

const CACHED_PAGES: usize = 12;

/// Sparse, bounded metadata storage. The server's total determines the scroll
/// range; cached pages only determine which slots can currently be displayed.
#[derive(Default)]
pub struct LibraryPages {
    pub total: Option<usize>,
    pages: HashMap<usize, Vec<Item>>,
    recent: VecDeque<usize>,
}

impl LibraryPages {
    pub fn get(&self, index: usize) -> Option<&Item> {
        self.pages.get(&(index / PAGE_SIZE))?.get(index % PAGE_SIZE)
    }

    pub fn contains(&self, page: usize) -> bool {
        self.pages.contains_key(&page)
    }

    pub fn touch(&mut self, page: usize) {
        if self.contains(page) {
            self.recent.retain(|&cached| cached != page);
            self.recent.push_back(page);
        }
    }

    pub fn insert(&mut self, index: usize, page: Page) -> Result<(), &'static str> {
        let total = page.total_record_count;
        self.total = Some(total);
        self.pages.retain(|&index, _| index * PAGE_SIZE < total);
        self.recent.retain(|index| self.pages.contains_key(index));
        let expected = total.saturating_sub(index * PAGE_SIZE).min(PAGE_SIZE);
        if page.items.len() != expected {
            return Err(
                "Jellyfin returned an incomplete library page. Retry to refresh this part of the library.",
            );
        }
        if expected > 0 {
            self.pages.insert(index, page.items);
            self.touch(index);
        }
        while self.pages.len() > CACHED_PAGES {
            let oldest = self
                .recent
                .pop_front()
                .expect("cached page has recency entry");
            self.pages.remove(&oldest);
        }
        Ok(())
    }
}

/// Visible pages first, then one adjacent page in each direction for prefetch.
pub fn needed_pages(visible: Range<usize>, total: usize) -> Vec<usize> {
    let end = visible.end.min(total);
    if visible.start >= end {
        return Vec::new();
    }
    let first = visible.start / PAGE_SIZE;
    let last = (end - 1) / PAGE_SIZE;
    let mut pages: Vec<_> = (first..=last).collect();
    if last + 1 < total.div_ceil(PAGE_SIZE) {
        pages.push(last + 1);
    }
    if first > 0 {
        pages.push(first - 1);
    }
    pages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(index: usize, total: usize) -> Page {
        Page {
            total_record_count: total,
            items: (index * PAGE_SIZE..((index + 1) * PAGE_SIZE).min(total))
                .map(|index| {
                    serde_json::from_value(
                        serde_json::json!({"Id": index.to_string(), "Name": "Album"}),
                    )
                    .unwrap()
                })
                .collect(),
        }
    }

    #[test]
    fn scrollbar_total_is_independent_of_which_pages_are_cached() {
        let mut pages = LibraryPages::default();
        pages.insert(0, page(0, 10_025)).unwrap();
        pages.insert(100, page(100, 10_025)).unwrap();
        assert_eq!(pages.total, Some(10_025));
        assert_eq!(pages.get(10_024).unwrap().id, "10024");
        assert!(pages.get(5_000).is_none());
    }

    #[test]
    fn scroll_jumps_request_the_destination_and_adjacent_pages() {
        assert_eq!(needed_pages(5_010..5_040, 10_000), [50, 51, 49]);
        assert_eq!(needed_pages(98..103, 10_000), [0, 1, 2]);
        assert_eq!(needed_pages(10_000..10_025, 10_025), [100, 99]);
        assert!(needed_pages(0..10, 0).is_empty());
    }

    #[test]
    fn metadata_cache_is_bounded_and_keeps_recently_viewed_pages() {
        let mut pages = LibraryPages::default();
        for index in 0..CACHED_PAGES {
            pages.insert(index, page(index, 50_000)).unwrap();
        }
        pages.touch(0);
        pages.insert(30, page(30, 50_000)).unwrap();
        assert_eq!(pages.pages.len(), CACHED_PAGES);
        assert!(pages.contains(0));
        assert!(!pages.contains(1));
        assert_eq!(pages.total, Some(50_000));
    }

    #[test]
    fn empty_and_shrinking_libraries_update_the_total() {
        let mut pages = LibraryPages::default();
        pages.insert(10, page(10, 1_500)).unwrap();
        pages.insert(10, page(10, 50)).unwrap();
        assert_eq!(pages.total, Some(50));
        assert!(pages.get(1_000).is_none());
        pages.insert(0, page(0, 0)).unwrap();
        assert_eq!(pages.total, Some(0));
        assert!(pages.pages.is_empty());
    }

    #[test]
    fn incomplete_pages_do_not_turn_into_permanent_empty_slots() {
        let mut pages = LibraryPages::default();
        assert!(
            pages
                .insert(
                    5,
                    Page {
                        items: Vec::new(),
                        total_record_count: 1_000
                    }
                )
                .is_err()
        );
        assert!(!pages.contains(5));
        assert_eq!(pages.total, Some(1_000));
        pages.insert(5, page(5, 1_000)).unwrap();
        assert!(pages.get(500).is_some());
    }
}
