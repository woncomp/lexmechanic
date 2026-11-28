use super::Workbench;
use agent_chat_reader_core::model::Role;
use gpui_kit::{accesskit, component::ActiveTheme, prelude::FluentBuilder, *};
use std::{collections::HashSet, time::Duration};
use unicode_segmentation::UnicodeSegmentation;

const ROW_HEIGHT: f32 = 12.;

fn marker_width(current: bool, hovered: bool, focused: bool) -> f32 {
    if current || hovered || focused {
        28.
    } else {
        12.
    }
}

pub(super) fn role_accent(role: Role) -> Rgba {
    match role {
        Role::User => rgb(0xff9800),
        Role::Assistant => rgb(0x00bcd4),
    }
}

fn marker_background(
    role: Role,
    current: bool,
    user_color: Hsla,
    assistant_color: Hsla,
    background: Hsla,
) -> Background {
    let accent = role_accent(role);
    if current {
        return accent.into();
    }
    let light = if background.blend(user_color).l <= background.blend(assistant_color).l {
        assistant_color
    } else {
        user_color
    };
    linear_gradient(
        90.,
        linear_color_stop(light, 0.),
        linear_color_stop(accent, 1.),
    )
}

#[derive(Default)]
pub(crate) struct RailState {
    pointer_inside: bool,
    hovered: Option<i64>,
    focus: Option<i64>,
    has_focus: bool,
    focus_preview: bool,
    preview: Option<i64>,
    hover_generation: u64,
}

impl RailState {
    pub fn exploring(&self) -> bool {
        self.pointer_inside || self.has_focus
    }

    pub fn focus(&mut self, message: Option<i64>) {
        self.has_focus = true;
        self.focus = message;
        self.focus_preview = true;
    }

    pub fn blur(&mut self) {
        self.has_focus = false;
        self.focus_preview = false;
    }

    pub fn dismiss_preview(&mut self) -> bool {
        let visible = self.preview.take().is_some() || self.focus_preview;
        self.focus_preview = false;
        self.hover_generation = self.hover_generation.wrapping_add(1);
        visible
    }

    fn enter_message(&mut self, message: i64) -> u64 {
        self.hovered = Some(message);
        self.preview = None;
        self.hover_generation = self.hover_generation.wrapping_add(1);
        self.hover_generation
    }

    fn leave_message(&mut self, message: i64) {
        if self.hovered == Some(message) {
            self.hovered = None;
            self.preview = None;
            self.hover_generation = self.hover_generation.wrapping_add(1);
        }
    }

    fn reveal_hover(&mut self, message: i64, generation: u64) -> bool {
        if self.hovered == Some(message) && self.hover_generation == generation {
            self.preview = Some(message);
            true
        } else {
            false
        }
    }

    fn preview_for(&self, message: i64) -> bool {
        self.preview == Some(message)
            || (self.has_focus
                && self.focus_preview
                && self.focus == Some(message)
                && (self.hovered.is_none() || self.hovered == Some(message)))
    }
}

fn preview_text(preview: &str, attachments: &[String]) -> String {
    let text = preview.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.is_empty() {
        return if attachments.is_empty() {
            "Empty message".into()
        } else {
            format!("Attachment: {}", attachments.join(", "))
        };
    }
    let mut graphemes = text.graphemes(true);
    let shortened = graphemes.by_ref().take(240).collect::<String>();
    if graphemes.next().is_some() {
        format!("{shortened}…")
    } else {
        shortened
    }
}

impl Workbench {
    pub(super) fn render_message_rail(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let document = self.document.as_ref().unwrap();
        let count = document.messages.len();
        let scroll = document.nav_scroll.clone();
        let document_key = document.key.clone();
        let document_generation = self.generation;
        let hits: HashSet<i64> = self
            .hits
            .iter()
            .filter(|hit| Some(&hit.session) == self.tabs.active.as_ref())
            .map(|hit| hit.message_id)
            .collect();
        let markers = uniform_list(
            "message-navigation",
            count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range
                    .filter_map(|index| {
                        let document = this.document.as_ref()?;
                        if document.key != document_key {
                            return None;
                        }
                        let message = document.messages.get(index)?;
                        let message_id = message.info.rowid;
                        let role = message.info.role;
                        let current = index == document.current;
                        let hit = hits.contains(&message_id);
                        let hovered = document.rail.hovered == Some(message_id);
                        let focused =
                            document.rail.has_focus && document.rail.focus == Some(message_id);
                        let show_preview = document.rail.preview_for(message_id);
                        let text = preview_text(&message.info.preview, &message.attachment_names);
                        let label = format!(
                            "{}, message {}{}{}",
                            role.label(),
                            index + 1,
                            if current { ", current" } else { "" },
                            if hit { ", search result" } else { "" }
                        );
                        let key = document.key.clone();
                        let click_key = key.clone();
                        let bar_width = marker_width(current, hovered, focused);
                        let background = marker_background(
                            role,
                            current,
                            cx.theme().muted_foreground,
                            cx.theme().border,
                            cx.theme().background,
                        );
                        Some(
                            div()
                                .id(("nav-message", index))
                                .role(accesskit::Role::ListBoxOption)
                                .aria_label(label.clone())
                                .aria_description(text.clone())
                                .aria_selected(current)
                                .when(focused, |item| item.aria_active_descendant())
                                .relative()
                                .w(px(40.))
                                .h(px(ROW_HEIGHT))
                                .flex()
                                .items_center()
                                .justify_end()
                                .pr_1()
                                .cursor_pointer()
                                .on_hover(cx.listener(move |this, entered: &bool, _, cx| {
                                    if this.generation != document_generation {
                                        return;
                                    }
                                    let Some(document) = &mut this.document else {
                                        return;
                                    };
                                    if document.key != key {
                                        return;
                                    }
                                    if *entered {
                                        let generation = document.rail.enter_message(message_id);
                                        let key = key.clone();
                                        let executor = cx.background_executor().clone();
                                        cx.spawn(async move |this, cx| {
                                            executor.timer(Duration::from_millis(200)).await;
                                            let _ = this.update(cx, |this, cx| {
                                                if this.generation == document_generation
                                                    && let Some(document) = &mut this.document
                                                    && document.key == key
                                                    && document
                                                        .rail
                                                        .reveal_hover(message_id, generation)
                                                {
                                                    cx.notify();
                                                }
                                            });
                                        })
                                        .detach();
                                    } else {
                                        document.rail.leave_message(message_id);
                                    }
                                    cx.notify();
                                }))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    if this.generation == document_generation
                                        && let Some(document) = &mut this.document
                                        && document.key == click_key
                                    {
                                        document.rail.dismiss_preview();
                                        document.rail.focus = Some(message_id);
                                        if let Some(index) = document
                                            .messages
                                            .iter()
                                            .position(|message| message.info.rowid == message_id)
                                        {
                                            document.jump(index);
                                        }
                                    }
                                    cx.notify();
                                }))
                                .when(hit, |item| {
                                    item.child(
                                        div()
                                            .size(px(3.))
                                            .mr_1()
                                            .rounded_full()
                                            .bg(cx.theme().warning),
                                    )
                                })
                                .child(
                                    div()
                                        .w(px(bar_width))
                                        .h(px(if current || hovered || focused {
                                            4.
                                        } else {
                                            2.
                                        }))
                                        .rounded_full()
                                        .bg(background),
                                )
                                .when(show_preview, |item| {
                                    item.child(
                                        div().absolute().left_0().top_0().child(
                                            deferred(
                                                anchored()
                                                    .anchor(Anchor::TopRight)
                                                    .offset(point(px(-8.), px(-8.)))
                                                    .snap_to_window()
                                                    .child(
                                                        div()
                                                            .w(px(300.))
                                                            .p_3()
                                                            .rounded_md()
                                                            .border_1()
                                                            .border_color(cx.theme().border)
                                                            .bg(cx.theme().popover)
                                                            .text_color(
                                                                cx.theme().popover_foreground,
                                                            )
                                                            .shadow_md()
                                                            .child(
                                                                div()
                                                                    .text_xs()
                                                                    .text_color(
                                                                        cx.theme().muted_foreground,
                                                                    )
                                                                    .child(label),
                                                            )
                                                            .child(
                                                                div()
                                                                    .text_sm()
                                                                    .line_clamp(3)
                                                                    .child(text),
                                                            ),
                                                    ),
                                            )
                                            .with_priority(1),
                                        ),
                                    )
                                }),
                        )
                    })
                    .collect()
            }),
        )
        .track_scroll(&scroll)
        .w(px(40.))
        .h(px(count as f32 * ROW_HEIGHT))
        .max_h(relative(1.));

        div()
            .id("message-rail")
            .role(accesskit::Role::ListBox)
            .aria_label("Message navigation")
            .aria_description("Use up and down to preview messages, Enter to jump")
            .track_focus(&self.rail_focus)
            .h_full()
            .min_h_0()
            .w(px(40.))
            .flex()
            .flex_col()
            .justify_center()
            .on_hover(cx.listener(|this, inside: &bool, _, cx| {
                if let Some(document) = &mut this.document {
                    document.rail.pointer_inside = *inside;
                    if !inside {
                        if let Some(message) = document.rail.hovered {
                            document.rail.leave_message(message);
                        }
                        document.follow_current();
                    }
                    cx.notify();
                }
            }))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                let Some(document) = &mut this.document else {
                    return;
                };
                if event.keystroke.modifiers.control
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.platform
                {
                    return;
                }
                let index = document
                    .rail
                    .focus
                    .and_then(|id| {
                        document
                            .messages
                            .iter()
                            .position(|message| message.info.rowid == id)
                    })
                    .unwrap_or(document.current);
                match event.keystroke.key.as_str() {
                    "up" | "down" => {
                        let next = if event.keystroke.key == "up" {
                            index.saturating_sub(1)
                        } else {
                            (index + 1).min(document.messages.len().saturating_sub(1))
                        };
                        document.rail.focus = document
                            .messages
                            .get(next)
                            .map(|message| message.info.rowid);
                        document.rail.focus_preview = true;
                        document
                            .nav_scroll
                            .scroll_to_item(next, ScrollStrategy::Nearest);
                    }
                    "enter" | "space" => {
                        document.rail.dismiss_preview();
                        document.jump(index);
                    }
                    "escape" if document.rail.dismiss_preview() => {}
                    _ => return,
                }
                cx.stop_propagation();
                cx.notify();
            }))
            .child(markers)
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{RailState, marker_background, marker_width, preview_text};
    use agent_chat_reader_core::model::Role;
    use gpui_kit::{Background, hsla, linear_color_stop, linear_gradient, rgb};

    #[test]
    fn marker_colors_follow_role_in_light_and_dark_themes() {
        let dark = hsla(0., 0., 0.2, 1.);
        let light = hsla(0., 0., 0.8, 1.);
        for (user_color, assistant_color, background) in [(dark, light, light), (light, dark, dark)]
        {
            let user = linear_gradient(
                90.,
                linear_color_stop(light, 0.),
                linear_color_stop(rgb(0xff9800), 1.),
            );
            assert_eq!(
                marker_background(
                    Role::Assistant,
                    false,
                    user_color,
                    assistant_color,
                    background
                ),
                linear_gradient(
                    90.,
                    linear_color_stop(light, 0.),
                    linear_color_stop(rgb(0x00bcd4), 1.),
                ),
            );
            assert_eq!(
                marker_background(Role::User, false, user_color, assistant_color, background),
                user
            );
            for (role, accent) in [
                (Role::User, rgb(0xff9800)),
                (Role::Assistant, rgb(0x00bcd4)),
            ] {
                assert_eq!(
                    marker_background(role, true, user_color, assistant_color, background),
                    Background::from(accent)
                );
            }
        }
    }

    #[test]
    fn marker_gradient_orders_translucent_colors_on_the_background() {
        let background = hsla(0., 0., 0., 1.);
        let transparent_white = hsla(0., 0., 1., 0.1);
        let gray = hsla(0., 0., 0.5, 1.);
        assert_eq!(
            marker_background(Role::User, false, transparent_white, gray, background),
            linear_gradient(
                90.,
                linear_color_stop(gray, 0.),
                linear_color_stop(rgb(0xff9800), 1.),
            )
        );
        assert_eq!(
            marker_background(Role::Assistant, false, transparent_white, gray, background),
            linear_gradient(
                90.,
                linear_color_stop(gray, 0.),
                linear_color_stop(rgb(0x00bcd4), 1.),
            )
        );
    }

    #[test]
    fn marker_width_is_role_independent_and_current_matches_hover() {
        assert_eq!(marker_width(false, false, false), 12.);
        assert_eq!(marker_width(false, true, false), 28.);
        assert_eq!(marker_width(false, false, true), 28.);
        assert_eq!(marker_width(true, false, false), 28.);
        assert_eq!(marker_width(true, true, true), 28.);
    }

    #[test]
    fn hover_preview_rejects_stale_timers_and_dismissal() {
        let mut state = RailState::default();
        let first = state.enter_message(10);
        let second = state.enter_message(20);
        assert!(!state.reveal_hover(10, first));
        assert!(state.reveal_hover(20, second));
        assert!(state.dismiss_preview());
        assert!(!state.reveal_hover(20, second));
        state.leave_message(20);
        assert!(!state.preview_for(20));
    }

    #[test]
    fn focus_and_hover_pause_follow_independently() {
        let mut state = RailState::default();
        state.pointer_inside = true;
        state.focus(Some(42));
        assert!(state.exploring());
        assert!(state.preview_for(42));
        state.blur();
        assert!(state.exploring());
        state.pointer_inside = false;
        assert!(!state.exploring());
        assert!(!state.preview_for(42));
    }

    #[test]
    fn preview_is_plain_bounded_and_handles_empty_messages() {
        assert_eq!(preview_text("  first\n  second ", &[]), "first second");
        assert_eq!(preview_text("", &[]), "Empty message");
        assert_eq!(
            preview_text("", &["note.txt".into()]),
            "Attachment: note.txt"
        );
        assert_eq!(
            preview_text(&"😀".repeat(241), &[]),
            format!("{}…", "😀".repeat(240))
        );
    }
}
