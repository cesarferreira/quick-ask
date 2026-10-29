mod attachments;
mod downloads;
mod draft_images;
mod image_http;
mod live_markdown;
mod native;
mod turn;
mod ui;

use gpui::*;
use gpui_component::{Root, Theme, ThemeMode};
use quick_ask_core::auth::AuthManager;
use std::{
    fs::OpenOptions,
    sync::{Arc, Mutex},
};

struct PanelView(Entity<ui::QuickAsk>);
impl Global for PanelView {}

fn main() -> anyhow::Result<()> {
    let data_dir = dirs::data_local_dir()
        .ok_or_else(|| anyhow::anyhow!("Could not locate Application Support"))?
        .join("Quick Ask");
    std::fs::create_dir_all(&data_dir)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join("app.lock"))?;
    if lock.try_lock().is_err() {
        eprintln!("Quick Ask is already running. Open it from the menu bar or press its shortcut.");
        return Ok(());
    }
    let auth = Arc::new(Mutex::new(AuthManager::new(data_dir.clone())?));
    let app = Application::new()
        .with_assets(image_http::ImageAssets)
        .with_http_client(Arc::new(image_http::ImageHttpClient::new()?));
    app.on_reopen(|cx| {
        if let Some(handle) = cx.windows().first().copied() {
            let view = cx.try_global::<PanelView>().map(|panel| panel.0.clone());
            let _ = handle.update(cx, |_, window, cx| {
                if let Some(view) = view {
                    view.update(cx, |view, cx| view.show(window, cx));
                }
            });
        }
    });
    app.run(move |cx| {
        struct ProcessLock {
            _file: std::fs::File,
        }
        impl Global for ProcessLock {}
        cx.set_global(ProcessLock { _file: lock });
        native::prepare_app();
        gpui_component::init(cx);
        let shell = native::NativeShell::new();
        let bounds = Bounds::centered(None, size(px(620.), px(156.)), cx);
        let result = cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: None,
                kind: WindowKind::PopUp,
                show: false,
                focus: false,
                is_resizable: false,
                is_minimizable: false,
                window_background: WindowBackgroundAppearance::Transparent,
                app_id: Some("dev.cesar.quickask".into()),
                ..Default::default()
            },
            |window, cx| {
                Theme::change(ThemeMode::from(window.appearance()), Some(window), cx);
                Theme::global_mut(cx).background = transparent_black();
                Theme::global_mut(cx).font_size = px(14.);
                Theme::global_mut(cx).muted_foreground = if Theme::global(cx).mode.is_dark() {
                    rgb(0xb6b6c0).into()
                } else {
                    rgb(0x60606a).into()
                };
                window.set_window_title("Quick Ask");
                let view = cx.new(|cx| ui::QuickAsk::new(window, cx, auth, data_dir, shell));
                cx.set_global(PanelView(view.clone()));
                let root = cx.new(|cx| Root::new(view.clone(), window, cx));
                native::install_glass(window);
                view.update(cx, |view, cx| view.show(window, cx));
                root
            },
        );
        if let Err(error) = result {
            eprintln!("Could not open Quick Ask: {error}");
            cx.quit();
        }
    });
    Ok(())
}
