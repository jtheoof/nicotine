//! Windows tray icon: a notification-area icon via tray-icon.
//!
//! tray-icon delivers clicks to a hidden window owned by the thread that
//! created the icon, so the icon lives on a dedicated thread running its
//! own message loop rather than depending on iced's event loop.

use super::{dispatch, Action, ICON_32};
use crate::config::LiveSettings;
use std::sync::{Arc, Mutex, Once};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
use tray_icon::{MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostThreadMessageW, TranslateMessage, MSG, WM_QUIT,
};

const MENU_ACTIONS: [(&str, &str, Action); 4] = [
    ("open", "Open Nicotine", Action::Open),
    ("toggle-previews", "Toggle previews", Action::TogglePreviews),
    ("restack", "Restack EVE windows", Action::Restack),
    ("quit", "Quit", Action::Quit),
];

/// tray-icon's event handlers are process-wide and can only be set once,
/// so they're installed on the first spawn and reused by later icons.
fn install_handlers() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        MenuEvent::set_event_handler(Some(|event: MenuEvent| {
            if let Some((_, _, action)) = MENU_ACTIONS.iter().find(|(id, ..)| event.id == *id) {
                dispatch(*action);
            }
        }));
        TrayIconEvent::set_event_handler(Some(|event: TrayIconEvent| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                dispatch(Action::Open);
            }
        }));
    });
}

fn build(restack_supported: bool) -> Result<TrayIcon, String> {
    let menu = Menu::new();
    for (id, label, action) in MENU_ACTIONS {
        if action == Action::Restack && !restack_supported {
            continue;
        }
        if action == Action::Quit {
            menu.append(&PredefinedMenuItem::separator())
                .map_err(|e| e.to_string())?;
        }
        menu.append(&MenuItem::with_id(id, label, true, None))
            .map_err(|e| e.to_string())?;
        if action == Action::Open {
            menu.append(&PredefinedMenuItem::separator())
                .map_err(|e| e.to_string())?;
        }
    }
    let icon = tray_icon::Icon::from_rgba(ICON_32.to_vec(), 32, 32).map_err(|e| e.to_string())?;
    TrayIconBuilder::new()
        .with_icon(icon)
        .with_tooltip("Nicotine")
        .with_menu(Box::new(menu))
        .with_menu_on_left_click(false)
        .build()
        .map_err(|e| e.to_string())
}

/// The tray thread; dropping this ends its message loop, which drops the
/// icon and removes it from the notification area.
pub struct Handle {
    thread_id: u32,
}

impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// The Windows menu has a plain "Toggle previews" item, no state to sync.
pub fn refresh() {}

pub fn spawn(_live: Arc<Mutex<LiveSettings>>, restack_supported: bool) -> Result<Handle, String> {
    install_handlers();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("nicotine-tray".into())
        .spawn(move || {
            let tray = match build(restack_supported) {
                Ok(tray) => tray,
                Err(e) => {
                    let _ = tx.send(Err(e));
                    return;
                }
            };
            let _ = tx.send(Ok(unsafe { GetCurrentThreadId() }));
            let mut msg = MSG::default();
            unsafe {
                while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            drop(tray);
        })
        .map_err(|e| e.to_string())?;
    let thread_id = rx.recv().map_err(|_| "tray thread exited".to_string())??;
    Ok(Handle { thread_id })
}
