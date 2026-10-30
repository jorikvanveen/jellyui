use super::QUEUE_ROW_PITCH;
use crate::ui::theme;
use gpui::{Context, EntityId, Window, div, prelude::*, px, rgb};
#[derive(Clone)]
pub(super) struct QueueDrag {
    pub(super) owner: EntityId,
    pub(super) revision: u64,
    pub(super) index: usize,
    pub(super) title: String,
    pub(super) subtitle: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct QueueReorder {
    pub(super) from: usize,
    pub(super) before: usize,
    pub(super) revision: u64,
    pub(super) inside: bool,
}

pub(super) fn queue_preview_index(index: usize, from: usize, before: usize) -> usize {
    let to = if before > from { before - 1 } else { before };
    if index == from {
        to
    } else if from < index && index <= to {
        index - 1
    } else if to <= index && index < from {
        index + 1
    } else {
        index
    }
}

pub(super) fn queue_drop_before(from: usize, before: usize, y: f32, count: usize) -> usize {
    if y < 0. {
        return 0;
    }
    if y >= count as f32 * QUEUE_ROW_PITCH {
        return count;
    }
    let slot = (y / QUEUE_ROW_PITCH) as usize;
    // Keep the current target throughout the open gap. Animated rows never
    // change these hit areas, so a stationary pointer cannot flip the target.
    if slot == queue_preview_index(from, from, before) {
        return before;
    }
    slot + usize::from(y % QUEUE_ROW_PITCH >= QUEUE_ROW_PITCH / 2.)
}

impl Render for QueueDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .w(px(280.))
            .p_3()
            .rounded_md()
            .bg(rgb(theme::SURFACE0))
            .border_1()
            .border_color(rgb(theme::MAUVE))
            .shadow_md()
            .text_color(rgb(theme::TEXT))
            .flex()
            .flex_col()
            .gap_1()
            .child(div().truncate().child(self.title.clone()))
            .child(
                div()
                    .truncate()
                    .text_sm()
                    .text_color(rgb(theme::SUBTEXT0))
                    .child(self.subtitle.clone()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Item, queue::Queue};
    #[test]
    fn animated_queue_positions_match_every_possible_committed_move() {
        for from in 0..5 {
            for before in 0..=5 {
                let mut queue = Queue::default();
                queue.append(item(serde_json::json!({"Id":"album", "Name":"Album"})),
                    (0..5).map(|index| item(serde_json::json!({"Id":index.to_string(), "Name":index.to_string()}))).collect());
                queue.move_track(from, before);
                for index in 0..5 {
                    let preview_index = queue_preview_index(index, from, before);
                    assert_eq!(queue.tracks[preview_index].track.id, index.to_string());
                }
            }
        }
    }

    #[test]
    fn queue_gap_stays_open_until_the_pointer_leaves_it_and_supports_both_ends() {
        for fraction in [0., 0.25, 0.5, 0.99] {
            assert_eq!(
                queue_drop_before(2, 4, (3. + fraction) * QUEUE_ROW_PITCH, 5),
                4
            );
            assert_eq!(queue_drop_before(3, 0, fraction * QUEUE_ROW_PITCH, 5), 0);
        }
        assert_eq!(queue_drop_before(2, 4, 4.25 * QUEUE_ROW_PITCH, 5), 4);
        assert_eq!(queue_drop_before(2, 4, 4.75 * QUEUE_ROW_PITCH, 5), 5);
        assert_eq!(queue_drop_before(2, 2, -10., 5), 0);
        assert_eq!(queue_drop_before(2, 2, 10_000., 5), 5);
        assert_eq!(queue_drop_before(2, 2, 0.75 * QUEUE_ROW_PITCH, 5), 1);
        assert_eq!(queue_drop_before(2, 2, 1.25 * QUEUE_ROW_PITCH, 5), 1);
        assert_eq!(queue_drop_before(0, 0, 0., 0), 0);
    }

    fn item(value: serde_json::Value) -> Item {
        serde_json::from_value(value).unwrap()
    }
}
