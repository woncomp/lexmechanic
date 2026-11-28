use super::{Workbench, message_rail::role_accent};
use crate::{runtime::SEARCH_HIT_LIMIT, session_table::SessionTable};
use agent_chat_reader_core::search::{HitKind, SearchHit, find_ranges};
use chrono::{DateTime, Utc};
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants},
        scroll::ScrollbarHandle,
        *,
    },
    prelude::FluentBuilder,
    *,
};
use std::{cell::RefCell, collections::HashSet, ops::Range, rc::Rc};

/// Rows have fixed heights so the scroll extent is known before any row is drawn.
const HEADER_HEIGHT: Pixels = px(32.);
/// Fits the role line and two snippet lines.
const HIT_HEIGHT: Pixels = px(76.);

/// One row of the grouped search results: a session header, then the message hits of that session.
#[derive(Debug, PartialEq)]
pub(super) enum SearchRow {
    Session {
        key: String,
        /// Index of the title hit of the session, shown in the header rather than as a row.
        title: Option<usize>,
        /// Index of the first hit of the session.
        first: usize,
        /// Message hits of the session.
        hits: usize,
        collapsed: bool,
    },
    Hit(usize),
}

impl SearchRow {
    fn height(&self) -> Pixels {
        match self {
            Self::Session { .. } => HEADER_HEIGHT,
            Self::Hit(_) => HIT_HEIGHT,
        }
    }
}

/// Scrollbar handle for the grouped results.
///
/// The list only knows the heights of rows it has drawn, so its own handle grows the content as
/// rows scroll into view. The rows here have fixed heights, so the extent is computed instead.
#[derive(Clone)]
pub(super) struct SearchScroll {
    pub(super) list: ListState,
    /// The top of each row, followed by the total height.
    tops: Rc<RefCell<Vec<Pixels>>>,
}

impl SearchScroll {
    pub(super) fn new() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(300.)),
            tops: Rc::new(RefCell::new(vec![px(0.)])),
        }
    }

    fn set_rows(&self, rows: &[SearchRow]) {
        let mut tops = self.tops.borrow_mut();
        tops.clear();
        tops.push(px(0.));
        let mut top = px(0.);
        for row in rows {
            top += row.height();
            tops.push(top);
        }
    }
}

impl ScrollbarHandle for SearchScroll {
    fn viewport_bounds(&self) -> Bounds<Pixels> {
        self.list.viewport_bounds()
    }

    fn offset(&self) -> Point<Pixels> {
        let top = self.list.logical_scroll_top();
        let tops = self.tops.borrow();
        let row = tops
            .get(top.item_ix)
            .copied()
            .unwrap_or_else(|| tops[tops.len() - 1]);
        point(px(0.), -(row + top.offset_in_item))
    }

    fn set_offset(&self, offset: Point<Pixels>) {
        let tops = self.tops.borrow();
        let total = tops[tops.len() - 1];
        let max = (total - self.list.viewport_bounds().size.height).max(px(0.));
        let y = (-offset.y).clamp(px(0.), max);
        let item_ix = tops.partition_point(|top| *top <= y).saturating_sub(1);
        self.list.scroll_to(ListOffset {
            item_ix,
            offset_in_item: y - tops[item_ix],
        });
    }

    fn content_size(&self) -> gpui_kit::Size<Pixels> {
        let tops = self.tops.borrow();
        size(self.list.viewport_bounds().size.width, tops[tops.len() - 1])
    }
}

struct Group {
    key: String,
    /// The session time when the group first appeared; groups never move afterwards.
    time: Option<DateTime<Utc>>,
    title: Option<usize>,
    hits: Vec<usize>,
}

/// Global search results grouped by session, newest session first.
///
/// Each session arrives once and complete, so a group never changes after it is added; later
/// sessions are only inserted between the existing groups. The hits themselves stay in the order
/// they arrived and are referred to by index.
#[derive(Default)]
pub(super) struct SearchResults {
    groups: Vec<Group>,
}

impl SearchResults {
    pub(super) fn clear(&mut self) {
        self.groups.clear();
    }

    pub(super) fn sessions(&self) -> usize {
        self.groups.len()
    }

    /// Adds `hits[start..]`, whose sessions arrive whole; `time` places each new session.
    pub(super) fn add(
        &mut self,
        hits: &[SearchHit],
        start: usize,
        time: impl Fn(&str) -> Option<DateTime<Utc>>,
    ) {
        let mut slot = 0;
        for (index, hit) in hits.iter().enumerate().skip(start) {
            if index == start || hits[index - 1].session != hit.session {
                let time = time(&hit.session);
                slot = self.groups.partition_point(|group| {
                    time.cmp(&group.time)
                        .then_with(|| group.key.cmp(&hit.session))
                        .is_lt()
                });
                self.groups.insert(
                    slot,
                    Group {
                        key: hit.session.clone(),
                        time,
                        title: None,
                        hits: Vec::new(),
                    },
                );
            }
            let group = &mut self.groups[slot];
            match hit.kind {
                HitKind::Title => group.title = Some(index),
                HitKind::Message => group.hits.push(index),
            }
        }
    }

    pub(super) fn rows(&self, collapsed: &HashSet<String>) -> Vec<SearchRow> {
        let mut rows = Vec::new();
        for group in &self.groups {
            let collapsed = collapsed.contains(&group.key);
            rows.push(SearchRow::Session {
                key: group.key.clone(),
                title: group.title,
                first: group
                    .title
                    .or(group.hits.first().copied())
                    .unwrap_or_default(),
                hits: group.hits.len(),
                collapsed,
            });
            if !collapsed {
                rows.extend(group.hits.iter().copied().map(SearchRow::Hit));
            }
        }
        rows
    }

    /// Hit indexes in display order, for stepping through the results.
    pub(super) fn order(&self) -> Vec<usize> {
        self.groups
            .iter()
            .flat_map(|group| group.title.iter().chain(&group.hits).copied())
            .collect()
    }
}

/// The list splices, applied in order, that turn the `old` rows into the `new` ones. Rows that
/// only appeared or only disappeared are spliced on their own, so rows around them, and the
/// scroll position, are kept.
pub(super) fn row_splices(old: &[SearchRow], new: &[SearchRow]) -> Vec<(Range<usize>, usize)> {
    let subsequence = |short: &[SearchRow], long: &[SearchRow]| {
        let mut rest = long.iter();
        short.iter().all(|row| rest.any(|other| other == row))
    };
    // Runs of `long` rows missing from `short`, as (index in `long`, length).
    let runs = |short: &[SearchRow], long: &[SearchRow]| {
        let mut runs = Vec::new();
        let mut kept = short.iter().peekable();
        let mut index = 0;
        while index < long.len() {
            if kept.peek() == Some(&&long[index]) {
                kept.next();
                index += 1;
                continue;
            }
            let start = index;
            while index < long.len() && kept.peek() != Some(&&long[index]) {
                index += 1;
            }
            runs.push((start, index - start));
        }
        runs
    };
    if subsequence(old, new) {
        return runs(old, new)
            .into_iter()
            .map(|(start, length)| (start..start, length))
            .collect();
    }
    if subsequence(new, old) {
        let mut removed = 0;
        return runs(new, old)
            .into_iter()
            .map(|(start, length)| {
                let at = start - removed;
                removed += length;
                (at..at + length, 0)
            })
            .collect();
    }
    let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
    let suffix = old[prefix..]
        .iter()
        .rev()
        .zip(new[prefix..].iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    vec![(prefix..old.len() - suffix, new.len() - prefix - suffix)]
}

impl Workbench {
    /// Rebuilds the rows from scratch, for a new search.
    pub(super) fn reset_search_rows(&mut self) {
        self.search_rows = self.results.rows(&self.collapsed_groups);
        self.search_scroll.set_rows(&self.search_rows);
        self.search_scroll.list.reset(self.search_rows.len());
    }

    /// Brings the rows up to date, changing only the rows that appeared or disappeared so the
    /// list keeps its scroll position.
    pub(super) fn update_search_rows(&mut self) {
        let rows = self.results.rows(&self.collapsed_groups);
        let old = std::mem::replace(&mut self.search_rows, rows);
        self.search_scroll.set_rows(&self.search_rows);
        // The list keeps its top row in view when rows are inserted above it. At the very start
        // of the results, stay at the start instead, so newer sessions show up rather than
        // pushing the view down.
        let top = self.search_scroll.list.logical_scroll_top();
        let at_start = top.item_ix == 0 && top.offset_in_item == px(0.);
        for (range, count) in row_splices(&old, &self.search_rows) {
            self.search_scroll.list.splice(range, count);
        }
        if at_start {
            self.search_scroll.list.scroll_to(ListOffset {
                item_ix: 0,
                offset_in_item: px(0.),
            });
        }
    }

    fn toggle_group(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(key) {
            self.collapsed_groups.insert(key.to_owned());
        }
        self.update_search_rows();
        cx.notify();
    }

    /// Expands the group of the hit and scrolls its row into view.
    pub(super) fn reveal_search_hit(&mut self, index: usize) {
        let Some(hit) = self.hits.get(index) else {
            return;
        };
        if self.collapsed_groups.remove(&hit.session) {
            self.update_search_rows();
        }
        if let Some(row) = self.search_rows.iter().position(|row| match row {
            SearchRow::Session { title, .. } => *title == Some(index),
            SearchRow::Hit(hit) => *hit == index,
        }) {
            self.search_scroll.list.scroll_to_reveal_item(row);
        }
    }

    fn highlighted(&self, text: String, cx: &App) -> StyledText {
        let style = HighlightStyle {
            background_color: Some(cx.theme().warning.opacity(0.35)),
            ..Default::default()
        };
        let ranges = find_ranges(&text, &self.query_text);
        StyledText::new(text).with_highlights(ranges.into_iter().map(|range| (range, style)))
    }

    pub(super) fn render_search_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        match self.search_rows.get(index) {
            Some(SearchRow::Session {
                key,
                title,
                first,
                hits,
                collapsed,
            }) => self.render_group_header(index, key, *title, *first, *hits, *collapsed, cx),
            Some(SearchRow::Hit(hit)) => self.render_hit_row(*hit, cx),
            None => div().into_any_element(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn render_group_header(
        &self,
        row: usize,
        key: &str,
        title: Option<usize>,
        first: usize,
        hits: usize,
        collapsed: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = self.sessions.get(key).cloned();
        let name = self
            .hits
            .get(first)
            .map(|hit| hit.session_name.clone())
            .filter(|name| !name.is_empty())
            .or_else(|| session.as_ref().map(|session| session.name.clone()))
            .unwrap_or_else(|| "Session".into());
        let name = if title.is_some() {
            self.highlighted(name, cx).into_any_element()
        } else {
            name.into_any_element()
        };
        let meta = [
            (hits > 0).then(|| {
                if hits >= SEARCH_HIT_LIMIT {
                    format!("{hits}+")
                } else {
                    hits.to_string()
                }
            }),
            session
                .as_ref()
                .map(|session| SessionTable::date_label(session, self.settings.mode)),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        let selected = title.is_some_and(|title| title == self.hit_index);
        let toggle_key = key.to_owned();
        h_flex()
            .id(("search-group", row))
            .w_full()
            .h(HEADER_HEIGHT)
            .flex_shrink_0()
            .overflow_hidden()
            .pl_1()
            .pr_3()
            .py_1()
            .gap_1()
            .when(row > 0, |header| {
                header.border_t_1().border_color(cx.theme().border)
            })
            .cursor_pointer()
            .when(selected, |header| header.bg(cx.theme().list_active))
            .hover(|style| style.bg(cx.theme().list_hover))
            .child(div().w(px(22.)).flex_shrink_0().when(hits > 0, |slot| {
                slot.child(
                    Button::new(("search-toggle", row))
                        .icon(if collapsed {
                            IconName::ChevronRight
                        } else {
                            IconName::ChevronDown
                        })
                        .ghost()
                        .xsmall()
                        .tooltip(if collapsed { "Show hits" } else { "Hide hits" })
                        .on_click(cx.listener(move |this, _, _, cx| {
                            cx.stop_propagation();
                            this.toggle_group(&toggle_key, cx);
                        })),
                )
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(name),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(meta),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                let permanent = event.click_count() >= 2;
                if let Some(session) = session.clone() {
                    this.focus_document = permanent;
                    this.open_session(session, permanent, window, cx);
                }
            }))
            .into_any_element()
    }

    fn render_hit_row(&self, index: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(hit) = self.hits.get(index).cloned() else {
            return div().into_any_element();
        };
        let snippet = self.highlighted(hit.snippet.clone(), cx);
        h_flex()
            .id(("search-hit", index))
            .w_full()
            .h(HIT_HEIGHT)
            .flex_shrink_0()
            .overflow_hidden()
            .pl_6()
            .pr_3()
            .py_1()
            .gap_2()
            .cursor_pointer()
            .when(index == self.hit_index, |row| {
                row.bg(cx.theme().list_active)
            })
            .hover(|style| style.bg(cx.theme().list_hover))
            .child(
                div()
                    .w(px(2.))
                    .flex_shrink_0()
                    .self_stretch()
                    .rounded_sm()
                    .bg(role_accent(hit.role)),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("#{} · {}", hit.ordinal + 1, hit.role.label())),
                    )
                    .child(div().text_sm().line_clamp(2).child(snippet)),
            )
            .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                this.hit_index = index;
                this.open_hit(hit.clone(), event.click_count() >= 2, window, cx)
            }))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{HEADER_HEIGHT, HIT_HEIGHT, SearchResults, SearchRow, SearchScroll, row_splices};
    use agent_chat_reader_core::{
        model::Role,
        search::{HitKind, SearchHit},
    };
    use chrono::{DateTime, TimeZone, Utc};
    use gpui_kit::{ListOffset, component::scroll::ScrollbarHandle, px};
    use std::collections::HashSet;

    fn hit(session: &str, kind: HitKind) -> SearchHit {
        SearchHit {
            session: session.into(),
            message_id: 0,
            ordinal: 0,
            role: Role::User,
            snippet: String::new(),
            offset: 0,
            session_name: String::new(),
            workspace: None,
            document_snapshot: false,
            kind,
        }
    }

    fn header(key: &str, title: Option<usize>, first: usize, hits: usize) -> SearchRow {
        SearchRow::Session {
            key: key.into(),
            title,
            first,
            hits,
            collapsed: false,
        }
    }

    /// Session "a" is the newest and "c" the oldest.
    fn time(key: &str) -> Option<DateTime<Utc>> {
        let day = match key {
            "a" => 3,
            "b" => 2,
            "c" => 1,
            _ => return None,
        };
        Some(Utc.with_ymd_and_hms(2026, 1, day, 0, 0, 0).unwrap())
    }

    #[test]
    fn sessions_are_placed_by_time_as_they_arrive_and_keep_the_title_in_the_header() {
        let mut hits = vec![hit("b", HitKind::Title), hit("b", HitKind::Message)];
        let mut results = SearchResults::default();
        results.add(&hits, 0, time);
        // Later arrivals land before and after the existing group.
        hits.extend([
            hit("c", HitKind::Message),
            hit("a", HitKind::Message),
            hit("a", HitKind::Message),
        ]);
        results.add(&hits, 2, time);
        assert_eq!(
            results.rows(&HashSet::new()),
            [
                header("a", None, 3, 2),
                SearchRow::Hit(3),
                SearchRow::Hit(4),
                header("b", Some(0), 0, 1),
                SearchRow::Hit(1),
                header("c", None, 2, 1),
                SearchRow::Hit(2),
            ]
        );
        assert_eq!(results.order(), [3, 4, 0, 1, 2]);
        assert_eq!(results.sessions(), 3);
        let collapsed = HashSet::from(["a".to_owned()]);
        assert_eq!(results.rows(&collapsed)[1], header("b", Some(0), 0, 1));
    }

    #[test]
    fn row_splices_insert_and_remove_only_the_changed_runs() {
        let old = [header("b", None, 0, 1), SearchRow::Hit(0)];
        let new = [
            header("a", None, 2, 1),
            SearchRow::Hit(2),
            header("b", None, 0, 1),
            SearchRow::Hit(0),
            header("c", None, 1, 0),
        ];
        assert_eq!(row_splices(&old, &new), [(0..0, 2), (4..4, 1)]);
        assert_eq!(row_splices(&new, &old), [(0..2, 0), (2..3, 0)]);
        let collapsed = [SearchRow::Session {
            key: "b".into(),
            title: None,
            first: 0,
            hits: 1,
            collapsed: true,
        }];
        assert_eq!(row_splices(&old, &collapsed), [(0..2, 1)]);
    }

    #[test]
    fn the_scroll_extent_and_offset_come_from_the_fixed_row_heights() {
        let scroll = SearchScroll::new();
        let rows = [
            header("a", None, 0, 2),
            SearchRow::Hit(0),
            SearchRow::Hit(1),
        ];
        scroll.set_rows(&rows);
        scroll.list.reset(rows.len());
        assert_eq!(
            scroll.content_size().height,
            HEADER_HEIGHT + HIT_HEIGHT + HIT_HEIGHT
        );
        scroll.list.scroll_to(ListOffset {
            item_ix: 2,
            offset_in_item: px(10.),
        });
        assert_eq!(scroll.offset().y, -(HEADER_HEIGHT + HIT_HEIGHT + px(10.)));
    }
}
