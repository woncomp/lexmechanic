use crate::{runtime::DocumentMessage, workbench::message_rail::RailState};
use gpui_kit::{
    component::text::{RenderedText, TextViewState},
    *,
};
use std::collections::{HashMap, HashSet};

#[derive(Clone, Copy)]
pub struct DocumentRow {
    pub message: usize,
    pub part: usize,
}

pub struct CachedChunk {
    pub state: Entity<TextViewState>,
    pub bytes: usize,
    pub used: u64,
    pub highlighted: String,
    pub snapshot: Option<RenderedText>,
    pub source_prefix: usize,
}

pub struct Document {
    pub key: String,
    pub messages: Vec<DocumentMessage>,
    pub rows: Vec<DocumentRow>,
    pub collapsed: HashSet<String>,
    pub chunks: HashMap<(i64, usize), CachedChunk>,
    pub pending: HashSet<(i64, usize)>,
    pub list: ListState,
    pub nav_scroll: UniformListScrollHandle,
    pub rail: RailState,
    pub current: usize,
    pub sync_current: bool,
    pub complete: bool,
    pub loading: bool,
    pub tick: u64,
    pub reveal: Option<(i64, usize, usize)>,
    pub error: Option<String>,
}

impl Document {
    pub fn new(key: String) -> Self {
        Self {
            key,
            messages: Vec::new(),
            rows: Vec::new(),
            collapsed: HashSet::new(),
            chunks: HashMap::new(),
            pending: HashSet::new(),
            list: ListState::new(0, ListAlignment::Top, px(500.)),
            nav_scroll: UniformListScrollHandle::new(),
            rail: RailState::default(),
            current: 0,
            sync_current: false,
            complete: false,
            loading: true,
            tick: 0,
            reveal: None,
            error: None,
        }
    }

    pub fn rebuild(&mut self) {
        self.rows.clear();
        for (index, message) in self.messages.iter().enumerate() {
            let count = if self.collapsed.contains(&message.native_id) {
                1
            } else {
                message.parts.len()
            };
            for part in 0..count {
                self.rows.push(DocumentRow {
                    message: index,
                    part,
                });
            }
        }
        self.list.reset(self.rows.len());
    }

    pub fn jump(&mut self, message: usize) {
        if let Some(index) = self.rows.iter().position(|row| row.message == message) {
            self.current = message;
            self.list.scroll_to(ListOffset {
                item_ix: index,
                offset_in_item: px(0.),
            });
            self.follow_current();
        }
    }

    /// Picks the message under a reading line held at the viewport center, which slides to the
    /// top/bottom edge over the first/last half screen so the first and last messages are reachable.
    pub fn sync_current_from_scroll(&mut self) {
        if !std::mem::take(&mut self.sync_current) || self.rows.is_empty() {
            return;
        }
        let viewport = self.list.viewport_bounds();
        let height = viewport.size.height;
        if height <= px(0.) {
            return;
        }
        let max = self.list.max_offset_for_scrollbar().y.max(px(0.));
        let offset = (-self.list.scroll_px_offset_for_scrollbar().y).clamp(px(0.), max);
        let line = reading_line(offset, max, height).min((height - px(1.)).max(px(0.)));
        let probe = viewport.top() + line;
        let mut index = self
            .list
            .logical_scroll_top()
            .item_ix
            .min(self.rows.len() - 1);
        while index + 1 < self.rows.len() {
            match self.list.bounds_for_item(index) {
                Some(bounds) if bounds.bottom() <= probe => index += 1,
                _ => break,
            }
        }
        if self.rows[index].message != self.current {
            self.current = self.rows[index].message;
            self.follow_current();
        }
    }

    pub fn follow_current(&self) {
        if !self.rail.exploring() && !self.messages.is_empty() {
            self.nav_scroll
                .scroll_to_item(self.current, ScrollStrategy::Nearest);
        }
    }

    pub fn evict(&mut self) {
        let mut bytes: usize = self.chunks.values().map(|chunk| chunk.bytes).sum();
        if bytes <= 128 * 1024 * 1024 && self.chunks.len() <= 256 {
            return;
        }
        let mut oldest: Vec<_> = self
            .chunks
            .iter()
            .map(|(key, value)| (*key, value.used, value.bytes))
            .collect();
        oldest.sort_by_key(|(_, used, _)| *used);
        for (key, _, size) in oldest {
            if bytes <= 96 * 1024 * 1024 && self.chunks.len() <= 192 {
                break;
            }
            self.chunks.remove(&key);
            bytes = bytes.saturating_sub(size);
        }
    }
}

/// Reading line position within the viewport for a scroll `offset` out of `max`.
fn reading_line(offset: Pixels, max: Pixels, height: Pixels) -> Pixels {
    if max < height {
        // Too short for both half-screen ramps: sweep top to bottom linearly.
        return if max > px(0.) {
            height * (offset / max)
        } else {
            px(0.)
        };
    }
    let half = height * 0.5;
    let remaining = max - offset;
    if offset < half {
        offset
    } else if remaining < half {
        height - remaining
    } else {
        half
    }
}

#[cfg(test)]
mod tests {
    use super::{px, reading_line};

    #[test]
    fn reading_line_centers_and_ramps_to_both_edges() {
        let h = px(800.);
        assert_eq!(reading_line(px(0.), px(5000.), h), px(0.));
        assert_eq!(reading_line(px(200.), px(5000.), h), px(200.));
        assert_eq!(reading_line(px(400.), px(5000.), h), px(400.));
        assert_eq!(reading_line(px(2500.), px(5000.), h), px(400.));
        assert_eq!(reading_line(px(4800.), px(5000.), h), px(600.));
        assert_eq!(reading_line(px(5000.), px(5000.), h), h);
        assert_eq!(reading_line(px(150.), px(300.), h), px(400.));
        assert_eq!(reading_line(px(300.), px(300.), h), h);
        assert_eq!(reading_line(px(0.), px(0.), h), px(0.));
    }
}
