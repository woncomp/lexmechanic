# Message Navigation Rail Redesign

## Goal

Match the reference's compact right-edge stack of dashes and its hover-to-preview, click-to-jump interaction. Keep one marker per message, including assistant replies, so existing jump targets and search hits are still reachable. The screenshot does not establish whether the original control includes assistant replies; keeping them is intentional.

## Visual Design

- Replace the full-height divider and uniform bars with a right-aligned stack centered vertically in the transcript viewport. Keep a roughly 40 px lane so the rail does not cover selectable transcript text. Omit the rail for an empty transcript.
- Use a 12 px row pitch and a 40 px-wide hit area per row. Use right-aligned rounded dashes, 12 px wide for both user and assistant messages. Make current, hovered, or focused dashes 28 px wide and 4 px thick; inactive dashes are 2 px thick. Emphasis never shifts other rows.
- Give unselected dashes a gradient from the lighter of the original user and assistant theme colors to cyan for assistant messages or orange for user messages. Use the role's solid cyan or orange for the current dash.
- Use theme tokens in light and dark mode. Distinguish the current message with a heavier solid dash, keyboard focus with an emphasized dash and preview, and search hits with a warning-colored dot. Hover and focus must not erase the current or search-hit states.
- Show a floating card to the left of the hovered or focused dash, aligned with it and clamped inside the transcript viewport. Use a subtle border and shadow, a maximum width around 300 px, and up to three lines of plain-text preview. Include role and message number for orientation. The card must not reflow the transcript.

## Interaction

- Hover reveals the preview after a short delay (start at 200 ms); keyboard focus reveals it immediately. Hover alone never changes the transcript or current marker. Dismiss on Escape, click, pointer exit, tab switch, or when the anchor scrolls out of view. Keep the card non-interactive so it does not steal pointer events.
- Click a marker, or press Enter/Space on a focused marker, to jump to the message's first row. Keep existing collapse state and preserve message-number, Home/End, and search navigation. Scroll updates the current marker using the same top-visible-message rule as today.
- Give the rail one keyboard tab stop using roving focus: Up/Down moves preview focus without jumping, scrolling the rail to reveal the marker as needed. Give each marker a label such as `User, message 12`, plus current/search-hit state where supported by the toolkit. Keep focus separate from the current transcript position.
- For short chats, center the complete stack. For long chats, cap it to the available transcript height and keep it independently scrollable and virtualized; do not sample messages or shrink the pitch with message count. Follow the active message just enough to keep it visible rather than recentering on every scroll. Defer automatic following while the pointer or keyboard is exploring the rail; resume on exit. Wheel input over an overflowing rail browses markers, not the transcript.

## Implementation Plan

1. **Extract the rail renderer.** Move the inline marker list out of [the document renderer](../crates/app/src/workbench.rs#L1604) into a dedicated `message_rail` module. Keep `uniform_list` and the existing document-owned scroll handle. Replace the [divided right-hand container](../crates/app/src/workbench.rs#L1792) with a centered, height-capped rail. Derive styling from role, current, hover/focus, and search-hit state. Compute search-hit membership for the conversation once per relevant update, rather than scanning hits for each visible marker.
2. **Add previews and focus.** Reuse [`MessageInfo::preview`](../crates/core/src/store.rs#L26), which the reader [already uses for collapsed messages](../crates/app/src/workbench.rs#L1291). Keep it plain text; provide an attachment or empty-message fallback. Start with the project's [existing tooltip pattern](../crates/app/src/session_table.rs#L318); if it cannot support left-side placement, clamping, or focus, use a single anchored overlay outside the clipped list. Scope preview and focus to the active document, key them by stable message ID, and invalidate pending previews on document changes. No new fetches, schema changes, or persisted settings.
3. **Separate jumping from rail following.** Keep [`Document::jump`](../crates/app/src/document.rs#L78) as the marker's transcript navigation path. Replace unconditional rail centering in that method, the [transcript scroll handler](../crates/app/src/workbench.rs#L619), and [search-result reveal](../crates/app/src/workbench.rs#L1066) with a shared follow policy. Preserve search result expansion, within-message offsets, and saved-position restoration. Check that appending batches keeps focus and preview anchored to the same message.
4. **Validate layout and behavior.** Unit-test height/overflow, marker-state precedence, and follow/preview transitions. Run `cargo fmt --all -- --check` and `cargo test --workspace` after implementation. On Windows, use the [smoke script](../scripts/smoke-windows.ps1#L1) and manually test light/dark mode, resizing and display scaling, short and long conversations, search hits, keyboard navigation, collapsed and attachment-only messages, and tab switches while a preview is pending.

## Acceptance Criteria

- The rail looks like a compact stack of right-aligned dashes, not a second scrollbar; there is no full-height divider.
- A preview opens to the left without shifting content; click or keyboard activation jumps to the correct message. Rapid pointer movement and tab switches never show a stale preview.
- Current, hover/focus, and search-hit states can be distinguished when they overlap. Existing transcript navigation, search reveal, and saved positions behave as before.
- A long conversation keeps every message reachable without overlapping hit areas, loading message bodies for previews, or creating one UI element per message.
