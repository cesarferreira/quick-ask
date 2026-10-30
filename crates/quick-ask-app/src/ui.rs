use crate::{
    attachments::AttachmentStore,
    downloads,
    draft_images::DraftImages,
    image_http,
    live_markdown::{self, BlockKind},
    native::{self, NativeEvent, NativeShell},
    turn::Turn,
};
use gpui::{prelude::*, *};
use gpui_component::{
    ActiveTheme, Icon, IconName, IconNamed, Theme, ThemeMode,
    input::{Input, InputEvent, InputState},
    text::{TextView, TextViewStyle},
};
use quick_ask_core::{
    auth::{AccountSummary, AuthManager},
    chat::{self, ImageAttachment, Message, Model, ReplyOptions},
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
const DONE_FEEDBACK: Duration = Duration::from_millis(1500);

/// The button that last finished a copy or download; it shows a check until the feedback expires.
#[derive(Default)]
struct DoneButton {
    id: Option<ElementId>,
    generation: u64,
}

impl Global for DoneButton {}

struct DownloadIcon;

impl IconNamed for DownloadIcon {
    fn path(self) -> SharedString {
        image_http::DOWNLOAD_ICON_PATH.into()
    }
}

/// An uploaded or rendered image shown full size in the panel.
#[derive(Clone)]
struct PreviewImage {
    name: String,
    source: PreviewSource,
}

#[derive(Clone)]
enum PreviewSource {
    Local(PathBuf),
    Remote(SharedString),
}

impl PreviewImage {
    fn local(image: &ImageAttachment) -> Self {
        Self {
            name: image.name.clone(),
            source: PreviewSource::Local(image.path.clone()),
        }
    }

    fn remote(url: &str, alt: &str) -> Self {
        let name = if !alt.trim().is_empty() {
            alt.trim().to_owned()
        } else if image_http::is_data_uri(url) {
            "Image".into()
        } else {
            url.to_owned()
        };
        Self {
            name,
            source: PreviewSource::Remote(url.to_owned().into()),
        }
    }

    fn element(&self) -> Img {
        match &self.source {
            PreviewSource::Local(path) => img(path.clone()),
            PreviewSource::Remote(url) => img(url.clone()),
        }
    }
}

enum Event {
    Status(u64, String),
    Connected(u64, bool, Result<(AccountSummary, Vec<Model>), String>),
    SignedOut(Result<(), String>),
    Delta(u64, String),
    Finished(u64, Result<(), String>),
    ImagesImported(u64, Vec<ImageAttachment>, Vec<String>),
}

pub struct QuickAsk {
    input: Entity<InputState>,
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
    image_store: AttachmentStore,
    draft_images: DraftImages,
    turn_images: Vec<ImageAttachment>,
    preview: Option<PreviewImage>,
    _subscriptions: Vec<Subscription>,
}

impl QuickAsk {
    fn import_images(
        &mut self,
        source: native::ClipboardImages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.account.is_none() || self.auth_busy {
            self.error = Some("Sign in before attaching images.".into());
            self.fit(window);
            cx.notify();
            return;
        }
        let count = match &source {
            native::ClipboardImages::Paths(paths) => paths.len(),
            native::ClipboardImages::Bytes(_) => 1,
        };
        let ticket = match self.draft_images.begin_import(count) {
            Ok(ticket) => ticket,
            Err(error) => {
                self.error = Some(error.to_string());
                self.fit(window);
                cx.notify();
                return;
            }
        };
        let store = self.image_store.clone();
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let results = match source {
                native::ClipboardImages::Paths(paths) => paths
                    .iter()
                    .map(|path| store.import_path(path))
                    .collect::<Vec<_>>(),
                native::ClipboardImages::Bytes(bytes) => {
                    vec![store.import_bytes("Pasted image", &bytes)]
                }
            };
            let mut images = Vec::new();
            let mut errors = Vec::new();
            for result in results {
                match result {
                    Ok(image) => images.push(image),
                    Err(error) => errors.push(error.to_string()),
                }
            }
            let _ = tx.send(Event::ImagesImported(ticket, images, errors));
        });
        self.error = None;
        self.show(window, cx);
        self.fit(window);
        cx.notify();
    }

    fn draft_images_view(&self, cx: &mut Context<Self>) -> AnyElement {
        let mut row = div()
            .id("draft-images")
            .flex_shrink_0()
            .flex()
            .items_center()
            .gap_2()
            .px_5()
            .pt_3();
        for image in &self.draft_images.images {
            let path = image.path.clone();
            let name = image.name.clone();
            let preview = PreviewImage::local(image);
            row = row.child(
                div()
                    .id(SharedString::from(format!("draft-{}", path.display())))
                    .relative()
                    .w(px(80.))
                    .h(px(72.))
                    .rounded_lg()
                    .overflow_hidden()
                    .bg(rgba(0x80808025))
                    .cursor_pointer()
                    .tooltip(move |window, cx| {
                        gpui_component::tooltip::Tooltip::new(name.clone()).build(window, cx)
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_preview(preview.clone(), window, cx);
                    }))
                    .child(img(path.clone()).size_full().object_fit(ObjectFit::Cover))
                    .child(
                        div()
                            .id("remove-image")
                            .absolute()
                            .top_1()
                            .right_1()
                            .size(px(20.))
                            .rounded_full()
                            .bg(rgba(0x18181bcc))
                            .text_color(rgb(0xffffff))
                            .flex()
                            .items_center()
                            .justify_center()
                            .cursor_pointer()
                            .hover(|style| style.bg(rgb(0x0a84ff)))
                            .child(Icon::new(IconName::Close).size(px(12.)))
                            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.draft_images.remove(&path);
                                this.fit(window);
                                cx.notify();
                                cx.stop_propagation();
                            })),
                    ),
            );
        }
        if self.draft_images.loading {
            row = row.child(activity_indicator("Preparing…", cx));
        }
        row.into_any_element()
    }

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
                cx.notify();
            }
        });
        let activation = cx.observe_window_activation(window, |_, window, cx| {
            if !window.is_window_active() && native::panel_visible(window) {
                cx.spawn_in(window, async move |this, cx| {
                    loop {
                        Timer::after(Duration::from_millis(120)).await;
                        let keep_waiting = this
                            .update_in(cx, |_, window, cx| {
                                if window.is_window_active() || !native::panel_visible(window) {
                                    return false;
                                }
                                if native::mouse_button_pressed() {
                                    return true;
                                }
                                native::hide_panel(window);
                                cx.notify();
                                false
                            })
                            .unwrap_or(false);
                        if !keep_waiting {
                            break;
                        }
                    }
                })
                .detach();
            }
        });

        let mut last_size = window.bounds().size;
        let bounds = cx.observe_window_bounds(window, move |_, window, _| {
            let size = window.bounds().size;
            if size != last_size {
                last_size = size;
                native::keep_panel_on_screen(window);
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
        let image_store = AttachmentStore::new(data_dir.clone());
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
            image_store,
            draft_images: DraftImages::default(),
            turn_images: vec![],
            preview: None,
            _subscriptions: vec![subscription, activation, appearance, bounds],
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
        if (prompt.is_empty() && self.draft_images.images.is_empty())
            || self.turn.busy
            || self.auth_busy
        {
            return;
        }
        if self.draft_images.loading {
            self.error = Some("Wait for the images to finish preparing before sending.".into());
            self.fit(window);
            cx.notify();
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
        self.turn_images = self.draft_images.take();
        self.follow_reply = true;
        self.scroll.scroll_to_bottom();
        self.history_open = false;
        self.models_open = false;
        self.settings_open = false;
        self.preview = None;
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
            images: self.turn_images.clone(),
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
        self.draft_images.clear();
        self.turn_images.clear();
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
        self.preview = None;
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
        let extra = if self.error.is_some() || self.notice.is_some() {
            100.
        } else {
            0.
        };
        let height = if self.preview.is_some() {
            580.
        } else if self.settings_open {
            280. + extra
        } else if self.history_open {
            (180. + self.conversations.len().max(1) as f32 * 64.).min(540.)
        } else if self.models_open {
            (268. + self.models.len() as f32 * 36.).min(540.)
        } else {
            // Chat height is measured after layout, including wrapped input and Markdown.
            return;
        };
        let target = size(
            px(620.),
            px((height
                + if self.draft_images.loading || !self.draft_images.images.is_empty() {
                    88.
                } else {
                    0.
                })
            .min(620.)),
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
            self.conversation.title = if first.content.trim().is_empty() {
                first
                    .images
                    .first()
                    .map(|image| image.name.clone())
                    .unwrap_or_else(|| "Image chat".into())
            } else {
                first.content.chars().take(70).collect()
            };
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
                Event::ImagesImported(ticket, images, errors) => {
                    if self.draft_images.complete(ticket, images) && !errors.is_empty() {
                        self.error = Some(errors.join("\n"));
                    }
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
                images: std::mem::take(&mut self.turn_images),
            });
            self.conversation.messages.push(Message {
                role: "assistant".into(),
                content: reply,
                images: vec![],
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
            copy_to_clipboard("copy".into(), text, cx);
        }
    }

    fn open_preview(&mut self, image: PreviewImage, window: &mut Window, cx: &mut Context<Self>) {
        self.history_open = false;
        self.models_open = false;
        self.settings_open = false;
        self.preview = Some(image);
        self.fit(window);
        cx.notify();
    }

    fn close_preview(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.preview = None;
        self.fit(window);
        cx.notify();
    }

    fn download_preview(&mut self, cx: &mut Context<Self>) {
        let Some(preview) = self.preview.clone() else {
            return;
        };
        let Some(directory) = dirs::download_dir() else {
            self.error = Some("Could not find your Downloads folder.".into());
            cx.notify();
            return;
        };
        let http = cx.http_client();
        let executor = cx.background_executor().clone();
        cx.spawn(async move |this, cx| {
            let saved = async {
                let bytes = match preview.source {
                    PreviewSource::Local(path) => {
                        executor.spawn(async move { std::fs::read(path) }).await?
                    }
                    PreviewSource::Remote(url) if image_http::is_data_uri(&url) => {
                        image_http::decode_data_image(&url)?.1
                    }
                    PreviewSource::Remote(url) => {
                        let response = http
                            .get(&url, gpui::http_client::AsyncBody::empty(), true)
                            .await?;
                        anyhow::ensure!(
                            response.status().is_success(),
                            "the server returned {}",
                            response.status()
                        );
                        let mut bytes = Vec::new();
                        futures_lite::AsyncReadExt::read_to_end(
                            &mut response.into_body(),
                            &mut bytes,
                        )
                        .await?;
                        bytes
                    }
                };
                executor
                    .spawn(async move { downloads::save_image(&directory, &preview.name, &bytes) })
                    .await
            }
            .await;
            this.update(cx, |this, cx| {
                match saved {
                    Ok(_) => mark_done("download-image".into(), cx),
                    Err(error) => this.error = Some(format!("Could not save the image: {error:#}")),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn sign_out(&mut self, cx: &mut Context<Self>) {
        if self.auth_busy {
            return;
        }
        self.cancel.store(true, Ordering::Relaxed);
        self.turn.reset();
        self.draft_images.clear();
        self.turn_images.clear();
        self.preview = None;
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
                        this.draft_images.clear();
                        this.turn_images.clear();
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
        let view = cx.entity();
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
            list = list.child(message_view(index, message, &view, window, cx));
        }
        if let Some(prompt) = &self.turn.prompt {
            list = list.child(message_view(
                self.conversation.messages.len(),
                &Message {
                    role: "user".into(),
                    content: prompt.clone(),
                    images: self.turn_images.clone(),
                },
                &view,
                window,
                cx,
            ));
            if !self.turn.visible_text().is_empty() {
                list = list.child(answer_view(
                    self.conversation.messages.len() + 1,
                    self.turn.visible_text(),
                    Some(self.turn.id),
                    &view,
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

fn icon_button(
    id: impl Into<ElementId>,
    icon: impl Into<Icon>,
    label: &'static str,
) -> Stateful<Div> {
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

fn copy_to_clipboard(id: ElementId, text: String, cx: &mut App) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    mark_done(id, cx);
}

fn mark_done(id: ElementId, cx: &mut App) {
    let done = cx.default_global::<DoneButton>();
    done.generation += 1;
    done.id = Some(id);
    let generation = done.generation;
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        Timer::after(DONE_FEEDBACK).await;
        cx.update(|cx| {
            let done = cx.default_global::<DoneButton>();
            if done.generation == generation {
                done.id = None;
                cx.refresh_windows();
            }
        })
        .ok();
    })
    .detach();
}

/// Shows a check in place of `icon` while the button's action has just finished.
fn done_icon(id: &ElementId, icon: impl Into<Icon>, cx: &App) -> Icon {
    let done = cx
        .try_global::<DoneButton>()
        .is_some_and(|done| done.id.as_ref() == Some(id));
    if done {
        IconName::Check.into()
    } else {
        icon.into()
    }
}

fn copy_button(
    id: impl Into<ElementId>,
    text: SharedString,
    label: &'static str,
    cx: &App,
) -> Stateful<Div> {
    let id = id.into();
    icon_button(id.clone(), done_icon(&id, IconName::Copy, cx), label)
        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
        .on_click(move |_, _, cx| {
            copy_to_clipboard(id.clone(), text.to_string(), cx);
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
        .flex_shrink_0()
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

fn message_view(
    index: usize,
    message: &Message,
    view: &Entity<QuickAsk>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    if message.role == "user" {
        let mut question = div()
            .id(("question", index))
            .flex_shrink_0()
            .flex()
            .flex_col()
            .items_end()
            .gap_2();
        if !message.images.is_empty() {
            question = question.child(
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap_2()
                    .max_w(px(530.))
                    .children(
                        message
                            .images
                            .iter()
                            .enumerate()
                            .map(|(image_index, image)| {
                                let name = image.name.clone();
                                let preview = PreviewImage::local(image);
                                let view = view.clone();
                                div()
                                    .id(("question-image", image_index))
                                    .cursor_pointer()
                                    .on_click(move |_, window, cx| {
                                        view.update(cx, |this, cx| {
                                            this.open_preview(preview.clone(), window, cx)
                                        });
                                    })
                                    .w(px(156.))
                                    .h(px(104.))
                                    .rounded_lg()
                                    .overflow_hidden()
                                    .bg(rgba(0x80808015))
                                    .tooltip(move |window, cx| {
                                        gpui_component::tooltip::Tooltip::new(name.clone())
                                            .build(window, cx)
                                    })
                                    .child(
                                        img(image.path.clone())
                                            .size_full()
                                            .object_fit(ObjectFit::Contain),
                                    )
                            }),
                    ),
            );
        }
        if !message.content.is_empty() {
            question = question
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
                        ("copy-question", index),
                        message.content.clone().into(),
                        "Copy question",
                        cx,
                    )
                    .text_color(cx.theme().muted_foreground),
                );
        }
        question.into_any_element()
    } else {
        answer_view(index, &message.content, None, view, window, cx)
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
            copy_button(("copy-code", code.as_ptr() as usize), code, "Copy code", cx)
                .text_color(cx.theme().muted_foreground)
        })
}

fn answer_view(
    index: usize,
    text: &str,
    live_turn: Option<u64>,
    view: &Entity<QuickAsk>,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let text: SharedString = text.to_owned().into();
    let mut answer = div()
        .id(("response", index))
        .flex_shrink_0()
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
                                        ("copy-live-code", block.start),
                                        body.to_owned().into(),
                                        "Copy code",
                                        cx,
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
                BlockKind::Image { alt, url } => {
                    answer = answer.child(answer_image(block.start, alt, url, view));
                }
            }
        }
    } else {
        // Keep text between images in one view so selection spans paragraphs.
        let mut markdown_start = 0;
        for block in live_markdown::blocks(&text) {
            if let BlockKind::Image { alt, url } = block.kind {
                let markdown = &text[markdown_start..block.start];
                if !markdown.trim().is_empty() {
                    answer = answer.child(markdown_view(
                        SharedString::from(format!("answer-{index}-{markdown_start}")),
                        markdown.to_owned().into(),
                        window,
                        cx,
                    ));
                }
                answer = answer.child(answer_image(block.start, alt, url, view));
                markdown_start = block.start + block.source.len();
            }
        }
        let markdown = &text[markdown_start..];
        if !markdown.trim().is_empty() {
            answer = answer.child(markdown_view(
                SharedString::from(format!("answer-{index}-{markdown_start}")),
                markdown.to_owned().into(),
                window,
                cx,
            ));
        }
    }
    answer
        .child(
            div().child(
                copy_button(
                    ("copy-response", index),
                    text,
                    "Copy response as Markdown",
                    cx,
                )
                .text_color(cx.theme().muted_foreground),
            ),
        )
        .into_any_element()
}

fn answer_image(start: usize, alt: &str, url: &str, view: &Entity<QuickAsk>) -> AnyElement {
    let preview = PreviewImage::remote(url, alt);
    let view = view.clone();
    div()
        .id(("answer-image", start))
        .cursor_pointer()
        .on_click(move |_, window, cx| {
            view.update(cx, |this, cx| {
                this.open_preview(preview.clone(), window, cx)
            });
        })
        .child(
            img(SharedString::from(url.to_owned()))
                .object_fit(ObjectFit::Contain)
                .max_w(relative(1.)),
        )
        .into_any_element()
}

impl Render for QuickAsk {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let expanded = !self.conversation.messages.is_empty() || self.turn.prompt.is_some();
        let has_section =
            self.history_open || self.models_open || self.settings_open || self.preview.is_some();
        let signed_in = self.account.is_some();
        let foreground = cx.theme().foreground;
        let muted = cx.theme().muted_foreground;
        let glass_tint = if cx.theme().mode.is_dark() {
            rgba(0x10101455)
        } else {
            rgba(0xffffffa8)
        };
        let scroll = self.scroll.clone();
        let mut body = div()
            .key_context("QuickAsk")
            .size_full()
            .flex()
            .flex_col()
            .bg(glass_tint)
            .text_color(foreground)
            .font_family(".SystemUIFont")
            .overflow_hidden()
            .when(!has_section, |body| {
                body.on_children_prepainted(move |bounds, window, cx| {
                    let mut height = bounds
                        .iter()
                        .fold(px(0.), |height, bounds| height + bounds.size.height);
                    if expanded {
                        // Replace the flexible viewport with its full content extent. This
                        // also measures content shorter than the current viewport so it shrinks.
                        let content = scroll
                            .bounds_for_item(0)
                            .zip(scroll.bounds_for_item(scroll.children_count().saturating_sub(1)))
                            .map(|(first, last)| last.bottom() - first.top())
                            .unwrap_or_default();
                        height += content + window.rem_size() * 2. - scroll.bounds().size.height;
                    }
                    let minimum = px(if signed_in { 132. } else { 156. });
                    let target = size(px(620.), height.ceil().clamp(minimum, px(620.)));
                    if window.bounds().size != target {
                        window.defer(cx, move |window, _| window.resize(target));
                    }
                })
            })
            .id("quick-ask-panel")
            .on_drop(cx.listener(|this, paths: &ExternalPaths, window, cx| {
                this.import_images(
                    native::ClipboardImages::Paths(paths.paths().to_vec()),
                    window,
                    cx,
                );
            }))
            .drag_over::<ExternalPaths>(|style, _, _, _| style.bg(rgba(0x0a84ff25)))
            .capture_action(
                cx.listener(|this, _: &gpui_component::input::Paste, window, cx| {
                    match native::clipboard_images() {
                        Ok(Some(source)) => {
                            this.import_images(source, window, cx);
                            cx.stop_propagation();
                        }
                        Ok(None) => cx.propagate(),
                        Err(error) => {
                            this.error = Some(error.to_string());
                            this.fit(window);
                            cx.notify();
                            cx.stop_propagation();
                        }
                    }
                }),
            )
            .capture_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                if event.keystroke.key == "escape" {
                    if this.preview.is_some() {
                        this.close_preview(window, cx);
                    } else {
                        native::hide_panel(window);
                    }
                    cx.stop_propagation();
                }
            }))
            .on_action(cx.listener(|_, _: &Dismiss, window, _| native::hide_panel(window)))
            .on_action(cx.listener(|this, _: &NewChat, window, cx| this.new_chat(window, cx)))
            .on_action(cx.listener(|this, _: &ShowHistory, window, cx| {
                this.history_open = !this.history_open;
                this.models_open = false;
                this.settings_open = false;
                this.preview = None;
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
            let title: SharedString = if self.history_open {
                "Chats".into()
            } else if self.models_open {
                "Model & reasoning".into()
            } else if self.settings_open {
                "Account".into()
            } else if let Some(preview) = &self.preview {
                SharedString::from(preview.name.clone())
            } else {
                "Quick Ask".into()
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
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
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
                                            this.preview = None;
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
                                    .min_w_0()
                                    .truncate()
                                    .child(title),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .text_color(muted)
                            .when(self.preview.is_some(), |s| {
                                s.child(
                                    icon_button(
                                        "download-image",
                                        done_icon(&"download-image".into(), DownloadIcon, cx),
                                        "Save to Downloads",
                                    )
                                    .on_click(
                                        cx.listener(|this, _, _, cx| this.download_preview(cx)),
                                    ),
                                )
                            })
                            .child(
                                icon_button("dismiss", IconName::Close, "Dismiss · Esc")
                                    .on_click(|_, window, _| native::hide_panel(window)),
                            ),
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
        } else if let Some(preview) = &self.preview {
            body = body.child(
                div()
                    .id("image-preview")
                    .flex_1()
                    .min_h_0()
                    .px_5()
                    .pb_3()
                    .child(preview.element().size_full().object_fit(ObjectFit::Contain)),
            );
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

        if self.draft_images.loading || !self.draft_images.images.is_empty() {
            body = body.child(self.draft_images_view(cx));
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
                            this.preview = None;
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
                                this.preview = None;
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
                icon_button(
                    "copy",
                    done_icon(&"copy".into(), IconName::Copy, cx),
                    "Copy answer · ⌘⇧C",
                )
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
                        this.preview = None;
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
                        this.preview = None;
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
