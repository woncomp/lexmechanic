use super::{Workbench, message_rail::role_accent};
use crate::session_table::SessionTable;
use agent_chat_reader_core::search::{HitKind, SearchHit, find_ranges};
use gpui_kit::{
    component::{
        button::{Button, ButtonVariants},
        scroll::ScrollbarHandle,
        *,
    },
    prelude::FluentBuilder,
    *,
};
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    rc::Rc,
};

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
        /// Index of the first hit of the session on this page.
        first: usize,
        /// Message hits of the session on this page.
        hits: usize,
        /// Message hits of the session across all pages.
        total: usize,
        continued: bool,
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

/// Groups a page of hits, which arrives in session order, into header and hit rows.
pub(super) fn build_rows(
    hits: &[SearchHit],
    totals: &HashMap<String, usize>,
    collapsed: &HashSet<String>,
    continued_from: Option<&str>,
) -> Vec<SearchRow> {
    let mut rows = Vec::new();
    let mut header = 0;
    for (index, hit) in hits.iter().enumerate() {
        if index == 0 || hits[index - 1].session != hit.session {
            header = rows.len();
            rows.push(SearchRow::Session {
                key: hit.session.clone(),
                title: None,
                first: index,
                hits: 0,
                total: totals.get(&hit.session).copied().unwrap_or_default(),
                continued: index == 0 && continued_from == Some(hit.session.as_str()),
                collapsed: collapsed.contains(&hit.session),
            });
        }
        let SearchRow::Session {
            title,
            hits,
            collapsed,
            ..
        } = &mut rows[header]
        else {
            unreachable!("every group starts with a header");
        };
        if hit.kind == HitKind::Title {
            *title = Some(index);
        } else {
            *hits += 1;
            if !*collapsed {
                rows.push(SearchRow::Hit(index));
            }
        }
    }
    rows
}

impl Workbench {
    pub(super) fn rebuild_search_rows(&mut self) {
        self.search_rows = build_rows(
            &self.hits,
            &self.hit_totals,
            &self.collapsed_groups,
            self.page_tail.as_deref(),
        );
        self.search_scroll.set_rows(&self.search_rows);
        self.search_scroll.list.reset(self.search_rows.len());
    }

    /// Regroups the same hits after a group is collapsed or expanded, replacing only the changed
    /// rows so the list keeps its scroll position.
    fn regroup_search_rows(&mut self) {
        let rows = build_rows(
            &self.hits,
            &self.hit_totals,
            &self.collapsed_groups,
            self.page_tail.as_deref(),
        );
        let old = std::mem::replace(&mut self.search_rows, rows);
        let new = &self.search_rows;
        let prefix = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let suffix = old[prefix..]
            .iter()
            .rev()
            .zip(new[prefix..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        self.search_scroll.set_rows(new);
        self.search_scroll
            .list
            .splice(prefix..old.len() - suffix, new.len() - prefix - suffix);
    }

    fn toggle_group(&mut self, key: &str, cx: &mut Context<Self>) {
        if !self.collapsed_groups.remove(key) {
            self.collapsed_groups.insert(key.to_owned());
        }
        self.regroup_search_rows();
        cx.notify();
    }

    /// Expands the group of the hit and scrolls its row into view.
    pub(super) fn reveal_search_hit(&mut self, index: usize) {
        let Some(hit) = self.hits.get(index) else {
            return;
        };
        if self.collapsed_groups.remove(&hit.session) {
            self.regroup_search_rows();
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
                total,
                continued,
                collapsed,
            }) => self.render_group_header(
                index, key, *title, *first, *hits, *total, *continued, *collapsed, cx,
            ),
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
        total: usize,
        continued: bool,
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
            (total > 0).then(|| total.to_string()),
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
            .when(continued, |header| {
                header.child(
                    div()
                        .flex_shrink_0()
                        .text_xs()
                        .italic()
                        .text_color(cx.theme().muted_foreground)
                        .child("continued"),
                )
            })
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
    use super::{HEADER_HEIGHT, HIT_HEIGHT, SearchRow, SearchScroll, build_rows};
    use agent_chat_reader_core::{
        model::Role,
        search::{HitKind, SearchHit},
    };
    use gpui_kit::{ListOffset, component::scroll::ScrollbarHandle, px};
    use std::collections::{HashMap, HashSet};

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
            total: 0,
            continued: false,
            collapsed: false,
        }
    }

    #[test]
    fn hits_group_under_their_session_with_the_title_hit_in_the_header() {
        let hits = [
            hit("a", HitKind::Title),
            hit("a", HitKind::Message),
            hit("a", HitKind::Message),
            hit("b", HitKind::Title),
            hit("c", HitKind::Message),
        ];
        let rows = build_rows(&hits, &HashMap::new(), &HashSet::new(), None);
        assert_eq!(
            rows,
            [
                header("a", Some(0), 0, 2),
                SearchRow::Hit(1),
                SearchRow::Hit(2),
                header("b", Some(3), 3, 0),
                header("c", None, 4, 1),
                SearchRow::Hit(4),
            ]
        );
    }

    #[test]
    fn collapsed_groups_drop_hit_rows_and_a_carried_over_group_is_marked() {
        let hits = [
            hit("a", HitKind::Message),
            hit("a", HitKind::Message),
            hit("b", HitKind::Message),
        ];
        let totals = HashMap::from([("a".to_owned(), 7)]);
        let collapsed = HashSet::from(["a".to_owned()]);
        let rows = build_rows(&hits, &totals, &collapsed, Some("a"));
        assert_eq!(
            rows,
            [
                SearchRow::Session {
                    key: "a".into(),
                    title: None,
                    first: 0,
                    hits: 2,
                    total: 7,
                    continued: true,
                    collapsed: true,
                },
                header("b", None, 2, 1),
                SearchRow::Hit(2),
            ]
        );
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
