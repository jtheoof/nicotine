//! Linux tray icon: a StatusNotifierItem served over D-Bus by ksni.

use super::{dispatch, Action, ICON_32, ICON_64};
use crate::config::LiveSettings;
use ksni::blocking::TrayMethods;
use ksni::menu::StandardItem;
use ksni::{Icon, MenuItem, ToolTip};
use std::sync::{Arc, Mutex};

struct SniTray {
    live: Arc<Mutex<LiveSettings>>,
    restack_supported: bool,
}

/// SNI pixmaps are ARGB32 in network byte order; the rasters are RGBA.
fn icon(size: i32, rgba: &[u8]) -> Icon {
    let data = rgba
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|&[r, g, b, a]| [a, r, g, b])
        .collect();
    Icon {
        width: size,
        height: size,
        data,
    }
}

fn item(label: &str, action: Action) -> MenuItem<SniTray> {
    StandardItem {
        label: label.into(),
        activate: Box::new(move |_: &mut SniTray| dispatch(action)),
        ..Default::default()
    }
    .into()
}

impl ksni::Tray for SniTray {
    fn id(&self) -> String {
        "nicotine".into()
    }

    fn title(&self) -> String {
        "Nicotine".into()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        vec![icon(32, ICON_32), icon(64, ICON_64)]
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            title: "Nicotine".into(),
            ..Default::default()
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        dispatch(Action::Open);
    }

    /// Re-read on every `refresh`, so the previews entry follows changes
    /// made anywhere (tray, panel checkbox, toggle-previews hotkey).
    fn menu(&self) -> Vec<MenuItem<Self>> {
        let show_previews = self.live.lock().map(|l| l.show_previews).unwrap_or(true);
        // A label that names the action rather than a checkmark: hosts send
        // only the changed property, and Quickshell (DankMaterialShell,
        // Noctalia, ...) treats such a partial update as a full property
        // set, resetting toggle-type so the checkmark vanishes for good. A
        // label-only change survives that, and reads clearly everywhere.
        let previews_label = if show_previews {
            "Hide previews"
        } else {
            "Show previews"
        };
        let mut items = vec![
            item("Open Nicotine", Action::Open),
            MenuItem::Separator,
            item(previews_label, Action::TogglePreviews),
        ];
        if self.restack_supported {
            items.push(item("Restack EVE windows", Action::Restack));
        }
        items.push(MenuItem::Separator);
        items.push(item("Quit", Action::Quit));
        items
    }

    fn watcher_offline(&self, reason: ksni::OfflineReason) -> bool {
        // Keep serving: the tray reappears if the host comes back (e.g.
        // waybar restarted), and the panel can still be reopened by
        // launching Nicotine again.
        eprintln!("tray host went away: {reason:?}");
        true
    }
}

/// The running tray's service handle, for `refresh` from any thread.
static ACTIVE: Mutex<Option<ksni::blocking::Handle<SniTray>>> = Mutex::new(None);

pub struct Handle(ksni::blocking::Handle<SniTray>);

impl Drop for Handle {
    fn drop(&mut self) {
        *ACTIVE.lock().unwrap() = None;
        // Removes the icon; the service thread winds down on its own.
        let _ = self.0.shutdown();
    }
}

pub fn refresh() {
    let Some(handle) = ACTIVE.lock().unwrap().clone() else {
        return;
    };
    // `update` re-runs `menu()` and signals the host with the new state.
    // It blocks on a D-Bus round-trip, so keep it off the caller's thread
    // (the panel's event loop, the hotkey listener).
    std::thread::spawn(move || {
        handle.update(|_| {});
    });
}

pub fn spawn(live: Arc<Mutex<LiveSettings>>, restack_supported: bool) -> Result<Handle, String> {
    SniTray {
        live,
        restack_supported,
    }
    .spawn()
    .map(|handle| {
        *ACTIVE.lock().unwrap() = Some(handle.clone());
        Handle(handle)
    })
    .map_err(|e| match e {
        ksni::Error::Dbus(e) => format!("couldn't connect to D-Bus: {e}"),
        _ => "no system tray found".into(),
    })
}
