use crate::workbench::Workbench;
use agent_chat_reader_core::model::{Session, TimeMode};
use chrono::Local;
use gpui_kit::{
    component::{table::*, tooltip::Tooltip},
    *,
};
use std::{cmp::Ordering, collections::HashMap, sync::Arc};

pub const AGENT_COLUMN: usize = 0;
pub const DATE_COLUMN: usize = 1;
const WORKSPACE_COLUMN: usize = 2;
const DEFAULT_WIDTHS: [f32; 4] = [36., 78., 124., 240.];

pub fn migrate_widths(widths: &[f32]) -> Vec<f32> {
    match widths {
        [_, workspace, name] => vec![DEFAULT_WIDTHS[0], DEFAULT_WIDTHS[1], *workspace, *name],
        [_, date, workspace, name] => vec![DEFAULT_WIDTHS[0], *date, *workspace, *name],
        _ => DEFAULT_WIDTHS.to_vec(),
    }
}

pub struct SessionTable {
    pub rows: Vec<Arc<Session>>,
    pub visible: Vec<usize>,
    pub mode: TimeMode,
    pub filter: String,
    pub days: Option<i64>,
    pub selected: Option<String>,
    pub widths: Vec<f32>,
    pub sort_column: usize,
    pub descending: bool,
    pub owner: WeakEntity<Workbench>,
}

impl SessionTable {
    pub fn update_rows(
        table: &mut TableState<Self>,
        update: impl FnOnce(&mut Self),
        cx: &mut Context<TableState<Self>>,
    ) {
        let scroll = table.vertical_scroll_handle.clone();
        let top = scroll.0.borrow().base_handle.logical_scroll_top().0;
        let anchor = if top > 0 || table.delegate().selected.is_some() {
            table
                .delegate()
                .at(top)
                .map(|session| session.candidate.key.storage_key())
        } else {
            None
        };
        let offset = scroll.0.borrow().base_handle.offset();
        let row_height = scroll
            .0
            .borrow()
            .last_item_size
            .map(|size| size.item.height);
        update(table.delegate_mut());
        table.refresh(cx);
        if let Some(selected) = table.delegate().selected.clone() {
            let index = table.delegate().visible.iter().position(|&index| {
                table.delegate().rows[index].candidate.key.storage_key() == selected
            });
            match index {
                Some(index) if table.selected_row() != Some(index) => {
                    table.set_selected_row(index, cx)
                }
                None => table.clear_selection(cx),
                _ => {}
            }
            table.delegate_mut().selected = Some(selected);
        }
        if let Some(anchor) = anchor
            && let Some(index) = table.delegate().visible.iter().position(|&index| {
                table.delegate().rows[index].candidate.key.storage_key() == anchor
            })
        {
            let mut scroll_state = scroll.0.borrow_mut();
            scroll_state.deferred_scroll_to_item = None;
            if let Some(height) = row_height {
                scroll_state.base_handle.set_offset(point(
                    offset.x,
                    offset.y - height * (index as f32 - top as f32),
                ));
            }
        }
        cx.notify();
    }

    pub fn apply(&mut self, incoming: Vec<Arc<Session>>) {
        let incoming: HashMap<_, _> = incoming
            .into_iter()
            .map(|session| (session.candidate.key.storage_key(), session))
            .collect();
        self.rows
            .retain(|session| !incoming.contains_key(&session.candidate.key.storage_key()));
        let mut incoming: Vec<_> = incoming.into_values().collect();
        incoming.sort_by(|a, b| compare(a, b, self.mode, self.sort_column, self.descending));
        let mut old = std::mem::take(&mut self.rows).into_iter().peekable();
        let mut new = incoming.into_iter().peekable();
        while let (Some(a), Some(b)) = (old.peek(), new.peek()) {
            if compare(a, b, self.mode, self.sort_column, self.descending).is_le() {
                if let Some(session) = old.next() {
                    self.rows.push(session);
                }
            } else if let Some(session) = new.next() {
                self.rows.push(session);
            }
        }
        self.rows.extend(old);
        self.rows.extend(new);
        self.refilter();
    }

    pub fn sort(&mut self) {
        let mode = self.mode;
        let column = self.sort_column;
        let descending = self.descending;
        self.rows
            .sort_by(|a, b| compare(a, b, mode, column, descending));
        self.refilter();
    }

    pub fn refilter(&mut self) {
        let filter = self.filter.to_lowercase();
        let cutoff = self
            .days
            .map(|days| chrono::Utc::now() - chrono::Duration::days(days));
        self.visible = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, session)| {
                (filter.is_empty()
                    || session
                        .workspace
                        .as_deref()
                        .unwrap_or_default()
                        .to_lowercase()
                        .contains(&filter)
                    || session.name.to_lowercase().contains(&filter))
                    && cutoff.is_none_or(|cutoff| {
                        session.time(self.mode).is_some_and(|time| time >= cutoff)
                    })
            })
            .map(|(index, _)| index)
            .collect();
    }

    pub fn at(&self, index: usize) -> Option<Arc<Session>> {
        self.visible
            .get(index)
            .and_then(|index| self.rows.get(*index))
            .cloned()
    }

    pub fn time_label(&self, session: &Session) -> String {
        Self::date_label(session, self.mode)
    }

    fn date_label(session: &Session, mode: TimeMode) -> String {
        session
            .time(mode)
            .map(|time| time.with_timezone(&Local).format("%m-%d").to_string())
            .unwrap_or_else(|| "—".into())
    }

    pub fn time_detail(&self, session: &Session) -> String {
        session
            .time(self.mode)
            .map(|time| {
                let time = time
                    .with_timezone(&Local)
                    .format("%Y-%m-%d %H:%M:%S %:z")
                    .to_string();
                if self.mode == TimeMode::Latest && session.latest.value().is_none() {
                    format!("{time} (created)")
                } else {
                    time
                }
            })
            .unwrap_or_else(|| {
                let time = match self.mode {
                    TimeMode::Latest => &session.latest,
                    TimeMode::FirstAssistant => &session.first_assistant,
                };
                if matches!(time, agent_chat_reader_core::model::MessageTime::Pending) {
                    "Extracting…".into()
                } else {
                    "No timestamp".into()
                }
            })
    }
}

fn compare(a: &Session, b: &Session, mode: TimeMode, column: usize, descending: bool) -> Ordering {
    if column == DATE_COLUMN {
        if descending {
            a.compare(b, mode)
        } else {
            a.time(mode)
                .is_none()
                .cmp(&b.time(mode).is_none())
                .then_with(|| a.time(mode).cmp(&b.time(mode)))
                .then_with(|| a.candidate.key.cmp(&b.candidate.key))
        }
    } else {
        let order = if column == WORKSPACE_COLUMN {
            a.workspace.cmp(&b.workspace)
        } else {
            a.name.cmp(&b.name)
        };
        let order = if descending { order.reverse() } else { order };
        order.then_with(|| a.candidate.key.cmp(&b.candidate.key))
    }
}

impl TableDelegate for SessionTable {
    fn columns_count(&self, _: &App) -> usize {
        4
    }

    fn rows_count(&self, _: &App) -> usize {
        self.visible.len()
    }

    fn column(&self, index: usize, _: &App) -> Column {
        if index == AGENT_COLUMN {
            return Column {
                key: "agent".into(),
                name: "".into(),
                width: px(DEFAULT_WIDTHS[0]),
                min_width: px(DEFAULT_WIDTHS[0]),
                max_width: px(DEFAULT_WIDTHS[0]),
                sort: None,
                resizable: false,
                movable: false,
                ..Default::default()
            };
        }
        Column {
            key: ["agent", "date", "workspace", "name"][index].into(),
            name: ["", "Date", "Workspace", "Session name"][index].into(),
            width: px(self
                .widths
                .get(index)
                .copied()
                .unwrap_or(DEFAULT_WIDTHS[index])),
            min_width: px(if index == DATE_COLUMN { 64. } else { 70. }),
            sort: Some(if index == self.sort_column {
                if self.descending {
                    ColumnSort::Descending
                } else {
                    ColumnSort::Ascending
                }
            } else {
                ColumnSort::Default
            }),
            movable: false,
            ..Default::default()
        }
    }

    fn perform_sort(
        &mut self,
        index: usize,
        sort: ColumnSort,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) {
        let owner = self.owner.clone();
        let _ = owner.update(cx, |this, cx| {
            this.sort_table(index, sort == ColumnSort::Descending, cx)
        });
    }

    fn render_tr(
        &mut self,
        index: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> Stateful<Div> {
        let owner = self.owner.clone();
        let session = self.at(index);
        div()
            .id(("session-row", index))
            .on_click(move |event, window, cx| {
                if let Some(session) = &session {
                    let session = session.clone();
                    let _ = owner.update(cx, |this, cx| {
                        this.open_session(session, event.click_count() >= 2, window, cx);
                    });
                }
            })
    }

    fn render_td(
        &mut self,
        row: usize,
        column: usize,
        _: &mut Window,
        _: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        if column == AGENT_COLUMN {
            let agent = self
                .at(row)
                .map(|session| session.candidate.key.agent.clone())
                .unwrap_or_default();
            let name = agent_chat_reader_core::agents::agent_name(&agent).to_owned();
            let icon = crate::assets::agent_icon(&agent);
            let cell = div()
                .id(("session-agent", row))
                .w_full()
                .h_full()
                .flex()
                .items_center()
                .justify_center()
                .tooltip(move |window, cx| Tooltip::new(name.clone()).build(window, cx));
            return match icon {
                Some(path) => cell
                    .child(img(path).size(px(18.)).object_fit(ObjectFit::Contain))
                    .into_any_element(),
                None => cell.child("?").into_any_element(),
            };
        }
        let detail = self
            .at(row)
            .map(|session| match column {
                DATE_COLUMN => self.time_detail(&session),
                WORKSPACE_COLUMN => session
                    .workspace
                    .clone()
                    .unwrap_or_else(|| "Unknown workspace".into()),
                _ => format!("{}\n{}", session.name, session.candidate.key.native),
            })
            .unwrap_or_default();
        let label = self
            .at(row)
            .map(|session| match column {
                DATE_COLUMN => self.time_label(&session),
                WORKSPACE_COLUMN => session
                    .workspace
                    .as_deref()
                    .and_then(|path| path.rsplit(['\\', '/']).find(|part| !part.is_empty()))
                    .unwrap_or("Unknown workspace")
                    .to_owned(),
                _ => session.name.clone(),
            })
            .unwrap_or_default();
        div()
            .id(("session-cell", row * 4 + column))
            .text_sm()
            .overflow_hidden()
            .text_ellipsis()
            .child(label)
            .tooltip(move |window, cx| Tooltip::new(detail.clone()).build(window, cx))
            .into_any_element()
    }

    fn cell_label(&self, row: usize, column: usize, _: &App) -> Option<SharedString> {
        self.at(row).map(|session| match column {
            AGENT_COLUMN => {
                agent_chat_reader_core::agents::agent_name(&session.candidate.key.agent)
                    .to_owned()
                    .into()
            }
            DATE_COLUMN => self.time_detail(&session).into(),
            WORKSPACE_COLUMN => session
                .workspace
                .clone()
                .unwrap_or_else(|| "Unknown workspace".into())
                .into(),
            _ => session.name.clone().into(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::core::prelude::v1::test;
    use agent_chat_reader_core::model::*;

    fn session(latest: &str, first: &str) -> Session {
        Session {
            candidate: Candidate {
                key: SessionKey {
                    agent: "vscode".into(),
                    source: "source".into(),
                    native: "one".into(),
                },
                events_path: "unused".into(),
                size: 0,
                modified_ns: 0,
                file_identity: None,
                read_mode: ReadMode::Snapshot,
            },
            name: "test".into(),
            workspace: None,
            created: MessageTime::parse(Some("2025-01-01T12:00:00Z")),
            latest: MessageTime::parse(Some(latest)),
            first_assistant: MessageTime::parse(Some(first)),
            indexed: false,
            available: true,
            content_error: None,
        }
    }

    #[test]
    fn compact_dates_keep_selected_time_mode_and_full_time_sorting() {
        let earlier = session("2026-03-10T01:00:00Z", "2026-01-02T01:00:00Z");
        let later = session("2026-03-10T02:00:00Z", "2026-02-02T01:00:00Z");
        let label = SessionTable::date_label(&earlier, TimeMode::Latest);
        assert_eq!(
            label,
            earlier
                .latest
                .value()
                .unwrap()
                .with_timezone(&Local)
                .format("%m-%d")
                .to_string()
        );
        assert_eq!(label.len(), 5);
        assert_eq!(
            SessionTable::date_label(&later, TimeMode::FirstAssistant),
            later
                .first_assistant
                .value()
                .unwrap()
                .with_timezone(&Local)
                .format("%m-%d")
                .to_string()
        );
        assert!(compare(&later, &earlier, TimeMode::Latest, DATE_COLUMN, true).is_lt());
        assert_eq!(
            SessionTable::date_label(&session("bad", ""), TimeMode::FirstAssistant),
            "—"
        );
        assert!(session("bad", "").time(TimeMode::FirstAssistant).is_none());
    }

    #[test]
    fn old_column_preferences_are_shifted_without_reusing_the_wide_date_column() {
        assert_eq!(
            migrate_widths(&[190., 180., 330.]),
            vec![36., 78., 180., 330.]
        );
        assert_eq!(
            migrate_widths(&[36., 84., 180., 330.]),
            vec![36., 84., 180., 330.]
        );
    }
}
