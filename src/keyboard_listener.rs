//! evdev keyboard cycle + per-character hotkey listener with **live**
//! binding reload. See the matching commentary on `mouse_listener` —
//! same shape, same hot-reload contract, plus the character_hotkeys
//! dispatch path that was missing from Linux entirely until now.

use crate::config::{CharacterHotkey, Config, LiveSettings};
use crate::cycle_state::CycleState;
use crate::input_actions::{fetch_bounded, ActionQueue, InputAction};
use crate::window_manager::WindowManager;
use anyhow::Result;
use evdev::{Device, EventType, InputEvent, KeyCode};
use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, PartialEq, Eq)]
pub struct KeyboardConfig {
    pub enable: bool,
    pub forward_key: u16,
    pub backward_key: u16,
    pub modifier_key: Option<u16>,
    pub keyboard_device_path: Option<String>,
    pub minimize_inactive: bool,
    pub character_hotkeys: HashMap<String, CharacterHotkey>,
    pub toggle_previews_key: Option<u16>,
    pub toggle_previews_modifier: Option<u16>,
}

impl KeyboardConfig {
    pub fn from_config(c: &Config) -> Self {
        Self {
            enable: c.enable_keyboard_buttons,
            forward_key: c.forward_key,
            backward_key: c.backward_key,
            modifier_key: c.modifier_key,
            keyboard_device_path: c.keyboard_device_path.clone(),
            minimize_inactive: c.minimize_inactive,
            character_hotkeys: c.character_hotkeys.clone(),
            toggle_previews_key: c.toggle_previews_key,
            toggle_previews_modifier: c.toggle_previews_modifier,
        }
    }

    /// Whether the listener has any reason to stay attached to the input
    /// device. It can idle (and release the device) only when cycling is
    /// off AND no character hotkeys AND no preview-toggle key are bound —
    /// otherwise a press it cares about would be missed.
    fn has_work(&self) -> bool {
        self.enable || !self.character_hotkeys.is_empty() || self.toggle_previews_key.is_some()
    }
}

const POLL_TIMEOUT_MS: u16 = 200;
/// How often autodetect mode looks for keyboards that appeared since the
/// last scan (a wireless keyboard waking up, a hotplugged one).
const RESCAN_INTERVAL: Duration = Duration::from_secs(5);
/// The same key event arriving from two device nodes within this window
/// is one physical press reported twice (some receivers expose a keyboard
/// on more than one interface); act on it once. Far below key-repeat
/// intervals, so repeats are never swallowed.
const ECHO_WINDOW: Duration = Duration::from_millis(15);

/// Last key event acted on, for [`is_echo`].
#[derive(Debug, Clone, Copy)]
struct KeyStamp {
    code: u16,
    value: i32,
    device: usize,
    at: Instant,
}

/// Is this key event a duplicate of `prev` delivered by another device?
fn is_echo(prev: Option<&KeyStamp>, code: u16, value: i32, device: usize, now: Instant) -> bool {
    prev.is_some_and(|p| {
        p.code == code
            && p.value == value
            && p.device != device
            && now.saturating_duration_since(p.at) < ECHO_WINDOW
    })
}

/// `N` of an `eventN` device node name.
fn event_number(name: &str) -> Option<u32> {
    name.strip_prefix("event")?.parse().ok()
}

pub struct KeyboardListener;

impl KeyboardListener {
    pub fn spawn(
        shared: Arc<Mutex<KeyboardConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
        live: Arc<Mutex<LiveSettings>>,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || Self::run_listener(shared, wm, state, live))
    }

    fn run_listener(
        shared: Arc<Mutex<KeyboardConfig>>,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
        live: Arc<Mutex<LiveSettings>>,
    ) {
        // Cycling/switching runs on a worker so a slow window-manager
        // round-trip never stalls reading (see `input_actions`).
        let actions = ActionQueue::spawn("keyboard", wm, state);
        // Every keyboard-capable event node we listen on. Gaming mice and
        // USB receivers expose keyboard interfaces too (for macros), so
        // "the first device with a Tab key" was often a mouse and the real
        // keyboard was never read. Listening on all of them catches the
        // real keyboard, a second one, and remappers' virtual keyboards.
        // Mouse nodes deliver 1 kHz motion here too; that's fine now that
        // reading never blocks on cycling and fetches are bounded.
        let mut devices: Vec<(PathBuf, Device)> = Vec::new();
        let mut current_dev_path: Option<String> = None;
        // Modifier-down state. Reset on (re)connect because the kernel
        // doesn't replay release events for keys held during a device
        // disappearance.
        let mut pressed_modifiers: HashSet<u16> = HashSet::new();
        let mut announced_listening = false;
        let mut announced_idle = false;
        let mut last_scan = Instant::now();
        let mut last_key: Option<KeyStamp> = None;

        loop {
            let snap = shared.lock().unwrap().clone();

            // Character hotkeys and the preview-toggle key are independent
            // dispatch paths from forward/backward cycling — keep the
            // listener live so they fire even when the user hasn't enabled
            // keyboard cycling.
            if !snap.has_work() {
                if !announced_idle {
                    println!(
                        "Keyboard listener idle (enable_keyboard_buttons = false, \
                         no character hotkeys or preview-toggle key bound)"
                    );
                    announced_idle = true;
                    announced_listening = false;
                }
                devices.clear();
                current_dev_path = None;
                pressed_modifiers.clear();
                std::thread::sleep(Duration::from_millis(500));
                continue;
            }

            if devices.is_empty() || current_dev_path != snap.keyboard_device_path {
                devices.clear();
                current_dev_path = snap.keyboard_device_path.clone();
                pressed_modifiers.clear();
                match Self::find_keyboard_devices(snap.keyboard_device_path.as_deref(), &[]) {
                    Ok(found) if !found.is_empty() => {
                        announced_idle = false;
                        announced_listening = false;
                        devices = found;
                        last_scan = Instant::now();
                    }
                    Ok(_) => {
                        eprintln!(
                            "No keyboard devices found; retrying in 2s. Check permissions \
                             on /dev/input/event* and whether your user is in the `input` group."
                        );
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                    Err(e) => {
                        eprintln!("Keyboard device scan failed ({}); retrying in 2s.", e);
                        std::thread::sleep(Duration::from_secs(2));
                        continue;
                    }
                }
            } else if snap.keyboard_device_path.is_none() && last_scan.elapsed() >= RESCAN_INTERVAL
            {
                // Autodetect: add keyboards that appeared since the last
                // scan (wireless wake-up, hotplug).
                last_scan = Instant::now();
                let known: Vec<PathBuf> = devices.iter().map(|(p, _)| p.clone()).collect();
                if let Ok(new) = Self::find_keyboard_devices(None, &known) {
                    if !new.is_empty() {
                        devices.extend(new);
                        announced_listening = false;
                    }
                }
            }

            if !announced_listening {
                println!(
                    "Listening on {} keyboard device(s) for keys: forward={} backward={} \
                     (+{} character hotkey(s))",
                    devices.len(),
                    snap.forward_key,
                    snap.backward_key,
                    snap.character_hotkeys.len()
                );
                announced_listening = true;
            }

            // Re-derive the modifier-code set from the current snapshot.
            // Set of every key code that's used as a modifier somewhere
            // (main modifier_key + every character hotkey's optional
            // modifier). Recomputed each iteration so swapping
            // modifiers in the panel takes effect immediately.
            let modifier_codes: HashSet<u16> = std::iter::empty()
                .chain(snap.modifier_key)
                .chain(snap.toggle_previews_modifier)
                .chain(snap.character_hotkeys.values().filter_map(|hk| hk.modifier))
                .collect();
            // Forget pressed modifiers that are no longer modifiers in
            // any binding — otherwise a key the user un-bound stays
            // "stuck" in the pressed set.
            pressed_modifiers.retain(|c| modifier_codes.contains(c));

            // `pollfds` borrows `devices`; it's dropped before any device
            // is read or removed.
            let mut pollfds: Vec<PollFd> = devices
                .iter()
                .map(|(_, d)| PollFd::new(d.as_fd(), PollFlags::POLLIN))
                .collect();
            let n = match poll(&mut pollfds, PollTimeout::from(POLL_TIMEOUT_MS)) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("Keyboard poll failed ({}); rescanning.", e);
                    devices.clear();
                    continue;
                }
            };
            if n == 0 {
                continue;
            }
            let mut ready: Vec<usize> = Vec::new();
            let mut dead: Vec<usize> = Vec::new();
            for (i, pfd) in pollfds.iter().enumerate() {
                let revents = pfd.revents().unwrap_or_else(PollFlags::empty);
                if revents.contains(PollFlags::POLLIN) {
                    ready.push(i);
                } else if revents
                    .intersects(PollFlags::POLLHUP | PollFlags::POLLERR | PollFlags::POLLNVAL)
                {
                    // POLLNVAL too: an fd poll reports but that isn't
                    // readable would otherwise spin this loop.
                    dead.push(i);
                }
            }
            drop(pollfds);

            let mut batches: Vec<(usize, Vec<InputEvent>)> = Vec::new();
            for &i in &ready {
                match fetch_bounded(&mut devices[i].1) {
                    Ok(events) => batches.push((i, events)),
                    Err(_) => dead.push(i),
                }
            }
            if !dead.is_empty() {
                dead.sort_unstable();
                dead.dedup();
                for &i in dead.iter().rev() {
                    let (path, device) = devices.remove(i);
                    eprintln!(
                        "Keyboard device '{}' ({}) went away; dropping.",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                }
                // A key held on the vanished device never sends its
                // release.
                pressed_modifiers.clear();
                announced_listening = false;
            }

            for (device_idx, events) in batches {
                for event in events {
                    if event.event_type() != EventType::KEY {
                        continue;
                    }
                    let code = event.code();
                    let now = Instant::now();
                    if is_echo(last_key.as_ref(), code, event.value(), device_idx, now) {
                        continue;
                    }
                    last_key = Some(KeyStamp {
                        code,
                        value: event.value(),
                        device: device_idx,
                        at: now,
                    });

                    // Track press/release of modifier-eligible keys.
                    if modifier_codes.contains(&code) {
                        if event.value() != 0 {
                            pressed_modifiers.insert(code);
                        } else {
                            pressed_modifiers.remove(&code);
                        }
                    }

                    // Only act on press / repeat.
                    if event.value() == 0 {
                        continue;
                    }

                    // Preview-visibility toggle. Independent of cycling enable,
                    // and acts only on the initial press (value == 1) so a held
                    // key doesn't strobe show/hide on key-repeat. Consume the
                    // key either way so it can't also trigger cycling/switching.
                    if toggle_previews_should_fire(
                        snap.toggle_previews_key,
                        snap.toggle_previews_modifier,
                        code,
                        &pressed_modifiers,
                    ) {
                        if event.value() == 1 {
                            let mut live = live.lock().unwrap();
                            live.show_previews = !live.show_previews;
                            println!(
                                "Preview windows toggled {} via hotkey",
                                if live.show_previews { "on" } else { "off" }
                            );
                        }
                        continue;
                    }

                    // Modifier+backward must be checked first so the
                    // same-key (Tab + Shift+Tab) pattern works. Skip the
                    // whole cycling block when the user has cycling
                    // disabled but kept the listener alive for character
                    // hotkeys — otherwise Tab would still cycle.
                    if snap.enable {
                        let main_modifier_held = snap
                            .modifier_key
                            .map(|m| pressed_modifiers.contains(&m))
                            .unwrap_or(false);
                        let minimize_inactive = snap.minimize_inactive;
                        if code == snap.backward_key && main_modifier_held {
                            actions.push(InputAction::Backward { minimize_inactive });
                            continue;
                        }
                        if code == snap.forward_key {
                            actions.push(InputAction::Forward { minimize_inactive });
                            continue;
                        }
                        if code == snap.backward_key {
                            actions.push(InputAction::Backward { minimize_inactive });
                            continue;
                        }
                    }

                    let target =
                        resolve_character_hotkey(code, &pressed_modifiers, &snap.character_hotkeys);
                    if let Some(name) = target {
                        actions.push(InputAction::SwitchTo {
                            name,
                            minimize_inactive: snap.minimize_inactive,
                        });
                    }
                }
            }
        }
    }

    /// Open the keyboard device(s) to listen on: the configured
    /// `keyboard_device_path` alone if set and openable, otherwise every
    /// event node advertising keyboard keys, in event-number order,
    /// skipping paths in `skip` (already open).
    fn find_keyboard_devices(
        configured_path: Option<&str>,
        skip: &[PathBuf],
    ) -> Result<Vec<(PathBuf, Device)>> {
        if let Some(path_str) = configured_path {
            let path = Path::new(path_str);
            match Device::open(path) {
                Ok(device) => {
                    println!(
                        "Using configured keyboard device {} ({})",
                        device.name().unwrap_or("Unknown"),
                        path.display()
                    );
                    return Ok(vec![(path.to_path_buf(), device)]);
                }
                Err(e) => {
                    eprintln!(
                        "Warning: Failed to open configured keyboard device '{}': {}",
                        path_str, e
                    );
                    eprintln!("Falling back to automatic device detection...");
                }
            }
        }

        let mut event_paths: Vec<(u32, PathBuf)> = std::fs::read_dir("/dev/input")?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter_map(|p| {
                let n = event_number(p.file_name()?.to_str()?)?;
                Some((n, p))
            })
            .collect();
        // Numeric, so event7 comes before event11.
        event_paths.sort();

        let mut found = Vec::new();
        for (_, path) in event_paths {
            if skip.contains(&path) {
                continue;
            }
            let Ok(device) = Device::open(&path) else {
                continue;
            };
            if device.supported_keys().is_some_and(|keys| {
                keys.contains(KeyCode::KEY_TAB)
                    || keys.contains(KeyCode::KEY_LEFTSHIFT)
                    || keys.contains(KeyCode::KEY_Z)
            }) {
                println!(
                    "Found keyboard device: {} ({})",
                    device.name().unwrap_or("Unknown"),
                    path.display()
                );
                found.push((path, device));
            }
        }
        Ok(found)
    }
}

/// Whether a key press should fire the preview-visibility toggle: the
/// pressed `code` must match the bound key, and — if a modifier is bound —
/// that modifier must currently be held. An unbound key (`None`) never
/// fires; a binding with no modifier fires on the bare key regardless of
/// which modifiers happen to be down (matching the no-modifier cycle keys).
fn toggle_previews_should_fire(
    toggle_key: Option<u16>,
    toggle_modifier: Option<u16>,
    code: u16,
    pressed_modifiers: &HashSet<u16>,
) -> bool {
    toggle_key == Some(code) && toggle_modifier.is_none_or(|m| pressed_modifiers.contains(&m))
}

/// Pick the character (if any) whose hotkey matches a key press.
///
/// Two-pass match so a binding *with* a modifier wins when its modifier
/// is held, even if a different character has the same key bound
/// *without* a modifier. The naive single-pass `HashMap::iter().find()`
/// returns whichever entry the hash function happens to visit first —
/// pressing "Shift+F1" when both "Shift+F1 → Alpha" and "F1 → Beta" are
/// bound would non-deterministically resolve to Alpha or Beta.
///
/// `vk == 0` is treated as a placeholder (the panel writes a
/// modifier-only entry before any key is bound) and never matches.
fn resolve_character_hotkey(
    code: u16,
    pressed_modifiers: &HashSet<u16>,
    hotkeys: &HashMap<String, CharacterHotkey>,
) -> Option<String> {
    // First pass: modifier-bearing bindings whose modifier is held.
    let with_modifier = hotkeys.iter().find(|(_, hk)| {
        hk.vk != 0
            && hk.vk == code
            && match hk.modifier {
                Some(m) => pressed_modifiers.contains(&m),
                None => false,
            }
    });
    if let Some((name, _)) = with_modifier {
        return Some(name.clone());
    }
    // Second pass: no-modifier bindings.
    hotkeys
        .iter()
        .find(|(_, hk)| hk.vk != 0 && hk.vk == code && hk.modifier.is_none())
        .map(|(name, _)| name.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hk(vk: u16, modifier: Option<u16>) -> CharacterHotkey {
        CharacterHotkey { vk, modifier }
    }

    #[test]
    fn unmatched_code_returns_none() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, None)); // F1
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(resolve_character_hotkey(0x71, &pressed, &hotkeys), None);
    }

    #[test]
    fn placeholder_vk_zero_never_matches() {
        // Panel writes vk=0 when the user picks a modifier before a
        // key. Pressing any key (including code 0, theoretically)
        // shouldn't dispatch a placeholder.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0, Some(42)));
        let pressed: HashSet<u16> = [42].into_iter().collect();
        assert_eq!(resolve_character_hotkey(0, &pressed, &hotkeys), None);
    }

    #[test]
    fn no_modifier_binding_matches_when_no_modifier_held() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, None));
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("Alpha".to_string())
        );
    }

    #[test]
    fn modifier_binding_requires_modifier_held() {
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        // Press F1 without Shift held.
        assert_eq!(
            resolve_character_hotkey(0x70, &HashSet::new(), &hotkeys),
            None
        );
        // Press F1 with Shift held.
        let pressed: HashSet<u16> = [0x10].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("Alpha".to_string())
        );
    }

    #[test]
    fn modifier_binding_beats_no_modifier_binding_when_modifier_held() {
        // Regression guard for Bug C: HashMap iteration order would
        // otherwise pick either binding non-deterministically. This
        // pins down the contract: modifier-bearing match wins.
        //
        // We run the resolution many times so that even if a future
        // change re-introduces HashMap-order dependence, the test
        // catches it in expectation rather than relying on a single
        // hash seed.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("AlphaWithShift".to_string(), hk(0x70, Some(0x10))); // Shift+F1
        hotkeys.insert("BetaPlain".to_string(), hk(0x70, None)); // F1
        let pressed: HashSet<u16> = [0x10].into_iter().collect();
        for _ in 0..32 {
            assert_eq!(
                resolve_character_hotkey(0x70, &pressed, &hotkeys),
                Some("AlphaWithShift".to_string())
            );
        }
    }

    #[test]
    fn falls_back_to_no_modifier_binding_when_modifier_not_held() {
        // Same setup as above, but no modifier is held — should
        // resolve to the no-modifier binding.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("AlphaWithShift".to_string(), hk(0x70, Some(0x10)));
        hotkeys.insert("BetaPlain".to_string(), hk(0x70, None));
        let pressed: HashSet<u16> = HashSet::new();
        assert_eq!(
            resolve_character_hotkey(0x70, &pressed, &hotkeys),
            Some("BetaPlain".to_string())
        );
    }

    #[test]
    fn multiple_distinct_modifiers_pick_the_matching_one() {
        // Hotkeys: Shift+F1 → Alpha, Ctrl+F1 → Beta.
        // Pressing F1 with Ctrl held should pick Beta. Pressing F1
        // with Shift held should pick Alpha.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        hotkeys.insert("Beta".to_string(), hk(0x70, Some(0x11)));

        let shift_only: HashSet<u16> = [0x10].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &shift_only, &hotkeys),
            Some("Alpha".to_string())
        );

        let ctrl_only: HashSet<u16> = [0x11].into_iter().collect();
        assert_eq!(
            resolve_character_hotkey(0x70, &ctrl_only, &hotkeys),
            Some("Beta".to_string())
        );
    }

    #[test]
    fn modifier_binding_doesnt_match_when_no_modifier_held() {
        // Hotkey "Shift+F1 → Alpha" should NOT fire on plain F1, even
        // if no other binding exists for F1. The first-pass filter
        // requires the modifier to be in pressed_modifiers.
        let mut hotkeys = HashMap::new();
        hotkeys.insert("Alpha".to_string(), hk(0x70, Some(0x10)));
        assert_eq!(
            resolve_character_hotkey(0x70, &HashSet::new(), &hotkeys),
            None
        );
    }

    fn idle_config() -> KeyboardConfig {
        KeyboardConfig {
            enable: false,
            forward_key: 15,
            backward_key: 15,
            modifier_key: None,
            keyboard_device_path: None,
            minimize_inactive: false,
            character_hotkeys: HashMap::new(),
            toggle_previews_key: None,
            toggle_previews_modifier: None,
        }
    }

    #[test]
    fn has_work_false_when_nothing_bound() {
        assert!(!idle_config().has_work());
    }

    #[test]
    fn has_work_true_when_only_toggle_key_bound() {
        // Regression guard: binding just the preview-toggle key (cycling
        // off, no character hotkeys) must keep the listener attached, or
        // the toggle press would never be seen.
        let mut c = idle_config();
        c.toggle_previews_key = Some(88);
        assert!(c.has_work());
    }

    #[test]
    fn has_work_true_when_cycling_enabled_or_character_hotkey_bound() {
        let mut enabled = idle_config();
        enabled.enable = true;
        assert!(enabled.has_work());

        let mut with_char = idle_config();
        with_char
            .character_hotkeys
            .insert("Alpha".to_string(), hk(0x70, None));
        assert!(with_char.has_work());
    }

    #[test]
    fn toggle_fires_on_bare_key_when_no_modifier_bound() {
        let pressed = HashSet::new();
        assert!(toggle_previews_should_fire(Some(88), None, 88, &pressed));
        // A no-modifier binding fires even if some modifier happens to be
        // held — same as the no-modifier cycle keys.
        let shift_down: HashSet<u16> = [42].into_iter().collect();
        assert!(toggle_previews_should_fire(Some(88), None, 88, &shift_down));
    }

    #[test]
    fn toggle_requires_bound_modifier_to_be_held() {
        // Bound as Shift(42)+88.
        let none_held = HashSet::new();
        assert!(!toggle_previews_should_fire(
            Some(88),
            Some(42),
            88,
            &none_held
        ));
        let shift_held: HashSet<u16> = [42].into_iter().collect();
        assert!(toggle_previews_should_fire(
            Some(88),
            Some(42),
            88,
            &shift_held
        ));
    }

    #[test]
    fn toggle_never_fires_on_wrong_key_or_when_unbound() {
        let shift_held: HashSet<u16> = [42].into_iter().collect();
        assert!(!toggle_previews_should_fire(
            Some(88),
            Some(42),
            99,
            &shift_held
        ));
        assert!(!toggle_previews_should_fire(
            None,
            None,
            88,
            &HashSet::new()
        ));
    }

    #[test]
    fn event_numbers_sort_numerically() {
        let mut names = vec![
            "event11", "event7", "event12", "event0", "mouse2", "event100",
        ];
        names.retain(|n| event_number(n).is_some());
        names.sort_by_key(|n| event_number(n));
        assert_eq!(
            names,
            ["event0", "event7", "event11", "event12", "event100"]
        );
        assert_eq!(event_number("eventX"), None);
    }

    #[test]
    fn echo_from_another_device_is_dropped_once() {
        let t0 = Instant::now();
        let prev = KeyStamp {
            code: 27,
            value: 1,
            device: 0,
            at: t0,
        };
        // Same press from another node right away: echo.
        assert!(is_echo(
            Some(&prev),
            27,
            1,
            1,
            t0 + Duration::from_millis(2)
        ));
        // Same device (e.g. auto-repeat) is never an echo.
        assert!(!is_echo(
            Some(&prev),
            27,
            1,
            0,
            t0 + Duration::from_millis(2)
        ));
        // Different key, or release vs press: not an echo.
        assert!(!is_echo(
            Some(&prev),
            26,
            1,
            1,
            t0 + Duration::from_millis(2)
        ));
        assert!(!is_echo(
            Some(&prev),
            27,
            0,
            1,
            t0 + Duration::from_millis(2)
        ));
        // Outside the window: a genuine second press.
        assert!(!is_echo(
            Some(&prev),
            27,
            1,
            1,
            t0 + Duration::from_millis(40)
        ));
        assert!(!is_echo(None, 27, 1, 1, t0));
    }
}
