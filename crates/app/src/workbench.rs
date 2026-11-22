use crate::{
    document::{CachedChunk, Document},
    persistence::{ReadingPosition, Settings},
    runtime::{Event, Runtime},
    session_table::{DATE_COLUMN, SessionTable, migrate_widths},
    tabs::Tabs,
};
use agent_chat_reader_core::{
    model::{Cancellation, Role, Session, TimeMode},
    scan::{ScanEvent, ScanProgress},
    search::{SearchCursor, SearchHit, SearchQuery},
};
use gpui_kit::{
    component::{
        button::Button,
        input::{Input, InputEvent, InputState},
        menu::{ContextMenuExt, PopupMenuItem},
        resizable::{ResizableState, h_resizable, resizable_panel},
        table::{DataTable, TableEvent, TableState},
        text::{RangeHighlight, TextView, TextViewState},
        *,
    },
    prelude::FluentBuilder,
    *,
};
use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

actions!(
    reader,
    [
        FindAll,
        FindDocument,
        CloseTab,
        ReopenTab,
        NextTab,
        PreviousTab,
        OpenSelected,
        PreviewSelected,
        NextMessage,
        PreviousMessage,
        Escape,
        Refresh,
        NextHit,
        PreviousHit
    ]
);

#[derive(Clone)]
struct DraggedTab {
    key: String,
    title: String,
}

impl Render for DraggedTab {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .p_3()
            .bg(cx.theme().muted)
            .border_1()
            .border_color(cx.theme().border)
            .child(self.title.clone())
    }
}

struct RawView {
    parts: Vec<SharedString>,
    states: HashMap<usize, Entity<TextViewState>>,
    list: ListState,
}

pub struct Workbench {
    runtime: Runtime,
    pub table: Entity<TableState<SessionTable>>,
    sessions: HashMap<String, Arc<Session>>,
    tabs: Tabs,
    document: Option<Document>,
    settings: Settings,
    search_input: Entity<InputState>,
    filter_input: Entity<InputState>,
    find_input: Entity<InputState>,
    jump_input: Entity<InputState>,
    focus: FocusHandle,
    doc_focus: FocusHandle,
    focus_document: bool,
    generation: u64,
    doc_cancel: Cancellation,
    query_generation: u64,
    query_cancel: Cancellation,
    query_due: Option<Instant>,
    query_local: bool,
    query_text: String,
    hits: Vec<SearchHit>,
    hits_scanned: usize,
    hits_complete: bool,
    more_hits: Option<SearchCursor>,
    hit_index: usize,
    search_mode: bool,
    find_visible: bool,
    show_settings: bool,
    show_diagnostics: bool,
    diagnostics: Vec<String>,
    status: String,
    progress: HashMap<String, ScanProgress>,
    catalog_run: u64,
    search_state: String,
    active_query: Option<SearchQuery>,
    finished: HashSet<String>,
    reclassified: HashSet<String>,
    save_due: Option<Instant>,
    restore_pending: Option<String>,
    jump_pending: Option<(i64, usize, usize)>,
    settings_path: PathBuf,
    raw_view: Option<RawView>,
    role_filter: Option<Role>,
    workspace_scope: bool,
    date_filter: Option<i64>,
    find_collapsed: Option<(String, HashSet<String>)>,
    _subscriptions: Vec<Subscription>,
}

impl Workbench {
    pub fn new(directory: PathBuf, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Search all conversations  ·  Ctrl+Shift+F")
        });
        let filter_input = cx
            .new(|cx| InputState::new(window, cx).placeholder("Filter workspace or session name"));
        let find_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("Find in conversation  ·  Enter / F3")
        });
        let jump_input = cx.new(|cx| InputState::new(window, cx).placeholder("Message #"));
        let owner = cx.weak_entity();
        let table = cx.new(|cx| {
            TableState::new(
                SessionTable {
                    rows: Vec::new(),
                    visible: Vec::new(),
                    mode: TimeMode::Latest,
                    filter: String::new(),
                    days: None,
                    selected: None,
                    widths: Vec::new(),
                    sort_column: DATE_COLUMN,
                    descending: true,
                    owner,
                },
                window,
                cx,
            )
            .row_header(false)
            .col_movable(false)
        });
        let mut subscriptions = Vec::new();
        subscriptions.push(cx.observe_window_appearance(window, |this, window, cx| {
            if this.settings.dark.is_none() {
                Theme::sync_system_appearance(Some(window), cx);
            }
        }));
        subscriptions.push(cx.on_release(|this, _| {
            this.save_position();
            this.settings.tabs = this
                .tabs
                .items
                .iter()
                .filter(|tab| tab.permanent)
                .cloned()
                .collect();
            this.settings.active = this
                .tabs
                .active
                .clone()
                .filter(|key| this.settings.tabs.iter().any(|tab| &tab.key == key));
            this.doc_cancel.cancel();
            this.query_cancel.cancel();
            if let Err(error) = this.runtime.shutdown(this.settings.clone()) {
                tracing::error!(%error, "Cannot save final reading state");
                if let Err(error) = crate::persistence::save(&this.settings_path, &this.settings) {
                    tracing::error!(%error, "Cannot preserve recovery reading state");
                }
            }
        }));
        subscriptions.push(
            cx.subscribe_in(&search_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Change | InputEvent::PressEnter { .. }) {
                    this.query_local = false;
                    this.queue_search(cx);
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(
                &find_input,
                window,
                |this, _, event, window, cx| match event {
                    InputEvent::Change => {
                        this.query_local = true;
                        this.queue_search(cx);
                    }
                    InputEvent::PressEnter { .. } => this.step_hit(false, window, cx),
                    _ => {}
                },
            ),
        );
        subscriptions.push(
            cx.subscribe_in(&jump_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::PressEnter { .. }) {
                    let value = this.jump_input.read(cx).value();
                    if let Ok(number) = value.trim().parse::<usize>() {
                        if let Some(document) = &mut this.document {
                            if number > 0 && number <= document.messages.len() {
                                document.jump(number - 1);
                            } else {
                                this.status = "Message number is outside this conversation".into();
                            }
                        }
                    } else {
                        this.status = "Enter a valid message number".into();
                    }
                    cx.notify();
                }
            }),
        );
        subscriptions.push(
            cx.subscribe_in(&filter_input, window, |this, _, event, _, cx| {
                if matches!(event, InputEvent::Change) {
                    let value = this.filter_input.read(cx).value().to_string();
                    this.table.update(cx, |table, cx| {
                        SessionTable::update_rows(
                            table,
                            |data| {
                                data.filter = value;
                                data.refilter();
                            },
                            cx,
                        );
                    });
                }
            }),
        );
        subscriptions.push(cx.subscribe_in(
            &table,
            window,
            |this, table, event, _, cx| match event {
                TableEvent::SelectRow(index) => {
                    let key = table
                        .read(cx)
                        .delegate()
                        .at(*index)
                        .map(|session| session.candidate.key.storage_key());
                    this.table
                        .update(cx, |table, _| table.delegate_mut().selected = key);
                }
                TableEvent::ColumnWidthsChanged(widths) => {
                    this.settings.column_widths =
                        widths.iter().map(|width| f32::from(*width)).collect();
                    this.table.update(cx, |table, _| {
                        table.delegate_mut().widths = this.settings.column_widths.clone();
                    });
                    this.save_due = Some(Instant::now() + Duration::from_secs(1));
                }
                _ => {}
            },
        ));
        cx.bind_keys([
            KeyBinding::new("ctrl-shift-f", FindAll, Some("AgentChatReader")),
            KeyBinding::new("ctrl-f", FindDocument, Some("AgentChatReader")),
            KeyBinding::new("ctrl-w", CloseTab, Some("AgentChatReader")),
            KeyBinding::new("ctrl-shift-t", ReopenTab, Some("AgentChatReader")),
            KeyBinding::new("ctrl-tab", NextTab, Some("AgentChatReader")),
            KeyBinding::new("ctrl-shift-tab", PreviousTab, Some("AgentChatReader")),
            KeyBinding::new("enter", OpenSelected, Some("DataTable")),
            KeyBinding::new("space", PreviewSelected, Some("DataTable")),
            KeyBinding::new("alt-down", NextMessage, Some("ReaderDocument")),
            KeyBinding::new("alt-up", PreviousMessage, Some("ReaderDocument")),
            KeyBinding::new("escape", Escape, Some("AgentChatReader")),
            KeyBinding::new("f5", Refresh, Some("AgentChatReader")),
            KeyBinding::new("f3", NextHit, Some("AgentChatReader")),
            KeyBinding::new("shift-f3", PreviousHit, Some("AgentChatReader")),
        ]);
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            loop {
                executor.timer(Duration::from_millis(50)).await;
                if this.update(cx, |this, cx| this.poll(cx)).is_err() {
                    break;
                }
            }
        })
        .detach();
        let settings_path = directory.join("settings.json");
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self {
            runtime: Runtime::new(directory),
            table,
            sessions: HashMap::new(),
            tabs: Tabs::default(),
            document: None,
            settings: Settings::default(),
            search_input,
            filter_input,
            find_input,
            jump_input,
            focus,
            doc_focus: cx.focus_handle(),
            focus_document: false,
            generation: 0,
            doc_cancel: Cancellation::default(),
            query_generation: 0,
            query_cancel: Cancellation::default(),
            query_due: None,
            query_local: false,
            query_text: String::new(),
            hits: Vec::new(),
            hits_scanned: 0,
            hits_complete: true,
            more_hits: None,
            hit_index: 0,
            search_mode: false,
            find_visible: false,
            show_settings: false,
            show_diagnostics: false,
            diagnostics: Vec::new(),
            status: "Discovering Copilot CLI, Copilot App and VS Code sessions…".into(),
            progress: HashMap::new(),
            catalog_run: 0,
            search_state: String::new(),
            active_query: None,
            finished: HashSet::new(),
            reclassified: HashSet::new(),
            save_due: None,
            restore_pending: None,
            jump_pending: None,
            settings_path,
            raw_view: None,
            role_filter: None,
            workspace_scope: false,
            date_filter: None,
            find_collapsed: None,
            _subscriptions: subscriptions,
        }
    }

    fn report(&mut self, result: anyhow::Result<()>) {
        if let Err(error) = result {
            self.error(format!("{error:#}"));
        }
    }

    fn error(&mut self, message: String) {
        tracing::error!(message = %message, "Reader operation failed");
        self.status = message.clone();
        self.diagnostics.push(message);
        if self.diagnostics.len() > 200 {
            self.diagnostics.remove(0);
        }
    }

    fn poll(&mut self, cx: &mut Context<Self>) {
        let started = Instant::now();
        let mut changed = Vec::new();
        let mut notify = false;
        for _ in 0..64 {
            let Ok(event) = self
                .runtime
                .events
                .try_recv()
                .or_else(|_| self.runtime.catalog_events.try_recv())
            else {
                break;
            };
            notify = true;
            match event {
                Event::Ready {
                    sessions,
                    mut settings,
                } => {
                    settings.column_widths = migrate_widths(&settings.column_widths);
                    if let Some(dark) = settings.dark {
                        Theme::change(
                            if dark {
                                ThemeMode::Dark
                            } else {
                                ThemeMode::Light
                            },
                            None,
                            cx,
                        );
                    } else {
                        Theme::sync_system_appearance(None, cx);
                    }
                    self.restore_pending = settings
                        .active
                        .clone()
                        .filter(|key| settings.tabs.iter().any(|tab| &tab.key == key));
                    for tab in &settings.tabs {
                        self.tabs.open(tab.key.clone(), tab.title.clone(), true);
                    }
                    self.table.update(cx, |table, cx| {
                        table.delegate_mut().mode = settings.mode;
                        table.delegate_mut().widths = settings.column_widths.clone();
                        table.refresh(cx);
                    });
                    self.settings = settings;
                    changed.extend(sessions.into_iter().map(Arc::new));
                }
                Event::Cached(sessions) => changed.extend(sessions.into_iter().map(Arc::new)),
                Event::CatalogStarted { run_id } => {
                    if run_id >= self.catalog_run {
                        self.catalog_run = run_id;
                        self.progress.clear();
                        self.finished.clear();
                    }
                }
                Event::Scan {
                    run_id,
                    event: ScanEvent::Reclassified(replacement),
                } if run_id == self.catalog_run => {
                    let old = replacement.previous.storage_key();
                    let new = replacement.current.storage_key();
                    self.save_position();
                    self.reclassified.insert(old.clone());
                    self.reclassified.remove(&new);
                    self.sessions.remove(&old);
                    changed.retain(|session: &Arc<Session>| {
                        session.candidate.key.storage_key() != old
                    });
                    if let Some(position) = self.settings.positions.remove(&old) {
                        self.settings
                            .positions
                            .entry(new.clone())
                            .or_insert(position);
                    }
                    if self.tabs.active.as_deref() == Some(&old) {
                        self.doc_cancel.cancel();
                        self.generation += 1;
                        self.document = None;
                        self.raw_view = None;
                        self.restore_pending = Some(new.clone());
                    }
                    self.tabs.replace_key(&old, &new);
                    self.table.update(cx, |table, cx| {
                        SessionTable::update_rows(
                            table,
                            |data| {
                                data.rows
                                    .retain(|session| session.candidate.key.storage_key() != old);
                                if data.selected.as_deref() == Some(&old) {
                                    data.selected = Some(new);
                                }
                                data.refilter();
                            },
                            cx,
                        )
                    });
                    self.save_due = Some(Instant::now() + Duration::from_secs(1));
                }
                Event::Scan {
                    run_id,
                    event: ScanEvent::Session(session),
                } if run_id == self.catalog_run => {
                    if self
                        .reclassified
                        .contains(&session.candidate.key.storage_key())
                    {
                        continue;
                    }
                    changed.push(Arc::new(*session));
                }
                Event::Scan {
                    run_id,
                    event: ScanEvent::Diagnostic(diagnostic),
                } if run_id == self.catalog_run => self.error(format!(
                    "{}{}: {}",
                    diagnostic.path.display(),
                    diagnostic
                        .offset
                        .map(|offset| format!(" at byte {offset}"))
                        .unwrap_or_default(),
                    diagnostic.description
                )),
                Event::Scan {
                    run_id,
                    event: ScanEvent::Progress { source, progress },
                } if run_id == self.catalog_run => {
                    self.progress.insert(source, progress);
                }
                Event::Scan {
                    run_id,
                    event: ScanEvent::Finished { source, .. },
                } if run_id == self.catalog_run => {
                    self.finished.insert(source);
                }
                Event::CatalogFinished { run_id } if run_id == self.catalog_run => {
                    self.status = if self.diagnostics.is_empty() {
                        "Ready · local main conversations · newest modified sessions scanned first"
                            .into()
                    } else {
                        format!(
                            "Scan finished with {} notices · open details",
                            self.diagnostics.len()
                        )
                    };
                }
                Event::SearchState {
                    generation,
                    message,
                } if generation == self.query_generation => {
                    self.search_state = message;
                }
                Event::SearchFailed {
                    generation,
                    message,
                } if generation == self.query_generation => {
                    self.hits_complete = true;
                    self.search_state = format!("搜索失败：{message}");
                    self.error(message);
                }
                Event::OpenHit {
                    generation,
                    hit,
                    permanent,
                } if generation == self.query_generation => {
                    self.focus_document = permanent;
                    self.open_verified_hit(hit, permanent, cx);
                }
                Event::Document {
                    generation,
                    key,
                    messages,
                    complete,
                } if generation == self.generation => {
                    let mut document = self
                        .document
                        .take()
                        .filter(|document| document.key == key && !document.loading)
                        .unwrap_or_else(|| Document::new(key.clone()));
                    let was_loading = document.loading;
                    let old_messages = document.messages.len();
                    let old_rows = document.rows.len();
                    let append_only = old_messages <= messages.len()
                        && document.messages.iter().zip(&messages).all(|(a, b)| {
                            a.info.rowid == b.info.rowid && a.native_id == b.native_id
                        });
                    document.messages = messages;
                    document.complete = complete;
                    document.loading = false;
                    if was_loading && let Some(position) = self.settings.positions.get(&key) {
                        document.collapsed = position.collapsed.iter().cloned().collect();
                    }
                    if append_only && !was_loading {
                        for (index, message) in
                            document.messages.iter().enumerate().skip(old_messages)
                        {
                            let count = if document.collapsed.contains(&message.native_id) {
                                1
                            } else {
                                message.parts.len()
                            };
                            for part in 0..count {
                                document.rows.push(crate::document::DocumentRow {
                                    message: index,
                                    part,
                                });
                            }
                        }
                        document
                            .list
                            .splice(old_rows..old_rows, document.rows.len() - old_rows);
                    } else {
                        document.rebuild();
                    }
                    if was_loading
                        && let Some(position) = self.settings.positions.get(&key)
                        && let Some(index) = document
                            .messages
                            .iter()
                            .position(|message| message.native_id == position.message_id)
                    {
                        document.jump(index);
                        if let Some(row_index) = document
                            .rows
                            .iter()
                            .position(|row| row.message == index && row.part == position.block)
                        {
                            document.list.scroll_to(ListOffset {
                                item_ix: row_index,
                                offset_in_item: px(position.offset),
                            });
                        }
                    }
                    if let Some((rowid, ordinal, offset)) = self.jump_pending.take()
                        && !Self::jump_to_match(&mut document, rowid, ordinal, offset)
                    {
                        self.status = "Search result changed; run the search again".into();
                    }
                    let owner = cx.weak_entity();
                    document.list.set_scroll_handler(move |event, _, cx| {
                        let _ = owner.update(cx, |this, cx| {
                            if let Some(document) = &mut this.document
                                && let Some(row) = document.rows.get(event.visible_range.start)
                            {
                                document.current = row.message;
                                document
                                    .nav_scroll
                                    .scroll_to_item(row.message, ScrollStrategy::Center);
                            }
                            this.save_due = Some(Instant::now() + Duration::from_secs(1));
                            cx.notify();
                        });
                    });
                    self.document = Some(document);
                    self.save_due = Some(Instant::now() + Duration::from_secs(1));
                }
                Event::DocumentFailed {
                    generation,
                    message,
                } if generation == self.generation => {
                    if let Some(document) = &mut self.document {
                        document.loading = false;
                        document.error = Some(message.clone());
                    }
                    self.error(message);
                }
                Event::Chunk {
                    generation,
                    rowid,
                    part,
                    text,
                } if generation == self.generation => {
                    if let Some(document) = &mut self.document {
                        document.pending.remove(&(rowid, part));
                        let bytes = text.len();
                        let segmented = document
                            .messages
                            .iter()
                            .any(|message| message.info.rowid == rowid && message.parts.len() > 1);
                        let (text, source_prefix) = if segmented {
                            let fence = "`".repeat(
                                text.split(|ch| ch != '`')
                                    .map(str::len)
                                    .max()
                                    .unwrap_or(0)
                                    .max(3)
                                    + 1,
                            );
                            let prefix = format!("{fence}text\n");
                            (format!("{prefix}{text}\n{fence}"), prefix.len())
                        } else {
                            (text, 0)
                        };
                        let state = cx.new(|cx| TextViewState::markdown(&text, cx));
                        document.chunks.insert(
                            (rowid, part),
                            CachedChunk {
                                state,
                                bytes,
                                used: document.tick,
                                highlighted: String::new(),
                                snapshot: None,
                                source_prefix,
                            },
                        );
                        if let Some(index) = document.rows.iter().position(|row| {
                            document.messages[row.message].info.rowid == rowid && row.part == part
                        }) {
                            document.list.splice(index..index + 1, 1);
                        }
                        document.evict();
                    }
                }
                Event::Raw { generation, parts } if generation == self.generation => {
                    let count = parts.len();
                    self.raw_view = Some(RawView {
                        parts: parts.into_iter().map(SharedString::from).collect(),
                        states: HashMap::new(),
                        list: ListState::new(count, ListAlignment::Top, px(300.)),
                    });
                }
                Event::Hits {
                    generation,
                    hits,
                    scanned,
                    complete,
                    more,
                } if generation == self.query_generation => {
                    self.hits.extend(hits);
                    self.hits_scanned = scanned;
                    self.hits_complete = complete;
                    self.more_hits = more;
                    self.search_state.clear();
                }
                Event::Copied(text) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    self.status = "Copied to clipboard".into();
                }
                Event::Notice(message) => {
                    self.status = message;
                }
                Event::Error(message) => self.error(message),
                _ => {}
            }
            if changed.len() >= 256 || started.elapsed() >= Duration::from_millis(4) {
                break;
            }
        }
        if !changed.is_empty() {
            for session in &changed {
                let key = session.candidate.key.storage_key();
                for tab in &mut self.tabs.items {
                    if tab.key == key {
                        tab.title = session.name.clone();
                    }
                }
                self.sessions.insert(key, session.clone());
            }
            self.table.update(cx, |table, cx| {
                SessionTable::update_rows(table, |data| data.apply(changed), cx);
            });
        }
        if let Some(key) = self.restore_pending.clone()
            && let Some(session) = self.sessions.get(&key).cloned()
        {
            self.restore_pending = None;
            let permanent = self
                .tabs
                .items
                .iter()
                .find(|tab| tab.key == key)
                .is_none_or(|tab| tab.permanent);
            self.start_open(session, permanent, cx);
        }
        if self.query_due.is_some_and(|due| Instant::now() >= due) {
            self.query_due = None;
            self.begin_search(cx);
            notify = true;
        }
        if self.save_due.is_some_and(|due| Instant::now() >= due) {
            self.save_due = None;
            self.save_position();
            self.settings.tabs = self
                .tabs
                .items
                .iter()
                .filter(|tab| tab.permanent)
                .cloned()
                .collect();
            self.settings.active = self
                .tabs
                .active
                .clone()
                .filter(|key| self.settings.tabs.iter().any(|tab| &tab.key == key));
            self.report(self.runtime.save(self.settings.clone()));
        }
        if notify {
            cx.notify();
        }
    }

    fn save_position(&mut self) {
        if let Some(document) = &self.document {
            let offset = document.list.logical_scroll_top();
            if let Some(row) = document.rows.get(offset.item_ix) {
                self.settings.positions.insert(
                    document.key.clone(),
                    ReadingPosition {
                        message_id: document.messages[row.message].native_id.clone(),
                        block: row.part,
                        offset: offset.offset_in_item.into(),
                        collapsed: document.collapsed.iter().cloned().collect(),
                    },
                );
            }
        }
    }

    pub fn open_session(
        &mut self,
        session: Arc<Session>,
        permanent: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_open(session, permanent, cx);
        if permanent {
            self.doc_focus.focus(window, cx);
        }
    }

    fn start_open(&mut self, session: Arc<Session>, permanent: bool, cx: &mut Context<Self>) {
        let key = session.candidate.key.storage_key();
        self.tabs.open(key.clone(), session.name.clone(), permanent);
        self.report(
            self.runtime
                .retain_documents(self.tabs.items.iter().map(|tab| tab.key.clone()).collect()),
        );
        self.save_due = Some(Instant::now() + Duration::from_secs(1));
        self.load_document(session, cx);
    }

    fn load_document(&mut self, session: Arc<Session>, cx: &mut Context<Self>) {
        let key = session.candidate.key.storage_key();
        if self
            .document
            .as_ref()
            .is_some_and(|document| document.key == key)
        {
            cx.notify();
            return;
        }
        self.save_position();
        self.jump_pending = None;
        self.raw_view = None;
        self.doc_cancel.cancel();
        self.doc_cancel = Cancellation::default();
        self.generation += 1;
        self.document = Some(Document::new(key));
        self.report(self.runtime.open(
            self.generation,
            (*session).clone(),
            self.doc_cancel.clone(),
        ));
        self.save_due = Some(Instant::now() + Duration::from_secs(1));
        cx.notify();
    }

    fn reload_document(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self
            .tabs
            .active
            .as_ref()
            .and_then(|key| self.sessions.get(key))
            .cloned()
        else {
            return;
        };
        self.save_position();
        self.doc_cancel.cancel();
        self.doc_cancel = Cancellation::default();
        self.generation += 1;
        self.document = Some(Document::new(session.candidate.key.storage_key()));
        self.raw_view = None;
        self.report(self.runtime.reload(
            self.generation,
            (*session).clone(),
            self.doc_cancel.clone(),
        ));
        cx.notify();
    }

    fn activate(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.get(key).cloned() {
            let permanent = self
                .tabs
                .items
                .iter()
                .any(|tab| tab.key == key && tab.permanent);
            self.open_session(session, permanent, window, cx);
        } else {
            self.error("The source for this saved tab is currently unavailable".into());
        }
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.report(self.runtime.refresh(
            self.settings.sources.clone(),
            self.settings.vscode_sources.clone(),
        ));
        self.status = "Refreshing all sources, newest modified sessions first…".into();
        cx.notify();
    }

    fn clear_index(&mut self, cx: &mut Context<Self>) {
        if let Err(error) = self.runtime.clear() {
            self.error(format!("{error:#}"));
            cx.notify();
            return;
        }
        self.save_position();
        self.doc_cancel.cancel();
        self.query_cancel.cancel();
        self.generation += 1;
        self.query_generation += 1;
        self.document = None;
        self.raw_view = None;
        self.sessions.clear();
        self.progress.clear();
        self.finished.clear();
        self.hits.clear();
        self.restore_pending = self.tabs.active.clone();
        self.table.update(cx, |table, cx| {
            table.delegate_mut().rows.clear();
            table.delegate_mut().visible.clear();
            table.delegate_mut().selected = None;
            table.clear_selection(cx);
            table.refresh(cx);
            cx.notify();
        });
        self.status = "Rebuilding local index; original source files are unchanged".into();
        cx.notify();
    }

    fn queue_search(&mut self, cx: &mut Context<Self>) {
        self.query_cancel.cancel();
        self.query_generation += 1;
        self.query_due = Some(Instant::now() + Duration::from_millis(180));
        self.hits.clear();
        self.hits_complete = false;
        self.more_hits = None;
        self.hit_index = usize::MAX;
        self.search_state = "准备搜索…".into();
        cx.notify();
    }

    fn begin_search(&mut self, cx: &mut Context<Self>) {
        self.query_cancel = Cancellation::default();
        self.query_text = if self.query_local {
            self.find_input.read(cx).value().to_string()
        } else {
            self.search_input.read(cx).value().to_string()
        };
        self.search_mode = !self.query_local && !self.query_text.is_empty();
        self.hits.clear();
        self.hits_scanned = 0;
        self.hits_complete = self.query_text.is_empty();
        if self.query_text.is_empty() {
            return;
        }
        self.send_search(SearchCursor::default());
    }

    fn send_search(&mut self, cursor: SearchCursor) {
        if cursor.offset > 0 {
            if let Some(query) = self.active_query.clone() {
                self.report(self.runtime.search(
                    self.query_generation,
                    query,
                    cursor,
                    self.query_cancel.clone(),
                ));
            }
            return;
        }
        let workspace = if self.workspace_scope && !self.query_local {
            self.tabs
                .active
                .as_ref()
                .and_then(|key| self.sessions.get(key))
                .and_then(|session| session.workspace.clone())
        } else {
            None
        };
        let since = if self.query_local {
            None
        } else {
            self.date_filter
                .map(|days| chrono::Utc::now() - chrono::Duration::days(days))
        };
        let query = SearchQuery {
            text: self.query_text.clone(),
            mode: self.settings.mode,
            workspace,
            since,
            role: if self.query_local {
                None
            } else {
                self.role_filter
            },
            session: if self.query_local {
                self.tabs.active.clone()
            } else {
                None
            },
            members: None,
        };
        self.active_query = Some(query.clone());
        self.report(self.runtime.search(
            self.query_generation,
            query,
            cursor,
            self.query_cancel.clone(),
        ));
    }

    fn input_focused(&self, window: &Window, cx: &App) -> bool {
        [
            &self.search_input,
            &self.filter_input,
            &self.find_input,
            &self.jump_input,
        ]
        .into_iter()
        .any(|input| Focusable::focus_handle(input.read(cx), cx).contains_focused(window, cx))
    }

    fn change_font(&mut self, delta: f32, cx: &mut Context<Self>) {
        self.settings.font_size =
            Some((self.settings.font_size.unwrap_or(15.) + delta).clamp(11., 26.));
        if let Some(document) = &mut self.document {
            let anchor = document.list.logical_scroll_top();
            document.list.reset(document.rows.len());
            document.list.scroll_to(anchor);
        }
        self.save_due = Some(Instant::now());
        cx.notify();
    }

    fn close_find(&mut self) {
        self.find_visible = false;
        if let Some((key, collapsed)) = self.find_collapsed.take()
            && let Some(document) = &mut self.document
            && document.key == key
        {
            let anchor = document.list.logical_scroll_top();
            document.collapsed = collapsed;
            document.rebuild();
            document.list.scroll_to(anchor);
        }
    }

    fn next_search_page(&mut self, cx: &mut Context<Self>) {
        if let Some(cursor) = self.more_hits.take() {
            self.hits.clear();
            self.hit_index = usize::MAX;
            self.send_search(cursor);
            cx.notify();
        }
    }

    fn step_hit(&mut self, backwards: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.hits.is_empty() {
            return;
        }
        self.hit_index = if self.hit_index >= self.hits.len() {
            if backwards { self.hits.len() - 1 } else { 0 }
        } else if backwards {
            (self.hit_index + self.hits.len() - 1) % self.hits.len()
        } else {
            (self.hit_index + 1) % self.hits.len()
        };
        let hit = self.hits[self.hit_index].clone();
        self.open_hit(hit, false, window, cx);
    }

    fn jump_to_match(document: &mut Document, rowid: i64, ordinal: usize, offset: usize) -> bool {
        if let Some(message) = document.messages.get(ordinal) {
            if message.info.rowid != rowid {
                return false;
            }
            document.collapsed.remove(&message.native_id);
            let part = message
                .parts
                .iter()
                .position(|range| range.contains(&offset) || range.start == offset)
                .unwrap_or_default();
            document.rebuild();
            if let Some(index) = document
                .rows
                .iter()
                .position(|row| row.message == ordinal && row.part == part)
            {
                document.current = ordinal;
                document.list.scroll_to(ListOffset {
                    item_ix: index,
                    offset_in_item: px(0.),
                });
                document
                    .nav_scroll
                    .scroll_to_item(ordinal, ScrollStrategy::Center);
                document.reveal = Some((
                    document.messages[ordinal].info.rowid,
                    part,
                    offset.saturating_sub(document.messages[ordinal].parts[part].start),
                ));
                return true;
            }
        }
        false
    }

    fn open_hit(
        &mut self,
        hit: SearchHit,
        permanent: bool,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.report(
            self.runtime
                .validate_hit(self.query_generation, hit, permanent),
        );
        cx.notify();
    }

    fn open_verified_hit(&mut self, hit: SearchHit, permanent: bool, cx: &mut Context<Self>) {
        if let Some(session) = self.sessions.get(&hit.session).cloned() {
            self.start_open(session, permanent, cx);
            if let Some(document) = &mut self.document {
                if !document.loading {
                    if !Self::jump_to_match(document, hit.message_id, hit.ordinal, hit.offset) {
                        self.status = "命中属于不同的文档版本，请 Reload 或重新搜索。".into();
                    }
                } else {
                    self.jump_pending = Some((hit.message_id, hit.ordinal, hit.offset));
                }
            }
        }
    }

    fn close(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.save_position();
        self.tabs.close(key);
        self.report(
            self.runtime
                .retain_documents(self.tabs.items.iter().map(|tab| tab.key.clone()).collect()),
        );
        if let Some(key) = self.tabs.active.clone() {
            self.activate(&key, window, cx);
        } else {
            self.doc_cancel.cancel();
            self.document = None;
        }
        self.save_due = Some(Instant::now() + Duration::from_millis(200));
        cx.notify();
    }

    fn change_mode(&mut self, cx: &mut Context<Self>) {
        self.settings.mode = match self.settings.mode {
            TimeMode::Latest => TimeMode::FirstAssistant,
            TimeMode::FirstAssistant => TimeMode::Latest,
        };
        self.table.update(cx, |table, cx| {
            SessionTable::update_rows(
                table,
                |data| {
                    data.mode = self.settings.mode;
                    data.sort_column = DATE_COLUMN;
                    data.descending = true;
                    data.sort();
                },
                cx,
            );
        });
        self.save_due = Some(Instant::now() + Duration::from_secs(1));
        if self.search_mode {
            self.queue_search(cx);
        }
        cx.notify();
    }

    pub fn sort_table(&mut self, column: usize, descending: bool, cx: &mut Context<Self>) {
        // The delegate invokes this while borrowed; defer updating its entity.
        let owner = cx.weak_entity();
        cx.defer(move |cx| {
            let _ = owner.update(cx, |this, cx| {
                this.table.update(cx, |table, cx| {
                    SessionTable::update_rows(
                        table,
                        |data| {
                            data.sort_column = column;
                            data.descending = descending;
                            data.sort();
                        },
                        cx,
                    )
                });
            });
        });
    }

    fn pick_source(&mut self, vscode: bool, cx: &mut Context<Self>) {
        let prompt = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some(
                if vscode {
                    "Select VS Code User directory"
                } else {
                    "Select Copilot home directory (CLI / App)"
                }
                .into(),
            ),
        });
        cx.spawn(async move |this, cx| match prompt.await {
            Ok(Ok(Some(paths))) => {
                let _ = this.update(cx, |this, cx| {
                    for path in paths {
                        let sources = if vscode {
                            this.settings.vscode_sources.get_or_insert_with(Vec::new)
                        } else {
                            &mut this.settings.sources
                        };
                        if !sources.contains(&path) {
                            sources.push(path);
                        }
                    }
                    this.save_due = Some(Instant::now());
                    this.refresh(cx);
                });
            }
            Ok(Ok(None)) => {}
            result => {
                let _ = this.update(cx, |this, cx| {
                    this.error(format!("Cannot select data source: {result:?}"));
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn export(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.tabs.active.clone() else {
            return;
        };
        let directory = std::env::var_os("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let prompt = cx.prompt_for_new_path(&directory, Some("conversation.md"));
        cx.spawn(async move |this, cx| match prompt.await {
            Ok(Ok(Some(path))) => {
                let _ = this.update(cx, |this, _| this.report(this.runtime.export(key, path)));
            }
            Ok(Ok(None)) => {}
            result => {
                let _ = this.update(cx, |this, _| {
                    this.error(format!("Cannot choose export path: {result:?}"))
                });
            }
        })
        .detach();
    }

    fn render_message(
        &mut self,
        index: usize,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(document) = &mut self.document else {
            return div().into_any_element();
        };
        let Some(row) = document.rows.get(index).copied() else {
            return div().into_any_element();
        };
        let message = &document.messages[row.message];
        let rowid = message.info.rowid;
        let part = row.part;
        let native_id = message.native_id.clone();
        let collapsed = document.collapsed.contains(&native_id);
        let role = message.info.role;
        let label = format!(
            "#{}  {}     {}",
            row.message + 1,
            role.label(),
            message
                .info
                .time
                .value()
                .map(|time| time
                    .with_timezone(&chrono::Local)
                    .format("%m-%d %H:%M:%S")
                    .to_string())
                .unwrap_or_else(|| "Unknown time".into())
        );
        let range = message.parts[part].clone();
        let segmented = message.parts.len() > 1;
        let attachments = message.attachment_names.clone();
        let preview = message.info.preview.clone();
        document.tick += 1;
        let content = if collapsed {
            div()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(preview)
                .into_any_element()
        } else if let Some(chunk) = document.chunks.get_mut(&(rowid, part)) {
            chunk.used = document.tick;
            let snapshot = chunk.state.read(cx).rendered_text();
            let query = if self.search_mode || self.find_visible {
                self.query_text.as_str()
            } else {
                ""
            };
            if chunk.highlighted != query || chunk.snapshot.as_ref() != Some(&snapshot) {
                let ranges = agent_chat_reader_core::search::find_ranges(snapshot.source(), query);
                let highlights = ranges
                    .into_iter()
                    .filter_map(|range| snapshot.range_for_source(range))
                    .map(|range| RangeHighlight::new(range, cx.theme().warning.opacity(0.35)))
                    .collect::<Vec<_>>();
                chunk.state.update(cx, |state, cx| {
                    if let Err(error) = state.set_range_highlights(highlights, cx) {
                        tracing::warn!(?error, "Cannot highlight the current text revision");
                    }
                });
                chunk.highlighted = query.to_owned();
                chunk.snapshot = Some(snapshot.clone());
            }
            if let Some((target, target_part, offset)) = document.reveal
                && target == rowid
                && target_part == part
                && !snapshot.is_empty()
            {
                let offset = offset + chunk.source_prefix;
                let end = snapshot
                    .source()
                    .get(offset..)
                    .and_then(|text| text.chars().next())
                    .map_or(offset, |character| offset + character.len_utf8());
                if let Some(range) = snapshot.range_for_source(offset..end) {
                    chunk.state.update(cx, |state, cx| {
                        if let Err(error) = state.reveal_range(range, cx) {
                            tracing::warn!(?error, "Cannot reveal the current text revision");
                        }
                    });
                    document.reveal = None;
                }
            }
            TextView::new(&chunk.state)
                .selectable(true)
                .scrollable(false)
                .image_source(|_| ImageSource::Custom(Arc::new(|_, _| None)))
                .on_link_click(|url, _, _, cx| {
                    if url.starts_with("https://") || url.starts_with("http://") {
                        cx.open_url(url);
                    }
                })
                .code_block_actions(|block, _, _| {
                    let text = block.code().to_string();
                    Button::new("copy-code")
                        .label("Copy")
                        .on_click(move |_, _, cx| {
                            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()))
                        })
                })
                .into_any_element()
        } else {
            if !document.pending.contains(&(rowid, part)) {
                match self.runtime.chunk(self.generation, rowid, part, range) {
                    Ok(()) => {
                        document.pending.insert((rowid, part));
                    }
                    Err(error) => {
                        tracing::warn!(%error, "Chunk request delayed");
                    }
                }
            }
            div()
                .h(px(64.))
                .text_color(cx.theme().muted_foreground)
                .child("Loading message…")
                .into_any_element()
        };
        let mut card = v_flex()
            .id(("message-part", index))
            .w_full()
            .p_5()
            .gap_3()
            .text_size(px(self.settings.font_size.unwrap_or(15.)))
            .border_b_1()
            .border_color(cx.theme().border)
            .when(role == Role::User, |card| {
                card.bg(cx.theme().muted.opacity(0.35))
            });
        if part == 0 {
            card = card.child(
                h_flex()
                    .justify_between()
                    .gap_2()
                    .child(
                        div()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(label),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new(("fold", index))
                                    .label(if collapsed { "Expand" } else { "Fold" })
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        if let Some(document) = &mut this.document {
                                            if !document.collapsed.remove(&native_id) {
                                                document.collapsed.insert(native_id.clone());
                                            }
                                            document.rebuild();
                                            document.jump(row.message);
                                        }
                                        this.save_due =
                                            Some(Instant::now() + Duration::from_secs(1));
                                        cx.notify();
                                    })),
                            )
                            .child(Button::new(("raw-message", index)).label("Raw").on_click(
                                cx.listener(move |this, _, _, _| {
                                    this.report(this.runtime.raw(this.generation, rowid));
                                }),
                            ))
                            .child(Button::new(("copy-message", index)).label("Copy").on_click(
                                cx.listener(move |this, _, _, _| {
                                    this.report(this.runtime.copy(rowid));
                                }),
                            )),
                    ),
            );
        }
        if segmented {
            card = card.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Long message · segment {} · complete text available via Copy / Export",
                        part + 1
                    )),
            );
        }
        card = card.child(content);
        if part == 0 {
            for name in attachments {
                card = card.child(
                    div()
                        .rounded_md()
                        .border_1()
                        .border_color(cx.theme().border)
                        .p_2()
                        .text_sm()
                        .child(format!("Attachment · {name} (not loaded)")),
                );
            }
        }
        card.into_any_element()
    }

    fn render_document(&mut self, cx: &mut Context<Self>) -> AnyElement {
        if let Some(raw) = &self.raw_view {
            let list_state = raw.list.clone();
            let owner = cx.entity();
            return v_flex()
                .size_full()
                .min_h_0()
                .child(
                    h_flex()
                        .p_3()
                        .gap_3()
                        .child(div().flex_1().child("Original message text · read-only"))
                        .child(
                            Button::new("close-raw")
                                .label("Back to conversation")
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.raw_view = None;
                                    cx.notify();
                                })),
                        ),
                )
                .child(
                    list(list_state, move |index, _, cx| {
                        owner.update(cx, |this, cx| {
                            let Some(raw) = &mut this.raw_view else {
                                return div().into_any_element();
                            };
                            let Some(text) = raw.parts.get(index).cloned() else {
                                return div().into_any_element();
                            };
                            if raw.states.len() >= 128
                                && !raw.states.contains_key(&index)
                                && let Some(farthest) = raw
                                    .states
                                    .keys()
                                    .copied()
                                    .max_by_key(|key| key.abs_diff(index))
                            {
                                raw.states.remove(&farthest);
                            }
                            let state = raw.states.entry(index).or_insert_with(|| {
                                let fence = "`".repeat(
                                    text.split(|ch| ch != '`')
                                        .map(str::len)
                                        .max()
                                        .unwrap_or(0)
                                        .max(3)
                                        + 1,
                                );
                                cx.new(|cx| {
                                    TextViewState::markdown(
                                        &format!("{fence}text\n{text}\n{fence}"),
                                        cx,
                                    )
                                })
                            });
                            div()
                                .p_4()
                                .w_full()
                                .child(TextView::new(state).selectable(true).scrollable(false))
                                .into_any_element()
                        })
                    })
                    .size_full(),
                )
                .into_any_element();
        }
        let Some(document) = &self.document else {
            return v_flex()
                .size_full()
                .font_family("Segoe UI")
                .justify_center()
                .items_center()
                .gap_4()
                .child(div().text_2xl().child("Your conversations, within reach."))
                .child(
                    div()
                        .text_color(cx.theme().muted_foreground)
                        .child("Click to preview · Double-click or Enter to keep a tab"),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("Local only · Read-only · Copilot CLI / App / VS Code"),
                )
                .into_any_element();
        };
        if document.loading {
            return div()
                .p_8()
                .child("Opening conversation…")
                .into_any_element();
        }
        if let Some(reason) = &document.error {
            return v_flex()
                .id("unreadable-session")
                .role(gpui_kit::Role::Group)
                .aria_label("无法读取 session 内容")
                .aria_description(reason.clone())
                .size_full()
                .p_8()
                .gap_4()
                .child(div().text_xl().child("无法读取 session 内容"))
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child(reason.clone()),
                )
                .child(div().text_sm().child(
                    "会话元数据仍保留在列表中。可手动 Reload 重试，或 Refresh 更新会话列表。",
                ))
                .child(
                    Button::new("retry-unreadable")
                        .label("Reload")
                        .on_click(cx.listener(|this, _, _, cx| this.reload_document(cx))),
                )
                .into_any_element();
        }
        let list_state = document.list.clone();
        let message_count = document.messages.len();
        let nav_scroll = document.nav_scroll.clone();
        let current = document.current;
        let owner = cx.entity();
        let body = list(list_state, move |index, window, cx| {
            owner.update(cx, |this, cx| this.render_message(index, window, cx))
        })
        .size_full();
        let nav = uniform_list(
            "message-navigation",
            message_count,
            cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                range
                    .map(|index| {
                        let role = this
                            .document
                            .as_ref()
                            .and_then(|document| document.messages.get(index))
                            .map(|message| message.info.role)
                            .unwrap_or(Role::Assistant);
                        let hit = this.hits.iter().any(|hit| {
                            Some(&hit.session) == this.tabs.active.as_ref()
                                && this
                                    .document
                                    .as_ref()
                                    .and_then(|document| document.messages.get(index))
                                    .is_some_and(|message| message.info.rowid == hit.message_id)
                        });
                        div()
                            .id(("nav-message", index))
                            .h(px(18.))
                            .w(px(40.))
                            .p_1()
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                if let Some(document) = &mut this.document {
                                    document.jump(index);
                                }
                                cx.notify();
                            }))
                            .child(
                                div()
                                    .w_full()
                                    .h(px(6.))
                                    .rounded_sm()
                                    .bg(if index == current {
                                        cx.theme().primary
                                    } else if hit {
                                        cx.theme().warning
                                    } else if role == Role::User {
                                        cx.theme().muted_foreground
                                    } else {
                                        cx.theme().border
                                    })
                                    .when(index == current, |bar| {
                                        bar.border_1().border_color(cx.theme().foreground)
                                    }),
                            )
                    })
                    .collect()
            }),
        )
        .track_scroll(&nav_scroll)
        .h_full()
        .w(px(40.));
        v_flex()
            .size_full()
            .min_h_0()
            .key_context("ReaderDocument")
            .track_focus(&self.doc_focus)
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_sm()
                            .child(
                                self.tabs
                                    .active
                                    .as_ref()
                                    .and_then(|key| self.sessions.get(key))
                                    .map(|session| {
                                        let workspace = session
                                            .workspace
                                            .as_deref()
                                            .unwrap_or("Unknown workspace");
                                        if session.available {
                                            workspace.to_owned()
                                        } else {
                                            format!("{workspace} · source unavailable (cached)")
                                        }
                                    })
                                    .unwrap_or_else(|| "Unknown workspace".into()),
                            ),
                    )
                    .child(Button::new("info").label("Info").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(session) = this
                                .tabs
                                .active
                                .as_ref()
                                .and_then(|key| this.sessions.get(key))
                            {
                                this.status = format!(
                                    "{} · {} · {}",
                                    session.candidate.key.agent,
                                    session.candidate.key.native,
                                    session.candidate.events_path.display()
                                );
                                this.show_diagnostics = true;
                                this.diagnostics.push(this.status.clone());
                            }
                            cx.notify();
                        },
                    )))
                    .when(
                        self.tabs.items.iter().any(|tab| {
                            Some(&tab.key) == self.tabs.active.as_ref() && !tab.permanent
                        }),
                        |row| {
                            row.child(Button::new("keep-tab").label("Keep tab").on_click(
                                cx.listener(|this, _, _, cx| {
                                    if let Some(key) = &this.tabs.active {
                                        this.tabs.keep(key.clone().as_str());
                                    }
                                    this.save_due = Some(Instant::now());
                                    cx.notify();
                                }),
                            ))
                        },
                    )
                    .child(
                        Button::new("export")
                            .label("Export Markdown")
                            .on_click(cx.listener(|this, _, _, cx| this.export(cx))),
                    ),
            )
            .when(self.find_visible, |view| {
                view.child(
                    h_flex()
                        .p_2()
                        .gap_2()
                        .child(div().flex_1().child(Input::new(&self.find_input)))
                        .child(Button::new("find-prev").label("Previous").on_click(
                            cx.listener(|this, _, window, cx| this.step_hit(true, window, cx)),
                        ))
                        .child(Button::new("find-next").label("Next").on_click(
                            cx.listener(|this, _, window, cx| this.step_hit(false, window, cx)),
                        ))
                        .when(self.more_hits.is_some(), |row| {
                            row.child(
                                Button::new("find-more").label("More matches").on_click(
                                    cx.listener(|this, _, _, cx| this.next_search_page(cx)),
                                ),
                            )
                        })
                        .child(format!(
                            "{}{}",
                            self.hits.len(),
                            if self.hits_complete { "" } else { "+" }
                        )),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .child(div().flex_1().min_w_0().h_full().child(body))
                    .child(
                        div()
                            .h_full()
                            .border_l_1()
                            .border_color(cx.theme().border)
                            .child(nav),
                    ),
            )
            .child(
                h_flex()
                    .px_4()
                    .py_2()
                    .gap_2()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(format!(
                        "Main conversation · {} / {message_count}",
                        if message_count == 0 { 0 } else { current + 1 }
                    ))
                    .child(div().flex_1())
                    .child(
                        Button::new("reload-document")
                            .label("Reload")
                            .on_click(cx.listener(|this, _, _, cx| this.reload_document(cx))),
                    )
                    .child(div().w(px(100.)).child(Input::new(&self.jump_input)))
                    .child(Button::new("top").label("Top").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(document) = &mut this.document {
                                document.jump(0);
                            }
                            cx.notify();
                        },
                    )))
                    .child(Button::new("latest").label("Latest").on_click(cx.listener(
                        |this, _, _, cx| {
                            if let Some(document) = &mut this.document {
                                document.jump(document.messages.len().saturating_sub(1));
                            }
                            cx.notify();
                        },
                    ))),
            )
            .into_any_element()
    }

    fn render_search(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.hits.len();
        v_flex()
            .size_full()
            .min_h_0()
            .gap_2()
            .child(
                h_flex()
                    .p_2()
                    .gap_2()
                    .child(
                        Button::new("back")
                            .label("Back to sessions")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.search_mode = false;
                                this.query_cancel.cancel();
                                cx.notify();
                            })),
                    )
                    .child(format!(
                        "{count} message hits{} · main conversations",
                        if self.hits_complete {
                            ""
                        } else if self.more_hits.is_some() {
                            " (more available)"
                        } else {
                            " (searching…)"
                        }
                    ))
                    .when(self.more_hits.is_some(), |row| {
                        row.child(
                            Button::new("search-more")
                                .label("Next page")
                                .on_click(cx.listener(|this, _, _, cx| this.next_search_page(cx))),
                        )
                    }),
            )
            .child(
                h_flex()
                    .px_2()
                    .gap_1()
                    .child(
                        Button::new("search-role")
                            .label(match self.role_filter {
                                None => "All roles",
                                Some(Role::User) => "User",
                                Some(Role::Assistant) => "Assistant",
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.role_filter = match this.role_filter {
                                    None => Some(Role::User),
                                    Some(Role::User) => Some(Role::Assistant),
                                    _ => None,
                                };
                                this.query_local = false;
                                this.queue_search(cx);
                            })),
                    )
                    .child(
                        Button::new("search-workspace")
                            .label(if self.workspace_scope {
                                "Active workspace"
                            } else {
                                "All workspaces"
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                if this
                                    .tabs
                                    .active
                                    .as_ref()
                                    .and_then(|key| this.sessions.get(key))
                                    .and_then(|session| session.workspace.as_ref())
                                    .is_some()
                                {
                                    this.workspace_scope = !this.workspace_scope;
                                    this.query_local = false;
                                    this.queue_search(cx);
                                } else {
                                    this.status =
                                        "Open a conversation with a known workspace first".into();
                                    cx.notify();
                                }
                            })),
                    )
                    .child(
                        Button::new("search-dates")
                            .label(match self.date_filter {
                                None => "All dates",
                                Some(7) => "7 days",
                                _ => "30 days",
                            })
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.date_filter = match this.date_filter {
                                    None => Some(7),
                                    Some(7) => Some(30),
                                    _ => None,
                                };
                                this.query_local = false;
                                this.queue_search(cx);
                            })),
                    ),
            )
            .child(
                div()
                    .px_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.search_state.is_empty() {
                        "搜索结果已固定；按 Enter 重新检查已有会话并搜索。".to_owned()
                    } else {
                        self.search_state.clone()
                    }),
            )
            .child(
                uniform_list(
                    "search-results",
                    count,
                    cx.processor(|this, range: std::ops::Range<usize>, _, cx| {
                        range
                            .map(|index| {
                                let hit = this.hits[index].clone();
                                let session = this.sessions.get(&hit.session);
                                let title = if hit.session_name.is_empty() {
                                    session
                                        .map(|session| session.name.clone())
                                        .unwrap_or_else(|| "Session".into())
                                } else {
                                    hit.session_name.clone()
                                };
                                let detail = hit.snippet.clone();
                                v_flex()
                                    .id(("search-hit", index))
                                    .p_3()
                                    .gap_1()
                                    .h(px(116.))
                                    .overflow_hidden()
                                    .border_b_1()
                                    .border_color(cx.theme().border)
                                    .cursor_pointer()
                                    .hover(|style| style.bg(cx.theme().muted))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_sm()
                                            .child(title),
                                    )
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(format!(
                                                "#{} · {}",
                                                hit.ordinal + 1,
                                                hit.role.label()
                                            )),
                                    )
                                    .child(div().text_sm().child(detail))
                                    .on_click(cx.listener(
                                        move |this, event: &ClickEvent, window, cx| {
                                            this.open_hit(
                                                hit.clone(),
                                                event.click_count() >= 2,
                                                window,
                                                cx,
                                            )
                                        },
                                    ))
                            })
                            .collect()
                    }),
                )
                .h_full()
                .w_full(),
            )
            .into_any_element()
    }

    fn render_settings(&mut self, cx: &mut Context<Self>) -> AnyElement {
        v_flex()
            .p_4()
            .gap_2()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                h_flex()
                    .gap_2()
                    .flex_wrap()
                    .child(div().flex_1().child("Sources & reading preferences"))
                    .child(
                        Button::new("choose-source")
                            .label("Add Copilot home…")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_source(false, cx))),
                    )
                    .child(
                        Button::new("choose-vscode-source")
                            .label("Add VS Code User…")
                            .on_click(cx.listener(|this, _, _, cx| this.pick_source(true, cx))),
                    )
                    .child(
                        Button::new("default-source")
                            .label("Use default source")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.settings.sources.clear();
                                this.settings.vscode_sources = None;
                                this.save_due = Some(Instant::now());
                                this.refresh(cx);
                            })),
                    )
                    .child(
                        Button::new("theme")
                            .label("Toggle theme")
                            .on_click(cx.listener(|this, _, window, cx| {
                                let dark = !cx.theme().is_dark();
                                Theme::change(
                                    if dark {
                                        ThemeMode::Dark
                                    } else {
                                        ThemeMode::Light
                                    },
                                    Some(window),
                                    cx,
                                );
                                this.settings.dark = Some(dark);
                                this.save_due = Some(Instant::now());
                            })),
                    )
                    .child(Button::new("font-minus").label("A−").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.change_font(-1., cx);
                        },
                    )))
                    .child(Button::new("font-plus").label("A+").on_click(cx.listener(
                        |this, _, _, cx| {
                            this.change_font(1., cx);
                        },
                    )))
                    .child(
                        Button::new("system-theme")
                            .label("System theme")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.settings.dark = None;
                                Theme::sync_system_appearance(Some(window), cx);
                                this.save_due = Some(Instant::now());
                            })),
                    )
                    .child(
                        Button::new("clear")
                            .label("Clear local index")
                            .on_click(cx.listener(|this, _, _, cx| this.clear_index(cx))),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(if self.settings.sources.is_empty() {
                        "Source: COPILOT_HOME or ~/.copilot".into()
                    } else {
                        format!(
                            "Sources: {}",
                            self.settings
                                .sources
                                .iter()
                                .map(|path| path.display().to_string())
                                .collect::<Vec<_>>()
                                .join(" · ")
                        )
                    }),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(match &self.settings.vscode_sources {
                        None => "VS Code: %APPDATA%\\Code\\User".to_owned(),
                        Some(roots) => format!(
                            "VS Code: {}",
                            roots
                                .iter()
                                .map(|root| root.display().to_string())
                                .collect::<Vec<_>>()
                                .join(" · ")
                        ),
                    }),
            )
            .child(div().text_xs().child(
                "Read-only sources · Remote images blocked · Full history · Index stored locally",
            ))
            .into_any_element()
    }
}

impl Render for Workbench {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.focus_document {
            self.focus_document = false;
            self.doc_focus.focus(window, cx);
        }
        let tabs: Vec<_> = self
            .tabs
            .items
            .iter()
            .map(|tab| {
                let key = tab.key.clone();
                let close_key = key.clone();
                let drop_key = key.clone();
                let menu_key = key.clone();
                let owner = cx.weak_entity();
                let active = self.tabs.active.as_ref() == Some(&key);
                let permanent = tab.permanent;
                h_flex()
                    .id(SharedString::from(format!("tab-{key}")))
                    .gap_2()
                    .px_3()
                    .h(px(38.))
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .cursor_pointer()
                    .when(active, |tab| {
                        tab.bg(cx.theme().background)
                            .border_b_2()
                            .border_color(cx.theme().primary)
                    })
                    .child(
                        div()
                            .max_w(px(190.))
                            .text_sm()
                            .text_ellipsis()
                            .overflow_hidden()
                            .when(!permanent, |title| title.italic())
                            .child(tab.title.clone()),
                    )
                    .child(
                        Button::new(SharedString::from(format!("close-{key}")))
                            .label("×")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.close(&close_key, window, cx);
                            })),
                    )
                    .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.tabs.keep(&key);
                        }
                        this.save_due = Some(Instant::now() + Duration::from_secs(1));
                        this.activate(&key, window, cx);
                    }))
                    .on_drag(
                        DraggedTab {
                            key: tab.key.clone(),
                            title: tab.title.clone(),
                        },
                        |drag, _, _, cx| cx.new(|_| drag.clone()),
                    )
                    .on_drop(cx.listener(move |this, drag: &DraggedTab, _, cx| {
                        this.tabs.move_before(&drag.key, &drop_key);
                        this.save_due = Some(Instant::now());
                        cx.notify();
                    }))
                    .context_menu(move |menu, _, _| {
                        let keep_key = menu_key.clone();
                        let keep_owner = owner.clone();
                        let others_key = menu_key.clone();
                        let others_owner = owner.clone();
                        let right_key = menu_key.clone();
                        let right_owner = owner.clone();
                        menu.item(PopupMenuItem::new("Keep tab").on_click(move |_, _, cx| {
                            let _ = keep_owner.update(cx, |this, cx| {
                                this.tabs.keep(&keep_key);
                                this.save_due = Some(Instant::now());
                                cx.notify();
                            });
                        }))
                        .item(PopupMenuItem::new("Close other tabs").on_click(
                            move |_, window, cx| {
                                let _ = others_owner.update(cx, |this, cx| {
                                    let keys: Vec<_> = this
                                        .tabs
                                        .items
                                        .iter()
                                        .filter(|tab| tab.key != others_key)
                                        .map(|tab| tab.key.clone())
                                        .collect();
                                    for key in keys {
                                        this.close(&key, window, cx);
                                    }
                                });
                            },
                        ))
                        .item(
                            PopupMenuItem::new("Close tabs to the right").on_click(
                                move |_, window, cx| {
                                    let _ = right_owner.update(cx, |this, cx| {
                                        if let Some(index) = this
                                            .tabs
                                            .items
                                            .iter()
                                            .position(|tab| tab.key == right_key)
                                        {
                                            let keys: Vec<_> = this
                                                .tabs
                                                .items
                                                .iter()
                                                .skip(index + 1)
                                                .map(|tab| tab.key.clone())
                                                .collect();
                                            for key in keys {
                                                this.close(&key, window, cx);
                                            }
                                        }
                                    });
                                },
                            ),
                        )
                    })
            })
            .collect();
        let doc = self.render_document(cx);
        let sidebar = if self.search_mode {
            self.render_search(cx)
        } else {
            v_flex()
                .size_full()
                .min_h_0()
                .child(
                    h_flex()
                        .p_2()
                        .gap_1()
                        .child(div().flex_1().child(Input::new(&self.filter_input)))
                        .child(
                            Button::new("date-filter")
                                .label(match self.table.read(cx).delegate().days {
                                    None => "All dates",
                                    Some(7) => "7 days",
                                    _ => "30 days",
                                })
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.table.update(cx, |table, cx| {
                                        SessionTable::update_rows(
                                            table,
                                            |data| {
                                                data.days = match data.days {
                                                    None => Some(7),
                                                    Some(7) => Some(30),
                                                    _ => None,
                                                };
                                                data.refilter();
                                            },
                                            cx,
                                        );
                                    });
                                })),
                        ),
                )
                .child(
                    DataTable::new(&self.table)
                        .bordered(false)
                        .stripe(false)
                        .accessibility_label("Agent sessions"),
                )
                .into_any_element()
        };
        let mut totals = ScanProgress::default();
        for progress in self.progress.values() {
            totals.discovered += progress.discovered;
            totals.scanned += progress.scanned;
            totals.indexable += progress.indexable;
            totals.indexed += progress.indexed;
            totals.unreadable += progress.unreadable;
            totals.failed += progress.failed;
        }
        let index_status = if totals.discovered > 0 {
            format!(
                "{} sessions · scanned {}/{} · indexed {}/{} · unreadable {} · failed {}",
                self.sessions.len(),
                totals.scanned,
                totals.discovered,
                totals.indexed,
                totals.indexable,
                totals.unreadable,
                totals.failed,
            )
        } else {
            format!("{} sessions", self.sessions.len())
        };
        v_flex()
            .id("agent-chat-reader")
            .key_context("AgentChatReader")
            .track_focus(&self.focus)
            .size_full()
            .font_family("Segoe UI")
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &FindAll, window, cx| {
                this.search_input
                    .update(cx, |input, cx| input.focus(window, cx))
            }))
            .on_action(cx.listener(|this, _: &FindDocument, window, cx| {
                if !this.find_visible {
                    this.find_collapsed = this
                        .document
                        .as_ref()
                        .map(|document| (document.key.clone(), document.collapsed.clone()));
                }
                this.find_visible = true;
                this.find_input
                    .update(cx, |input, cx| input.focus(window, cx));
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(key) = this.tabs.active.clone() {
                    this.close(&key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &ReopenTab, window, cx| {
                if let Some(key) = this.tabs.reopen() {
                    this.activate(&key, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| {
                this.tabs.cycle(false);
                if let Some(session) = this
                    .tabs
                    .active
                    .as_ref()
                    .and_then(|key| this.sessions.get(key))
                    .cloned()
                {
                    this.load_document(session, cx);
                    this.doc_focus.focus(window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PreviousTab, window, cx| {
                this.tabs.cycle(true);
                if let Some(session) = this
                    .tabs
                    .active
                    .as_ref()
                    .and_then(|key| this.sessions.get(key))
                    .cloned()
                {
                    this.load_document(session, cx);
                    this.doc_focus.focus(window, cx);
                }
            }))
            .on_key_up(cx.listener(|this, event: &KeyUpEvent, _, _| {
                if !event.keystroke.modifiers.control && this.tabs.finish_cycle() {
                    this.save_due = Some(Instant::now() + Duration::from_secs(1));
                }
            }))
            .on_modifiers_changed(cx.listener(|this, event: &ModifiersChangedEvent, _, _| {
                if !event.modifiers.control && this.tabs.finish_cycle() {
                    this.save_due = Some(Instant::now() + Duration::from_secs(1));
                }
            }))
            .on_action(cx.listener(|this, _: &OpenSelected, window, cx| {
                let session = this
                    .table
                    .read(cx)
                    .selected_row()
                    .and_then(|index| this.table.read(cx).delegate().at(index));
                if let Some(session) = session {
                    this.open_session(session, true, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PreviewSelected, window, cx| {
                let session = this
                    .table
                    .read(cx)
                    .selected_row()
                    .and_then(|index| this.table.read(cx).delegate().at(index));
                if let Some(session) = session {
                    this.open_session(session, false, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &NextMessage, window, cx| {
                if this.input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                if let Some(document) = &mut this.document {
                    document.jump(
                        (document.current + 1).min(document.messages.len().saturating_sub(1)),
                    );
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PreviousMessage, window, cx| {
                if this.input_focused(window, cx) {
                    cx.propagate();
                    return;
                }
                if let Some(document) = &mut this.document {
                    document.jump(document.current.saturating_sub(1));
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                if this.raw_view.is_some() {
                    this.raw_view = None;
                } else if this.find_visible {
                    this.close_find();
                } else if this.search_mode {
                    this.search_mode = false;
                    this.query_cancel.cancel();
                } else {
                    this.show_settings = false;
                    this.show_diagnostics = false;
                }
                this.doc_focus.focus(window, cx);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &Refresh, _, cx| this.refresh(cx)))
            .on_action(
                cx.listener(|this, _: &NextHit, window, cx| this.step_hit(false, window, cx)),
            )
            .on_action(
                cx.listener(|this, _: &PreviousHit, window, cx| this.step_hit(true, window, cx)),
            )
            .child(
                h_flex()
                    .h(px(56.))
                    .px_4()
                    .gap_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().font_weight(FontWeight::BOLD).child("AgentChatReader"))
                    .child(div().flex_1().child(Input::new(&self.search_input)))
                    .child(
                        Button::new("refresh")
                            .label("Refresh")
                            .on_click(cx.listener(|this, _, _, cx| this.refresh(cx))),
                    )
                    .child(
                        Button::new("settings")
                            .label("Settings")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_settings = !this.show_settings;
                                cx.notify();
                            })),
                    ),
            )
            .when(self.show_settings, |view| {
                view.child(self.render_settings(cx))
            })
            .child(
                div().flex_1().min_h_0().child(
                    h_resizable("workbench-panels")
                        .on_resize(cx.listener(|this, state: &Entity<ResizableState>, _, cx| {
                            this.settings.sidebar_width = state
                                .read(cx)
                                .sizes()
                                .first()
                                .map(|width| f32::from(*width));
                            this.save_due = Some(Instant::now() + Duration::from_secs(1));
                        }))
                        .child(
                            resizable_panel()
                                .size(px(self
                                    .settings
                                    .sidebar_width
                                    .unwrap_or(440.)
                                    .clamp(300., 800.)))
                                .size_range(px(300.)..px(800.))
                                .child(
                                    v_flex()
                                        .size_full()
                                        .min_h_0()
                                        .child(
                                            h_flex()
                                                .px_3()
                                                .h(px(38.))
                                                .justify_between()
                                                .border_b_1()
                                                .border_color(cx.theme().border)
                                                .child(
                                                    div()
                                                        .text_xs()
                                                        .font_weight(FontWeight::SEMIBOLD)
                                                        .child("SESSIONS"),
                                                )
                                                .child(
                                                    Button::new("time-mode")
                                                        .label(match self.settings.mode {
                                                            TimeMode::Latest => "Latest message ↓",
                                                            TimeMode::FirstAssistant => {
                                                                "First assistant ↓"
                                                            }
                                                        })
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.change_mode(cx)
                                                        })),
                                                ),
                                        )
                                        .child(div().flex_1().min_h_0().child(sidebar)),
                                ),
                        )
                        .child(
                            resizable_panel().child(
                                v_flex()
                                    .size_full()
                                    .min_h_0()
                                    .child(
                                        h_flex()
                                            .id("tabs")
                                            .h(px(38.))
                                            .flex_shrink_0()
                                            .overflow_x_scroll()
                                            .bg(cx.theme().muted)
                                            .children(tabs),
                                    )
                                    .child(div().flex_1().min_h_0().child(doc)),
                            ),
                        ),
                ),
            )
            .when(self.show_diagnostics, |view| {
                view.child(
                    v_flex()
                        .id("diagnostics")
                        .max_h(px(180.))
                        .overflow_y_scroll()
                        .p_3()
                        .gap_1()
                        .bg(cx.theme().muted)
                        .children(
                            self.diagnostics
                                .iter()
                                .rev()
                                .take(30)
                                .map(|message| div().text_xs().child(message.clone())),
                        ),
                )
            })
            .child(
                h_flex()
                    .h(px(28.))
                    .flex_shrink_0()
                    .px_3()
                    .gap_4()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(
                        div()
                            .id("index-progress")
                            .role(gpui_kit::Role::Status)
                            .aria_label(index_status.clone())
                            .child(index_status),
                    )
                    .child(
                        div()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .child(self.status.clone()),
                    )
                    .child(
                        Button::new("diagnostic-count")
                            .label(format!("{} notices", self.diagnostics.len()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.show_diagnostics = !this.show_diagnostics;
                                cx.notify();
                            })),
                    ),
            )
    }
}
