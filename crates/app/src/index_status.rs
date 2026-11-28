use super::Workbench;
use agent_chat_reader_core::{
    agents::{agent_name, source_agent},
    scan::ScanProgress,
};
use gpui_kit::{
    assets::IconName as Lucide,
    component::{
        button::{Button, ButtonVariants},
        popover::Popover,
        progress::Progress,
        spinner::Spinner,
        *,
    },
    prelude::FluentBuilder,
    *,
};
use std::collections::BTreeMap;

#[derive(Default)]
struct AgentProgress<'a> {
    agent: &'a str,
    indexed: usize,
    indexable: usize,
    failed: bool,
    running: bool,
}

impl Workbench {
    fn index_totals(&self) -> ScanProgress {
        let mut totals = ScanProgress::default();
        for progress in self.progress.values() {
            totals.discovered += progress.discovered;
            totals.scanned += progress.scanned;
            totals.indexable += progress.indexable;
            totals.indexed += progress.indexed;
            totals.unreadable += progress.unreadable;
            totals.failed += progress.failed;
        }
        totals
    }

    /// Scanned and indexed sessions as a share of all the work found so far.
    fn index_percent(&self, totals: &ScanProgress) -> f32 {
        let all = totals.discovered + totals.indexable;
        if self.catalog_done {
            100.
        } else if all == 0 {
            0.
        } else {
            (totals.scanned + totals.indexed) as f32 * 100. / all as f32
        }
    }

    fn index_phase(&self, totals: &ScanProgress) -> &'static str {
        if self.catalog_done {
            "Ready"
        } else if totals.discovered == 0 {
            "Discovering sessions…"
        } else if totals.scanned < totals.discovered {
            "Scanning sessions…"
        } else {
            "Indexing messages…"
        }
    }

    /// The status bar button for the session index. Its icon spins until the catalog run ends,
    /// and clicking it shows the scan and index details.
    pub(super) fn render_index_button(&self, cx: &mut Context<Self>) -> AnyElement {
        let totals = self.index_totals();
        let tooltip = if self.catalog_done {
            format!("Session index ready · {} sessions", self.sessions.len())
        } else {
            format!(
                "{} {}/{}",
                self.index_phase(&totals),
                totals.scanned + totals.indexed,
                totals.discovered + totals.indexable
            )
        };
        let button = Button::new("index-status")
            .ghost()
            .xsmall()
            .tooltip(tooltip);
        let button = if self.catalog_done {
            button.icon(Icon::new(Lucide::CircleCheck).text_color(cx.theme().success))
        } else {
            // More work can still be found while the run is going, so it never reads 100%.
            let percent = self.index_percent(&totals).floor().min(99.);
            button
                .icon(Spinner::new().icon(Lucide::RefreshCw).ease(linear))
                .label(format!("{percent}%"))
        };
        let owner = cx.weak_entity();
        Popover::new("index-popover")
            .anchor(Anchor::BottomRight)
            .w(px(320.))
            .trigger(button)
            .content(move |_, _, cx| {
                owner
                    .upgrade()
                    .map(|owner| owner.read(cx).render_index_panel(cx))
                    .unwrap_or_else(|| div().into_any_element())
            })
            .into_any_element()
    }

    fn render_index_panel(&self, cx: &App) -> AnyElement {
        let totals = self.index_totals();
        let theme = cx.theme();
        let percent = self.index_percent(&totals);
        let divider = || div().h(px(1.)).w_full().bg(theme.border);
        let row = |label: &'static str, value: String| {
            h_flex()
                .justify_between()
                .child(label)
                .child(div().text_color(theme.muted_foreground).child(value))
        };
        let state_icon = |done: bool, failed: bool| {
            if failed {
                Icon::new(Lucide::TriangleAlert)
                    .small()
                    .text_color(theme.warning)
                    .into_any_element()
            } else if done {
                Icon::new(Lucide::CircleCheck)
                    .small()
                    .text_color(theme.success)
                    .into_any_element()
            } else {
                Spinner::new()
                    .icon(Lucide::RefreshCw)
                    .ease(linear)
                    .small()
                    .into_any_element()
            }
        };
        // One row per agent: the sources of VS Code and VS Code Insiders are added together.
        let mut agents: BTreeMap<&str, AgentProgress<'_>> = BTreeMap::new();
        for (id, progress) in &self.progress {
            let agent = agents.entry(agent_name(source_agent(id))).or_default();
            agent.agent = source_agent(id);
            agent.indexed += progress.indexed;
            agent.indexable += progress.indexable;
            agent.failed |= progress.failed > 0;
            agent.running |= !self.finished.contains(id);
        }
        let dark = theme.is_dark();
        v_flex()
            .gap_2()
            .text_sm()
            .child(
                h_flex()
                    .justify_between()
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Session index"),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .text_color(theme.muted_foreground)
                            .child(state_icon(self.catalog_done, false))
                            .child(self.index_phase(&totals)),
                    ),
            )
            .child(
                Progress::new("index-progress-bar")
                    .value(percent)
                    .accessibility_label("Session index progress"),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme.muted_foreground)
                    .child(format!("{} sessions in the catalog", self.sessions.len())),
            )
            .child(divider())
            .child(row(
                "Scanned",
                format!("{} / {}", totals.scanned, totals.discovered),
            ))
            .child(row(
                "Indexed",
                format!("{} / {}", totals.indexed, totals.indexable),
            ))
            .child(row("Unreadable", totals.unreadable.to_string()))
            .child(row("Failed", totals.failed.to_string()))
            .when(!agents.is_empty(), |panel| {
                panel
                    .child(divider())
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.muted_foreground)
                            .child("Sources"),
                    )
                    .children(agents.into_values().map(|progress| {
                        let agent = progress.agent;
                        h_flex()
                            .gap_2()
                            .child(div().size(px(16.)).flex_shrink_0().when_some(
                                crate::assets::agent_icon(agent, dark),
                                |slot, path| {
                                    slot.child(
                                        img(crate::assets::icon_source(path, 16.)).size(px(16.)),
                                    )
                                },
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(agent_name(agent).to_owned()),
                            )
                            .child(
                                div().text_color(theme.muted_foreground).child(format!(
                                    "{} / {}",
                                    progress.indexed, progress.indexable
                                )),
                            )
                            .child(state_icon(
                                self.catalog_done || !progress.running,
                                progress.failed,
                            ))
                    }))
            })
            .into_any_element()
    }
}
