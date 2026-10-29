use crate::{
    live_markdown::{self, BlockKind},
    native::{self, NativeEvent, NativeShell},
    turn::Turn,
};
use gpui::{prelude::*, *};
use gpui_component::{
    ActiveTheme, Icon, IconName, Theme, ThemeMode,
    input::{Input, InputEvent, InputState},
    text::{TextView, TextViewStyle},
};
use quick_ask_core::{
    auth::{AccountSummary, AuthManager},
    chat::{self, Message, Model, ReplyOptions},
    history::{Conversation, HistoryStore},
};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

actions!(quick_ask, [Dismiss, NewChat, ShowHistory, CopyAnswer, Quit]);
const ACCENT: u32 = 0x0a84ff;
const USAGE_URL: &str = "https://chatgpt.com/settings/usage";

enum Event {
    Status(u64, String),
    Connected(u64, bool, Result<(AccountSummary, Vec<Model>), String>),
    SignedOut(Result<(), String>),
    Delta(u64, String),
    Finished(u64, Result<(), String>),
}

pub struct QuickAsk {
    input: Entity<InputState>,
    composer_expanded: bool,
    auth: Arc<Mutex<AuthManager>>,
    account: Option<AccountSummary>,
    auth_busy: bool,
    auth_can_cancel: bool,
    auth_request_id: u64,
    auth_cancel: Arc<AtomicBool>,
    models: Vec<Model>,
    selected_model: usize,
    history: HistoryStore,
    conversations: Vec<Conversation>,
    conversation: Conversation,
    history_open: bool,
    models_open: bool,
    settings_open: bool,
    history_writable: bool,
    turn: Turn,
    cancel: Arc<AtomicBool>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    native: Option<NativeShell>,
    shortcut: String,
    notice: Option<String>,
    error: Option<String>,
    status: String,
    scroll: ScrollHandle,
    follow_reply: bool,
    _subscriptions: Vec<Subscription>,
}

impl QuickAsk {
    pub fn new(
        window: &mut Window,
        cx: &mut Context<Self>,
        auth: Arc<Mutex<AuthManager>>,
        data_dir: PathBuf,
        shell: anyhow::Result<NativeShell>,
    ) -> Self {
        cx.bind_keys([
            KeyBinding::new("escape", Dismiss, Some("QuickAsk")),
            KeyBinding::new("cmd-n", NewChat, Some("QuickAsk")),
            KeyBinding::new("cmd-h", ShowHistory, Some("QuickAsk")),
            KeyBinding::new("cmd-shift-c", CopyAnswer, Some("QuickAsk")),
            KeyBinding::new("cmd-q", Quit, Some("QuickAsk")),
            KeyBinding::new(
                "shift-enter",
                gpui_component::input::Enter { secondary: true },
                Some("QuickAsk && Input"),
            ),
        ]);
        let input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("Ask anything…")
                .auto_grow(1, 4)
        });
        let subscription = cx.subscribe_in(&input, window, |this, _, event, window, cx| {
            if matches!(event, InputEvent::PressEnter { secondary: false }) {
                this.send(window, cx);
            }
            if matches!(event, InputEvent::Change) {
                let text = this.input.read(cx).value();
                this.composer_expanded = text.contains('\n') || text.chars().count() > 50;
                this.fit(window);
            }
        });
        let activation = cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() && native::panel_visible(window) {
                native::hide_panel(window);
                cx.notify();
            }
        });

        let appearance = cx.observe_window_appearance(window, |_, window, cx| {
            Theme::change(ThemeMode::from(window.appearance()), Some(window), cx);
            Theme::global_mut(cx).background = transparent_black();
            Theme::global_mut(cx).font_size = px(14.);
            Theme::global_mut(cx).muted_foreground = if Theme::global(cx).mode.is_dark() {
                rgb(0xb6b6c0).into()
            } else {
                rgb(0x60606a).into()
            };
            cx.notify();
        });
        let (tx, rx) = mpsc::channel();
        let history = HistoryStore::new(data_dir);
        let (conversations, history_writable, load_error) = match history.load() {
            Ok(items) => (items, true, None),
            Err(error) => (
                vec![],
                false,
                Some(format!(
                    "History could not be read; the original file was preserved. {error}"
                )),
            ),
        };
        let (shell, shortcut, mut notice) = match shell {
            Ok(shell) => {
                let label = shell.shortcut_label();
                let notice = shell.shortcut_warning().map(str::to_owned);
                (Some(shell), label, notice)
            }
            Err(error) => (
                None,
                "Menu bar".into(),
                Some(format!("Global shortcut unavailable: {error}")),
            ),
        };
        if let Some(load_error) = load_error {
            notice = Some(match notice {
                Some(notice) => format!("{notice}\n{load_error}"),
                None => load_error,
            });
        }
        let account = auth.lock().ok().and_then(|manager| manager.account());
        let has_account = account.is_some();
        let mut this = Self {
            input,
            composer_expanded: false,
            auth,
            account,
            auth_busy: false,
            auth_can_cancel: false,
            auth_request_id: 0,
            auth_cancel: Arc::new(AtomicBool::new(false)),
            models: vec![],
            selected_model: 0,
            history,
            conversations,
            conversation: Conversation::new(String::new()),
            history_open: false,
            models_open: false,
            settings_open: false,
            history_writable,
            turn: Turn::default(),
            cancel: Arc::new(AtomicBool::new(false)),
            tx,
            rx,
            native: shell,
            shortcut,
            notice,
            error: None,
            status: String::new(),
            scroll: ScrollHandle::new(),
            follow_reply: true,
            _subscriptions: vec![subscription, activation, appearance],
        };
        this.fit(window);
        this.input.update(cx, |input, cx| input.focus(window, cx));
        if has_account {
            this.connect(false, cx);
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                Timer::after(Duration::from_millis(16)).await;
                if this
                    .update_in(cx, |this, window, cx| this.poll(window, cx))
                    .is_err()
                {
                    break;
                }
            }
        })
        .detach();
        this
    }

    fn connect(&mut self, sign_in: bool, cx: &mut Context<Self>) {
        if self.auth_busy {
            return;
        }
        self.auth_busy = true;
        self.auth_can_cancel = true;
        self.auth_request_id = self.auth_request_id.wrapping_add(1);
        let request_id = self.auth_request_id;
        self.auth_cancel.store(true, Ordering::Relaxed);
        self.auth_cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.auth_cancel.clone();
        self.error = None;
        self.status = if sign_in {
            "Finish signing in in your browser…"
        } else {
            "Loading your models…"
        }
        .into();
        let auth = self.auth.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<_> {
                let mut manager = auth
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Account storage is unavailable"))?;
                let account = if sign_in {
                    manager.sign_in_cancellable(
                        |status| {
                            let _ = tx.send(Event::Status(request_id, status));
                        },
                        &cancel,
                    )?
                } else {
                    manager
                        .account()
                        .ok_or_else(|| anyhow::anyhow!("Sign in to continue"))?
                };
                let token = manager.access_token()?;
                drop(manager);
                let models = chat::list_models(&token)?;
                anyhow::ensure!(
                    !models.is_empty(),
                    "No eligible models were returned for this account"
                );
                Ok((account, models))
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Event::Connected(request_id, sign_in, result));
        });
        cx.notify();
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let prompt = self.input.read(cx).value().trim().to_owned();
        if prompt.is_empty() || self.turn.busy || self.auth_busy {
            return;
        }
        if self.account.is_none() || self.models.is_empty() {
            self.error = Some("Sign in and select an available model first.".into());
            cx.notify();
            return;
        }
        self.input
            .update(cx, |input, cx| input.set_value("", window, cx));
        self.start_turn(prompt, window, cx);
    }

    fn start_turn(&mut self, prompt: String, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        let id = self.turn.begin(prompt.clone());
        self.follow_reply = true;
        self.scroll.scroll_to_bottom();
        self.history_open = false;
        self.models_open = false;
        self.settings_open = false;
        self.error = None;
        self.status.clear();
        let model = self.models[self.selected_model].id.clone();
        let reasoning_effort =
            self.models[self.selected_model].resolve_effort(self.conversation.reasoning_effort);
        if let Some(effort) = reasoning_effort {
            self.conversation.reasoning_effort = effort;
        }
        self.conversation.model = model.clone();
        let mut messages = self.conversation.messages.clone();
        messages.push(Message {
            role: "user".into(),
            content: prompt,
        });
        let auth = self.auth.clone();
        let tx = self.tx.clone();
        let cancel = self.cancel.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                let token = auth
                    .lock()
                    .map_err(|_| anyhow::anyhow!("Account storage is unavailable"))?
                    .access_token()?;
                chat::stream_reply(
                    &token,
                    ReplyOptions {
                        model: &model,
                        reasoning_effort,
                    },
                    &messages,
                    &cancel,
                    |text| {
                        let _ = tx.send(Event::Delta(id, text));
                    },
                )
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Event::Finished(id, result));
        });
        self.fit(window);
        cx.notify();
    }

    fn stop(&mut self, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.turn.finish(self.turn.id, false);
        self.status = "Stopped · partial answer isn’t saved".into();
        cx.notify();
    }

    fn new_chat(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.cancel.store(true, Ordering::Relaxed);
        self.turn.reset();
        self.follow_reply = true;
        let reasoning_effort = self.conversation.reasoning_effort;
        self.conversation = Conversation::new(
            self.models
                .get(self.selected_model)
                .map(|m| m.id.clone())
                .unwrap_or_default(),
        );
        self.conversation.reasoning_effort = reasoning_effort;
        self.history_open = false;
        self.models_open = false;
        self.settings_open = false;
        self.error = None;
        self.status.clear();
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            input.focus(window, cx);
        });
        self.fit(window);
        cx.notify();
    }

    fn fit(&self, window: &mut Window) {
        let expanded = self.history_open
            || self.models_open
            || !self.conversation.messages.is_empty()
            || self.turn.prompt.is_some();
        let extra = if self.error.is_some() || self.notice.is_some() {
            100.
        } else {
            0.
        };
        let height = if self.settings_open {
            280. + extra
        } else if self.history_open {
            (180. + self.conversations.len().max(1) as f32 * 64.).min(540.)
        } else if self.models_open {
            (268. + self.models.len() as f32 * 36.).min(540.)
        } else if expanded {
            let message_height = |text: &str| {
                let lines: usize = text
                    .lines()
                    .map(|line| line.chars().count().div_ceil(68).max(1))
                    .sum();
                66. + lines.max(1) as f32 * 22. + text.matches("![").count() as f32 * 260.
            };
            let mut content: f32 = self
                .conversation
                .messages
                .iter()
                .map(|message| message_height(&message.content))
                .sum();
            if let Some(prompt) = &self.turn.prompt {
                content += message_height(prompt) + message_height(self.turn.visible_text());
            }
            (190. + content + extra).min(580.)
        } else if self.account.is_none() {
            156. + extra
        } else {
            132. + extra
        };
        let target = size(
            px(620.),
            px((height + if self.composer_expanded { 72. } else { 0. }).min(620.)),
        );
        if window.bounds().size != target {
            window.resize(target);
        }
    }

    fn save(&mut self) {
        if !self.history_writable {
            return;
        }
        self.conversation.updated_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Some(first) = self.conversation.messages.first() {
            self.conversation.title = first.content.chars().take(70).collect();
        }
        self.conversations
            .retain(|item| item.id != self.conversation.id);
        self.conversations.insert(0, self.conversation.clone());
        self.conversations.truncate(100);
        if let Err(error) = self.history.save(&self.conversations) {
            self.error = Some(format!("Could not save this chat: {error}"));
        }
    }

    fn poll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let events = self
            .native
            .as_ref()
            .map(|shell| shell.poll())
            .unwrap_or_default();
        for event in events {
            match event {
                NativeEvent::Toggle => {
                    if native::panel_visible(window) {
                        native::hide_panel(window);
                    } else {
                        self.show(window, cx);
                    }
                }
                NativeEvent::NewChat => {
                    self.new_chat(window, cx);
                    self.show(window, cx);
                }
                NativeEvent::ManageUsage => {
                    let _ = open::that(USAGE_URL);
                }
                NativeEvent::Quit => {
                    self.cancel.store(true, Ordering::Relaxed);
                    cx.quit();
                }
            }
        }
        let mut changed = false;
        while let Ok(event) = self.rx.try_recv() {
            changed = true;
            match event {
                Event::Status(id, status) => {
                    if id == self.auth_request_id {
                        self.status = status;
                    }
                }
                Event::Connected(id, interactive, result) => {
                    if id != self.auth_request_id {
                        continue;
                    }
                    self.auth_busy = false;
                    self.status.clear();
                    match result {
                        Ok((account, models)) => {
                            self.account = Some(account);
                            self.models = models;
                            self.selected_model = 0;
                            self.error = None;
                        }
                        Err(error) => {
                            self.error = Some(error);
                            if let Ok(manager) = self.auth.try_lock() {
                                self.account = manager.account();
                            }
                        }
                    }
                    if interactive {
                        self.show(window, cx);
                    }
                }
                Event::SignedOut(result) => {
                    self.auth_busy = false;
                    self.account = None;
                    self.models.clear();
                    self.new_chat(window, cx);
                    if let Err(error) = result {
                        self.error = Some(error);
                    }
                }
                Event::Delta(id, text) => {
                    self.turn.delta(id, &text);
                }
                Event::Finished(id, result) => {
                    if id == self.turn.id && self.turn.busy {
                        if let Err(error) = result {
                            self.turn.finish(id, false);
                            self.error = Some(error);
                            if let Ok(manager) = self.auth.try_lock() {
                                self.account = manager.account();
                            }
                            if self.account.is_none() {
                                self.models.clear();
                            }
                        } else {
                            self.turn.network_done = true;
                        }
                    }
                }
            }
        }
        changed |= self.turn.advance();
        if self.turn.network_done
            && self.turn.busy
            && let Some((prompt, reply)) = self.turn.finish(self.turn.id, true)
        {
            self.conversation.messages.push(Message {
                role: "user".into(),
                content: prompt,
            });
            self.conversation.messages.push(Message {
                role: "assistant".into(),
                content: reply,
            });
            self.save();
            self.status.clear();
            changed = true;
        }
        if changed {
            self.fit(window);
            if self.follow_reply {
                self.scroll.scroll_to_bottom();
            }
            cx.notify();
        }
    }

    pub(crate) fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        native::show_panel(window, cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    fn copy_answer(&mut self, cx: &mut Context<Self>) {
        let text = if !self.turn.text.is_empty() {
            Some(self.turn.visible_text().to_owned())
        } else {
            self.conversation
                .messages
                .iter()
                .rev()
                .find(|m| m.role == "assistant")
                .map(|m| m.content.clone())
        };
        if let Some(text) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        if self.auth_busy {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.turn.reset();
        self.auth_busy = true;
        self.auth_can_cancel = false;
        let auth = self.auth.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<()> {
                auth.lock()
                    .map_err(|_| anyhow::anyhow!("Account storage is unavailable"))?
                    .sign_out()
            })()
            .map_err(|error| format!("{error:#}"));
            let _ = tx.send(Event::SignedOut(result));
        });
        cx.notify();
    }

    fn history_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("history-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_2()
            .p_4();
        if self.conversations.is_empty() {
            list = list.child(
                div()
                    .py_8()
                    .text_color(cx.theme().muted_foreground)
                    .child("Your completed chats will appear here."),
            );
        }
        for (index, item) in self.conversations.iter().enumerate() {
            list = list.child(
                div()
                    .id(("history", index))
                    .rounded_lg()
                    .p_3()
                    .cursor_pointer()
                    .bg(rgba(0x8080800c))
                    .hover(|s| s.bg(rgba(0x80808018)))
                    .child(div().text_size(px(14.)).child(item.title.clone()))
                    .child(
                        div()
                            .mt_1()
                            .text_size(px(11.))
                            .text_color(cx.theme().muted_foreground)
                            .child(format!("{} messages · {}", item.messages.len(), item.model)),
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.cancel.store(true, Ordering::Relaxed);
                        this.turn.reset();
                        this.follow_reply = true;
                        this.scroll.scroll_to_bottom();
                        this.conversation = this.conversations[index].clone();
                        if let Some(i) = this
                            .models
                            .iter()
                            .position(|m| m.id == this.conversation.model)
                        {
                            this.selected_model = i;
                        }
                        this.history_open = false;
                        this.error = None;
                        this.fit(window);
                        cx.notify();
                    })),
            );
        }
        list.into_any_element()
    }

    fn models_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut list = div()
            .id("models-list")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .flex()
            .flex_col()
            .gap_1()
            .p_4();
        if let Some(model) = self.models.get(self.selected_model)
            && !model.reasoning_efforts.is_empty()
        {
            let selected = model.resolve_effort(self.conversation.reasoning_effort);
            let mut choices = div().flex().gap_1().pb_4();
            for effort in &model.reasoning_efforts {
                let effort = *effort;
                choices = choices.child(
                    pill(("choose-effort", effort as usize), effort.label())
                        .flex_1()
                        .text_center()
                        .when(selected == Some(effort), |s| {
                            s.bg(rgba(0x0a84ff22)).text_color(rgb(ACCENT))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            if !this.turn.busy {
                                this.conversation.reasoning_effort = effort;
                                this.models_open = false;
                                if !this.conversation.messages.is_empty() {
                                    this.save();
                                }
                                this.input.update(cx, |input, cx| input.focus(window, cx));
                                this.fit(window);
                                cx.notify();
                            }
                        })),
                );
            }
            list = list
                .child(
                    div()
                        .pb_2()
                        .text_size(px(12.))
                        .text_color(cx.theme().muted_foreground)
                        .child("Reasoning effort"),
                )
                .child(choices);
        }
        list = list.child(
            div()
                .pb_2()
                .text_size(px(12.))
                .text_color(cx.theme().muted_foreground)
                .child("Models available with your ChatGPT plan"),
        );
        for (index, model) in self.models.iter().enumerate() {
            list = list.child(
                pill(("choose-model", index), model.name.clone())
                    .p_3()
                    .when(index == self.selected_model, |s| {
                        s.bg(rgba(0x80808015)).text_color(rgb(ACCENT))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if this.turn.busy {
                            return;
                        }
                        this.selected_model = index;
                        if let Some(effort) =
                            this.models[index].resolve_effort(this.conversation.reasoning_effort)
                        {
                            this.conversation.reasoning_effort = effort;
                        }
                        this.models_open = false;
                        this.fit(window);
                        cx.notify();
                    })),
            );
        }
        list.into_any_element()
    }

    fn transcript(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let scroll = self.scroll.clone();
        let follow = self.follow_reply;
        let mut list = div()
            .on_children_prepainted(move |_, window, _| {
                if follow && scroll.max_offset().height + scroll.offset().y > px(1.) {
                    scroll.scroll_to_bottom();
                    window.refresh();
                }
            })
            .id("transcript")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                if event.delta.pixel_delta(px(20.)).y > px(0.) {
                    this.follow_reply = false;
                    cx.notify();
                } else {
                    let view = cx.entity();
                    cx.defer(move |cx| {
                        view.update(cx, |this, cx| {
                            if this.scroll.max_offset().height + this.scroll.offset().y <= px(36.) {
                                this.follow_reply = true;
                                cx.notify();
                            }
                        });
                    });
                }
            }))
            .flex()
            .flex_col()
            .gap_5()
            .px_5()
            .py_4();
        for (index, message) in self.conversation.messages.iter().enumerate() {
            list = list.child(message_view(index, message, window, cx));
        }
        if let Some(prompt) = &self.turn.prompt {
            list = list.child(message_view(
                self.conversation.messages.len(),
                &Message {
                    role: "user".into(),
                    content: prompt.clone(),
                },
                window,
                cx,
            ));
            if !self.turn.visible_text().is_empty() {
                list = list.child(answer_view(
                    self.conversation.messages.len() + 1,
                    self.turn.visible_text(),
                    Some(self.turn.id),
                    window,
                    cx,
                ));
            }
            if self.turn.busy {
                let label = if self.turn.visible_text().is_empty() {
                    "Thinking…"
                } else {
                    "Writing…"
                };
                list = list.child(activity_indicator(label, cx));
            }
        }
        list.into_any_element()
    }
}

fn pill(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Stateful<Div> {
    div()
        .id(id)
        .px_2()
        .py_1()
        .rounded_md()
        .cursor_pointer()
        .text_size(px(12.))
        .hover(|s| s.bg(rgba(0x80808018)))
        .child(label.into())
}

fn icon_button(id: impl Into<ElementId>, icon: IconName, label: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .size(px(28.))
        .rounded_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .hover(|s| s.bg(rgba(0x8080801a)))
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(label).build(window, cx))
        .child(Icon::new(icon).size(px(15.)))
}

fn copy_button(id: impl Into<ElementId>, text: SharedString, label: &'static str) -> Stateful<Div> {
    icon_button(id, IconName::Copy, label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
            cx.stop_propagation();
        })
}

fn drag_surface(id: &'static str) -> Stateful<Div> {
    div()
        .id(id)
        .cursor(CursorStyle::OpenHand)
        .on_mouse_down(MouseButton::Left, |_, window, cx| {
            native::start_drag(window);
            cx.stop_propagation();
        })
}

fn activity_indicator(label: &'static str, cx: &App) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap_2()
        .py_1()
        .text_size(px(12.))
        .text_color(cx.theme().muted_foreground)
        .child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .children((0..3usize).map(|index| {
                    div()
                        .size(px(4.))
                        .rounded_full()
                        .bg(rgb(ACCENT))
                        .with_animation(
                            ("activity-dot", index),
                            Animation::new(Duration::from_millis(1200)).repeat(),
                            move |dot, progress| {
                                let phase = (progress - index as f32 * 0.16).rem_euclid(1.);
                                dot.opacity(0.25 + 0.75 * (phase * std::f32::consts::PI).sin())
                            },
                        )
                })),
        )
        .child(label)
        .into_any_element()
}

fn message_view(index: usize, message: &Message, window: &mut Window, cx: &mut App) -> AnyElement {
    if message.role == "user" {
        div()
            .id(("question", index))
            .flex()
            .flex_col()
            .items_end()
            .gap_1()
            .child(
                div()
                    .max_w(px(530.))
                    .rounded_lg()
                    .px_4()
                    .py_3()
                    .bg(rgba(0x80808015))
                    .text_color(cx.theme().foreground)
                    .text_size(px(14.))
                    .child(message.content.clone()),
            )
            .child(
                copy_button(
                    "copy-question",
                    message.content.clone().into(),
                    "Copy question",
                )
                .text_color(cx.theme().muted_foreground),
            )
            .into_any_element()
    } else {
        answer_view(index, &message.content, None, window, cx)
    }
}

fn markdown_view(
    id: impl Into<ElementId>,
    text: SharedString,
    window: &mut Window,
    cx: &mut App,
) -> TextView {
    TextView::markdown(id, text, window, cx)
        .selectable(true)
        .style(TextViewStyle::default().code_block(div().pr_12().style().clone()))
        .code_block_actions(|block, _, cx| {
            let code = block.code();
            // Distinct owned code allocations keep sibling copy buttons independent.
            copy_button(("copy-code", code.as_ptr() as usize), code, "Copy code")
                .text_color(cx.theme().muted_foreground)
        })
}

fn answer_view(
    index: usize,
    text: &str,
    live_turn: Option<u64>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let text: SharedString = text.to_owned().into();
    let mut answer = div()
        .id(("response", index))
        .flex()
        .flex_col()
        .gap_1()
        .text_size(px(14.))
        .line_height(relative(1.55));
    if let Some(turn_id) = live_turn {
        for block in live_markdown::blocks(&text) {
            let key = SharedString::from(format!("live-block-{turn_id}-{}", block.start));
            match block.kind {
                BlockKind::Code { body, .. } => {
                    answer = answer.child(
                        div()
                            .id(key)
                            .relative()
                            .p_3()
                            .pr_12()
                            .rounded(cx.theme().radius)
                            .bg(cx.theme().muted)
                            .font_family(cx.theme().mono_font_family.clone())
                            .text_size(cx.theme().mono_font_size)
                            .child(body.to_owned())
                            .child(
                                div().absolute().top_2().right_2().child(
                                    copy_button(
                                        "copy-live-code",
                                        body.to_owned().into(),
                                        "Copy code",
                                    )
                                    .text_color(cx.theme().muted_foreground),
                                ),
                            ),
                    );
                }
                BlockKind::Markdown => {
                    let id = if block.complete {
                        key
                    } else {
                        SharedString::from(format!("{key}-{}", block.source.len()))
                    };
                    answer = answer.child(markdown_view(
                        id,
                        block.source.to_owned().into(),
                        window,
                        cx,
                    ));
                }
            }
        }
    } else {
        answer = answer.child(markdown_view(("answer", index), text.clone(), window, cx));
    }
    answer
        .child(
            div().child(
                copy_button("copy-response", text, "Copy response as Markdown")
                    .text_color(cx.theme().muted_foreground),
            ),
        )
        .into_any_element()
}

impl Render for QuickAsk {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let expanded = !self.conversation.messages.is_empty() || self.turn.prompt.is_some();
        let has_section = self.history_open || self.models_open || self.settings_open;
        let signed_in = self.account.is_some();
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let glass_tint = if cx.theme().mode.is_dark() {
            rgba(0x10101455)
        } else {
            rgba(0xffffffa8)
        };
        let mut body = div()
            .key_context("QuickAsk")
            .size_full()
            .flex()
            .flex_col()
            .bg(glass_tint)
            .text_color(foreground)
            .font_family(".SystemUIFont")
            .overflow_hidden()
            .capture_key_down(cx.listener(|_, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    native::hide_panel(window);
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|_, _: &Dismiss, window, _| native::hide_panel(window)))
            .on_action(cx.listener(|this, _: &NewChat, window, cx| this.new_chat(window, cx)))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                this.history_open = !this.history_open;
                this.models_open = false;
                this.settings_open = false;
                this.fit(window);
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &CopyAnswer, _, cx| this.copy_answer(cx)))
            .on_action(|_: &Quit, _, cx| cx.quit())
            .child(
                drag_surface("window-drag-rim")
                    .h(px(8.))
                    .w_full()
                    .flex_shrink_0(),
            );

        if expanded || has_section {
            let title = if self.history_open {
                "Chats"
            } else if self.models_open {
                "Model & reasoning"
            } else if self.settings_open {
                "Account"
            } else {
                "Quick Ask"
            };
            body = body.child(
                div()
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_4()
                    .pt_3()
                    .pb_2()
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .when(has_section, |s| {
                                s.child(
                                    icon_button("back", IconName::ChevronLeft, "Back").on_click(
                                        cx.listener(|this, _, window, cx| {
                                            this.history_open = false;
                                            this.models_open = false;
                                            this.settings_open = false;
                                            this.fit(window);
                                            cx.notify();
                                        }),
                                    ),
                                )
                            })
                            .child(
                                div()
                                    .text_size(px(12.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(muted)
                                    .child(title),
                            ),
                    )
                    .child(
                        icon_button("dismiss", IconName::Close, "Dismiss · Esc")
                            .text_color(muted)
                            .on_click(|_, window, _| native::hide_panel(window)),
                    ),
            );
        }

        if self.settings_open {
            let mut settings = div()
                .id("account-settings")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .flex()
                .flex_col()
                .gap_2()
                .px_5()
                .py_3();
            if let Some(account) = &self.account {
                settings = settings
                    .child(
                        div()
                            .text_size(px(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .child(account.email.clone()),
                    )
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(muted)
                            .child("Using your ChatGPT plan"),
                    )
                    .child(
                        pill("signout", "Sign out")
                            .text_color(muted)
                            .on_click(cx.listener(|this, _, _, cx| this.sign_out(cx))),
                    );
            } else {
                settings = settings.child(
                    div()
                        .text_size(px(13.))
                        .text_color(muted)
                        .child("Connect your personal ChatGPT account."),
                );
            }
            body = body.child(
                settings.child(
                    pill("usage", "Manage plan usage ↗")
                        .text_color(rgb(ACCENT))
                        .on_click(|_, _, _| {
                            let _ = open::that(USAGE_URL);
                        }),
                ),
            );
        } else if self.models_open {
            body = body.child(self.models_view(cx));
        } else if self.history_open {
            body = body.child(self.history_view(cx));
        } else if expanded {
            body = body.child(self.transcript(window, cx));
            if !self.follow_reply {
                body = body.child(
                    div().flex_shrink_0().flex().justify_end().px_5().child(
                        pill("jump-to-latest", "Jump to latest ↓")
                            .text_color(muted)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.follow_reply = true;
                                this.scroll.scroll_to_bottom();
                                cx.notify();
                            })),
                    ),
                );
            }
        }

        if let Some(notice) = &self.notice {
            body = body.child(
                div()
                    .id("notice")
                    .max_h(px(72.))
                    .overflow_y_scroll()
                    .px_5()
                    .pt_3()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(notice.clone()),
            );
        }
        if let Some(error) = &self.error {
            body = body.child(
                div()
                    .id("error")
                    .max_h(px(80.))
                    .overflow_y_scroll()
                    .px_5()
                    .pt_3()
                    .text_size(px(12.))
                    .text_color(rgb(0xe35b55))
                    .child(error.clone()),
            );
        }
        if self.auth_busy && !self.status.is_empty() {
            body = body.child(
                div()
                    .px_5()
                    .pt_2()
                    .text_size(px(11.))
                    .text_color(muted)
                    .child(self.status.clone()),
            );
        }

        let send = div()
            .id("send")
            .size(px(32.))
            .rounded_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .bg(if self.turn.busy || signed_in {
                rgb(ACCENT)
            } else {
                rgba(0x80808025)
            })
            .text_color(if signed_in {
                rgb(0xffffff).into()
            } else {
                muted
            })
            .child(if self.turn.busy {
                div()
                    .size(px(10.))
                    .rounded(px(2.))
                    .bg(rgb(0xffffff))
                    .into_any_element()
            } else {
                Icon::new(IconName::ArrowUp)
                    .size(px(18.))
                    .into_any_element()
            })
            .on_click(cx.listener(|this, _, window, cx| {
                if this.turn.busy {
                    this.stop(cx);
                } else {
                    this.send(window, cx);
                }
            }));
        body = body.child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .gap_3()
                .px_5()
                .pt_3()
                .pb_2()
                .child(
                    div().flex_1().min_w_0().child(
                        Input::new(&self.input)
                            .appearance(false)
                            .text_size(px(if expanded { 16. } else { 22. }))
                            .disabled(!signed_in || self.auth_busy),
                    ),
                )
                .child(send),
        );

        let model_label = self
            .models
            .get(self.selected_model)
            .map(|m| m.name.clone())
            .unwrap_or_else(|| "ChatGPT".into());
        let mut left = div().flex().items_center().gap_1().text_color(muted);
        if signed_in && !self.models.is_empty() {
            left = left.child(
                pill("model", model_label)
                    .child(Icon::new(IconName::ChevronDown).size(px(11.)))
                    .flex()
                    .items_center()
                    .gap_1()
                    .on_click(cx.listener(|this, _, window, cx| {
                        if !this.turn.busy {
                            this.models_open = !this.models_open;
                            this.history_open = false;
                            this.settings_open = false;
                            this.fit(window);
                            cx.notify();
                        }
                    })),
            );
            if let Some(effort) =
                self.models[self.selected_model].resolve_effort(self.conversation.reasoning_effort)
            {
                left = left.child(
                    pill("reasoning-effort", effort.label())
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(Icon::new(IconName::ChevronDown).size(px(11.)))
                        .on_click(cx.listener(|this, _, window, cx| {
                            if !this.turn.busy {
                                this.models_open = !this.models_open;
                                this.history_open = false;
                                this.settings_open = false;
                                this.fit(window);
                                cx.notify();
                            }
                        })),
                );
            }
        } else {
            left = left.child(div().text_size(px(12.)).child("Quick Ask"));
        }
        let mut controls = div().flex().items_center().gap_1().text_color(muted);
        if self.auth_busy {
            if self.auth_can_cancel {
                controls = controls.child(pill("cancel-signin", "Cancel").on_click(cx.listener(
                    |this, _, window, cx| {
                        this.auth_cancel.store(true, Ordering::Relaxed);
                        this.auth_request_id = this.auth_request_id.wrapping_add(1);
                        this.auth_busy = false;
                        this.status.clear();
                        this.fit(window);
                        cx.notify();
                    },
                )));
            }
        } else if !signed_in || self.models.is_empty() {
            controls = controls.child(
                div()
                    .id("signin")
                    .px_3()
                    .py_1()
                    .rounded_full()
                    .bg(rgb(ACCENT))
                    .text_color(rgb(0xffffff))
                    .text_size(px(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(0x0071e3)))
                    .child(if signed_in {
                        "Reconnect"
                    } else {
                        "Continue with ChatGPT"
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.connect(true, cx))),
            );
        }
        if expanded {
            controls = controls.child(
                icon_button("copy", IconName::Copy, "Copy answer · ⌘⇧C")
                    .on_click(cx.listener(|this, _, _, cx| this.copy_answer(cx))),
            );
        }
        controls = controls
            .child(
                icon_button("history", IconName::GalleryVerticalEnd, "Chat history · ⌘H").on_click(
                    cx.listener(|this, _, window, cx| {
                        this.history_open = !this.history_open;
                        this.models_open = false;
                        this.settings_open = false;
                        this.fit(window);
                        cx.notify();
                    }),
                ),
            )
            .child(
                icon_button("new", IconName::Plus, "New chat · ⌘N")
                    .on_click(cx.listener(|this, _, window, cx| this.new_chat(window, cx))),
            )
            .child(
                icon_button("settings", IconName::Ellipsis, "Account and usage").on_click(
                    cx.listener(|this, _, window, cx| {
                        this.settings_open = !this.settings_open;
                        this.history_open = false;
                        this.models_open = false;
                        this.fit(window);
                        cx.notify();
                    }),
                ),
            );
        body = body.child(
            div()
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_between()
                .gap_2()
                .px_5()
                .pb_3()
                .child(left)
                .child(drag_surface("toolbar-drag-space").flex_1().h(px(28.)))
                .child(controls),
        );
        if !signed_in && !self.auth_busy {
            body = body.child(
                div()
                    .px_5()
                    .pb_3()
                    .text_size(px(10.))
                    .text_color(muted)
                    .child(format!("Your ChatGPT plan · {} to open", self.shortcut)),
            );
        }
        body
    }
}
