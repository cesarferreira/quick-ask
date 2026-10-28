use anyhow::{Context, Result, anyhow};
use global_hotkey::{
    GlobalHotKeyEvent, GlobalHotKeyManager, HotKeyState,
    hotkey::{Code, HotKey, Modifiers},
};
use gpui::{App, Window};
use objc2::{MainThreadMarker, rc::Retained};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSAutoresizingMaskOptions, NSColor, NSEvent,
    NSGlassEffectView, NSGlassEffectViewStyle, NSScreen, NSView, NSWindow,
};
use objc2_foundation::NSPoint;
use objc2_quartz_core::kCACornerCurveContinuous;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem},
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeEvent {
    Toggle,
    NewChat,
    ManageUsage,
    Quit,
}

/// Lives on the GPUI application thread for the application's entire lifetime.
pub struct NativeShell {
    hotkeys: GlobalHotKeyManager,
    hotkey: Option<HotKey>,
    shortcut: &'static str,
    shortcut_warning: Option<String>,
    _tray: TrayIcon,
    _menu: Menu,
    toggle_id: MenuId,
    new_chat_id: MenuId,
    manage_usage_id: MenuId,
    quit_id: MenuId,
}

impl NativeShell {
    /// Create this after GPUI has started its main-thread application event loop.
    pub fn new() -> Result<Self> {
        MainThreadMarker::new().ok_or_else(|| {
            anyhow!("The menu bar and keyboard shortcut must start on the main thread")
        })?;
        let hotkeys = GlobalHotKeyManager::new()
            .context("Could not initialize the global keyboard shortcut")?;
        let preferred = HotKey::new(Some(Modifiers::ALT), Code::Space);
        let fallback = HotKey::new(Some(Modifiers::CONTROL | Modifiers::ALT), Code::Space);
        let (hotkey, shortcut, shortcut_warning) = match hotkeys.register(preferred) {
            Ok(()) => (Some(preferred), "⌥ Space", None),
            Err(preferred_error) => {
                if hotkeys.register(fallback).is_err() {
                    (None, "Menu bar", Some("Both Option+Space and Control+Option+Space are in use. Open Quick Ask from its menu bar icon; free one shortcut in the other app, then restart Quick Ask.".into()))
                } else {
                    (
                        Some(fallback),
                        "⌃ ⌥ Space",
                        Some(format!(
                            "Option+Space is unavailable ({preferred_error}). Quick Ask is using \
                         Control+Option+Space. To use Option+Space, disable it in ChatGPT Classic \
                         or the app using it, then reopen Quick Ask."
                        )),
                    )
                }
            }
        };

        let toggle = MenuItem::new(format!("Show / Hide Quick Ask   {shortcut}"), true, None);
        let new_chat = MenuItem::new("New Chat", true, None);
        let usage = MenuItem::new("Manage ChatGPT Usage…", true, None);
        let quit = MenuItem::new("Quit Quick Ask", true, None);
        let menu = Menu::with_items(&[
            &toggle,
            &new_chat,
            &PredefinedMenuItem::separator(),
            &usage,
            &PredefinedMenuItem::separator(),
            &quit,
        ])
        .context("Could not create the Quick Ask menu")?;
        let tray = TrayIconBuilder::new()
            .with_id("quick-ask")
            .with_tooltip(format!("Quick Ask — {shortcut}"))
            .with_icon(menu_bar_icon()?)
            .with_icon_as_template(true)
            .with_menu(Box::new(menu.clone()))
            .build()
            .context("Could not create the Quick Ask menu bar icon")?;

        Ok(Self {
            hotkeys,
            hotkey,
            shortcut,
            shortcut_warning,
            _tray: tray,
            _menu: menu,
            toggle_id: toggle.id().clone(),
            new_chat_id: new_chat.id().clone(),
            manage_usage_id: usage.id().clone(),
            quit_id: quit.id().clone(),
        })
    }

    /// Drain native event queues from a short GPUI main-thread timer.
    pub fn poll(&self) -> Vec<NativeEvent> {
        let mut events = Vec::new();
        for event in GlobalHotKeyEvent::receiver().try_iter() {
            if self.hotkey.is_some_and(|hotkey| event.id == hotkey.id())
                && event.state == HotKeyState::Pressed
            {
                events.push(NativeEvent::Toggle);
            }
        }
        for event in MenuEvent::receiver().try_iter() {
            let event = if event.id == self.toggle_id {
                NativeEvent::Toggle
            } else if event.id == self.new_chat_id {
                NativeEvent::NewChat
            } else if event.id == self.manage_usage_id {
                NativeEvent::ManageUsage
            } else if event.id == self.quit_id {
                NativeEvent::Quit
            } else {
                continue;
            };
            events.push(event);
        }
        events
    }

    pub fn shortcut_label(&self) -> String {
        self.shortcut.into()
    }

    pub fn shortcut_warning(&self) -> Option<&str> {
        self.shortcut_warning.as_deref()
    }
}

impl Drop for NativeShell {
    fn drop(&mut self) {
        if let Some(hotkey) = self.hotkey {
            let _ = self.hotkeys.unregister(hotkey);
        }
    }
}

/// Keep the resident application out of the Dock and the application switcher.
pub fn prepare_app() {
    let mtm = MainThreadMarker::new().expect("Quick Ask must start on the main thread");
    NSApplication::sharedApplication(mtm)
        .setActivationPolicy(NSApplicationActivationPolicy::Accessory);
}

/// Ordering out a nonactivating NSPanel returns key focus to the previous app.
pub fn hide_panel(window: &mut Window) {
    if let Some(native) = native_window(window) {
        native.orderOut(None);
    }
}

pub fn panel_visible(window: &Window) -> bool {
    native_window(window).is_some_and(|native| native.isVisible())
}

/// Let AppKit composite a real glass surface beneath the transparent Metal view.
pub fn install_glass(window: &mut Window) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let Some(native) = native_window(window) else {
        return;
    };
    native.setHasShadow(true);
    let Some(content) = native.contentView() else {
        return;
    };
    // Clip the Metal content with the same continuous curve AppKit uses for
    // glass. A separate GPUI circular radius produces a second visible edge.
    content.setWantsLayer(true);
    if let Some(layer) = content.layer() {
        layer.setCornerRadius(26.0);
        layer.setCornerCurve(unsafe { kCACornerCurveContinuous });
        layer.setMasksToBounds(true);
    }
    if objc2::runtime::AnyClass::get(c"NSGlassEffectView").is_none() {
        window.set_background_appearance(gpui::WindowBackgroundAppearance::Blurred);
        return;
    }
    let glass = NSGlassEffectView::new(mtm);
    glass.setFrame(content.frame());
    glass.setCornerRadius(26.0);
    glass.setStyle(NSGlassEffectViewStyle::Regular);
    glass.setAutoresizingMask(
        NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
    );
    native.setOpaque(false);
    native.setBackgroundColor(Some(&NSColor::clearColor()));
    native.setContentView(Some(&glass));
    glass.setContentView(Some(&content));
    window.refresh();
}

/// Position only when opening; expanding a visible conversation keeps its location.
pub fn show_panel(window: &mut Window, _cx: &mut App) {
    let Some(native) = native_window(window) else {
        window.activate_window();
        return;
    };
    if !native.isVisible() {
        position_on_cursor_display(&native);
    }
    // GPUI's WindowKind::PopUp is a nonactivating NSPanel. Making that panel key
    // accepts typing while the user's previous application remains active.
    native.makeKeyAndOrderFront(None);
    if let Ok(handle) = HasWindowHandle::window_handle(window)
        && let RawWindowHandle::AppKit(handle) = handle.as_raw()
    {
        // Wrapping the content in AppKit glass changes the window's responder;
        // keyboard events must continue to enter GPUI through its original view.
        let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
        native.makeFirstResponder(Some(view));
    }
    window.refresh();
}

fn native_window(window: &Window) -> Option<Retained<NSWindow>> {
    MainThreadMarker::new()?;
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
        return None;
    };
    // GPUI supplies a live NSView pointer for this borrowed window handle. The
    // call is main-thread-only, and retaining its NSWindow keeps the result alive.
    let view = unsafe { handle.ns_view.cast::<NSView>().as_ref() };
    view.window()
}

fn position_on_cursor_display(window: &NSWindow) {
    let Some(mtm) = MainThreadMarker::new() else {
        return;
    };
    let cursor = NSEvent::mouseLocation();
    let screen = NSScreen::screens(mtm)
        .into_iter()
        .find(|screen| {
            let frame = screen.frame();
            cursor.x >= frame.origin.x
                && cursor.x < frame.origin.x + frame.size.width
                && cursor.y >= frame.origin.y
                && cursor.y < frame.origin.y + frame.size.height
        })
        .or_else(|| NSScreen::mainScreen(mtm));
    let Some(screen) = screen else {
        return;
    };
    // AppKit uses logical points with a bottom-left origin for both screen and
    // window frames, including displays left of or below the primary display.
    let screen = screen.visibleFrame();
    let frame = window.frame();
    let padding = 20.0;
    let left = screen.origin.x + padding;
    let right = (screen.origin.x + screen.size.width - frame.size.width - padding).max(left);
    let bottom = screen.origin.y + padding;
    let top = (screen.origin.y + screen.size.height - frame.size.height - padding).max(bottom);
    let x = (screen.origin.x + (screen.size.width - frame.size.width) / 2.0).clamp(left, right);
    let y = (screen.origin.y + screen.size.height * 0.78 - frame.size.height).clamp(bottom, top);
    window.setFrameOrigin(NSPoint::new(x, y));
}

fn menu_bar_icon() -> Result<Icon> {
    const SIDE: usize = 36;
    let mut rgba = vec![0; SIDE * SIDE * 4];
    // A compact outlined speech bubble, rendered at 2× and tinted by AppKit.
    for y in 0..SIDE {
        for x in 0..SIDE {
            let outline = ((6..30).contains(&x) && ((6..9).contains(&y) || (25..28).contains(&y)))
                || ((6..28).contains(&y) && ((4..7).contains(&x) || (29..32).contains(&x)));
            let tail = (27..33).contains(&y) && (9..=13).contains(&x) && x + y <= 42;
            let dots = (15..19).contains(&y)
                && ((11..14).contains(&x) || (17..20).contains(&x) || (23..26).contains(&x));
            if outline || tail || dots {
                rgba[(y * SIDE + x) * 4 + 3] = 255;
            }
        }
    }
    Icon::from_rgba(rgba, SIDE as u32, SIDE as u32).context("Could not draw the menu bar icon")
}
