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
