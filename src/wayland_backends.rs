use crate::config::Config;
use crate::pointer_nudge::{schedule_nudge, PointerNudger};
use crate::window_manager::{EveWindow, WindowManager};
use anyhow::{Context, Result};
use serde_json::Value;
use std::process::Command;
use std::sync::{Arc, OnceLock};
use x11rb::connection::Connection as _;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask, InputFocus,
    CLIENT_MESSAGE_EVENT,
};
use x11rb::rust_connection::RustConnection;

// ============================================================================
// Shared XWayland / EWMH helpers (KWin + GNOME)
// ============================================================================
//
// Both the KWin and GNOME backends run EVE as XWayland clients and drive
// focus through an EWMH `_NET_ACTIVE_WINDOW` ClientMessage rather than a
// Wayland-native protocol. The activate primitive (a pager-sourced
// `_NET_ACTIVE_WINDOW` message plus an explicit input-focus) and the
// active-window read are identical across the two compositors, so they live
// here instead of being duplicated per backend.

/// Send the EWMH `_NET_ACTIVE_WINDOW` activate ClientMessage for `window_id`
/// (source = 2, pager, which dodges the focus-stealing penalty applied to
/// source = 1) plus an explicit `set_input_focus`. Together these move both
/// the raise and keyboard focus to the target XWayland window.
///
/// We used to also mint an xdg-activation token and stamp it on
/// `_NET_STARTUP_ID` to bridge X11 focus to Wayland surface focus. On KWin
/// that request was always *denied* (focus-stealing prevention rejects our
/// serial-less requests — our input arrives via evdev, not the Wayland
/// seat), so it did nothing while its per-cycle round-trip slowly backed the
/// compositor up and made cycling degrade over a session. Removed: the pager
/// message + `set_input_focus` already deliver focus, and pointer focus is
/// handled separately by the pointer nudge.
fn ewmh_activate(
    conn: &RustConnection,
    screen_num: usize,
    net_active_window_atom: u32,
    window_id: u32,
) -> Result<()> {
    let screen = &conn.setup().roots[screen_num];
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: window_id,
        type_: net_active_window_atom,
        data: ClientMessageData::from([2, x11rb::CURRENT_TIME, 0, 0, 0]),
    };
    conn.send_event(
        false,
        screen.root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )?;
    let _ = conn.set_input_focus(InputFocus::PARENT, window_id, x11rb::CURRENT_TIME);
    conn.flush()?;
    Ok(())
}

/// Read `_NET_ACTIVE_WINDOW` off the root and return the focused window id
/// (0 if unset / no X11 window focused).
fn ewmh_active_window(
    conn: &RustConnection,
    screen_num: usize,
    net_active_window_atom: u32,
) -> Result<u32> {
    let root = conn.setup().roots[screen_num].root;
    let reply = conn
        .get_property(false, root, net_active_window_atom, AtomEnum::WINDOW, 0, 1)
        .context("get_property _NET_ACTIVE_WINDOW")?
        .reply()
        .context("get_property reply")?;
    Ok(reply.value32().and_then(|mut v| v.next()).unwrap_or(0))
}

/// Iconify (minimize) an XWayland window via an ICCCM `WM_CHANGE_STATE` →
/// IconicState(3) ClientMessage. Sent on the caller's *own* X connection so
/// the WM processes it in order relative to an `ewmh_activate` issued on the
/// same connection: minimizing the previously-focused client can't race
/// ahead of the new client's activation and make the compositor fall back to
/// an arbitrary focus. (The previous KWin path shelled out to `xdotool`,
/// i.e. a *separate* X client, so the WM could iconify the still-focused old
/// window before our activation landed.)
fn ewmh_iconify(
    conn: &RustConnection,
    screen_num: usize,
    wm_change_state_atom: u32,
    window_id: u32,
) -> Result<()> {
    let root = conn.setup().roots[screen_num].root;
    let event = ClientMessageEvent {
        response_type: CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: window_id,
        type_: wm_change_state_atom,
        data: ClientMessageData::from([3, 0, 0, 0, 0]),
    };
    conn.send_event(
        false,
        root,
        EventMask::SUBSTRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_REDIRECT,
        event,
    )
    .context("send WM_CHANGE_STATE")?;
    conn.flush()?;
    Ok(())
}

// ============================================================================
// KDE Plasma / KWin Backend
// ============================================================================
//
// Window enumeration + window-state queries go through `wmctrl` (it's the
// shortest path to a stable list of EWMH-aware X11 windows under XWayland
// on KDE). Window *activation* uses an EWMH _NET_ACTIVE_WINDOW client
// message sent directly via x11rb — KWin reliably honors that for
// XWayland clients (EVE in Wine/Proton is one), whereas `wmctrl -i -a`
// silently succeeds without actually foregrounding the target on KDE
// Plasma Wayland. Same activation primitive `X11Manager` uses, which is
// why click-to-activate from the Linux preview manager has always
// worked while side-button cycling did not.

pub struct KWinManager {
    /// Auxiliary X11 connection used only for the EWMH activation
    /// ClientMessage path. wmctrl/xdotool/swaymsg-style shelling stays
    /// elsewhere for compatibility; this is one extra socket but it
    /// removes a class of "activate succeeded but nothing happened"
    /// bugs that only show up under KDE Plasma Wayland.
    conn: Arc<RustConnection>,
    screen_num: usize,
    net_active_window_atom: u32,
    /// `WM_CHANGE_STATE` — ICCCM iconify carrier. `minimize_window` sends
    /// IconicState through this on `conn` (not via a `xdotool` subprocess)
    /// so the iconify is ordered after activate on the same connection.
    wm_change_state_atom: u32,
    /// Lazily-created virtual pointer for the Wayland pointer-focus nudge
    /// (see `pointer_nudge`). Built on first activation so one-shot CLI
    /// commands that never activate a window don't spawn a uinput device.
    /// A cached `None` means uinput wasn't available.
    pointer_nudger: OnceLock<Option<PointerNudger>>,
}

impl KWinManager {
    pub fn new() -> Result<Self> {
        Command::new("wmctrl")
            .arg("-m")
            .output()
            .context("wmctrl not found. Install wmctrl package")?;

        let (conn, screen_num) =
            x11rb::connect(None).context("X11 connect failed (XWayland not running?)")?;
        let conn = Arc::new(conn);
        let net_active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let wm_change_state_atom = conn.intern_atom(false, b"WM_CHANGE_STATE")?.reply()?.atom;

        Ok(Self {
            conn,
            screen_num,
            net_active_window_atom,
            wm_change_state_atom,
            pointer_nudger: OnceLock::new(),
        })
    }

    /// Run `wmctrl -lp` and return `(window_id_hex, pid, title)` triples.
    /// `-lp` adds a pid column we need to process-filter EVE clients
    /// (see eve_match::pid_is_eve_client). Columns: `<id> <desktop>
    /// <pid> <host> <title>`.
    fn get_all_windows(&self) -> Result<Vec<(String, u32, String)>> {
        let output = Command::new("wmctrl")
            .args(["-l", "-p"])
            .output()
            .context("Failed to execute wmctrl")?;

        if !output.status.success() {
            anyhow::bail!("wmctrl failed: {}", String::from_utf8_lossy(&output.stderr));
        }

        let mut windows = Vec::new();
        let lines = String::from_utf8_lossy(&output.stdout);

        for line in lines.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            // wmctrl -lp columns: <id> <desktop> <pid> <host> <title…>.
            // In rare cases (containers / sandboxed sessions where
            // HOSTNAME is empty) the host token collapses out under
            // split_whitespace and we see only 4 columns with the
            // title starting at parts[3]. Accept both layouts.
            if parts.len() < 4 {
                continue;
            }
            let window_id = parts[0];
            let pid: u32 = parts[2].parse().unwrap_or(0);
            let title_start = if parts.len() >= 5 { 4 } else { 3 };
            let title = parts[title_start..].join(" ");
            windows.push((window_id.to_string(), pid, title));
        }

        Ok(windows)
    }

    // `get_window_title_by_id` was used by the old kdotool fallback in
    // activate_window. The EWMH ClientMessage path doesn't need it, but
    // keeping it dead-allowed in case a future restore-from-iconic
    // codepath needs to look up titles for X11 calls that take names
    // rather than ids.
    #[allow(dead_code)]
    fn get_window_title_by_id(&self, hex_id: &str) -> Option<String> {
        let output = Command::new("wmctrl").args(["-l", "-p"]).output().ok()?;
        if !output.status.success() {
            return None;
        }

        let lines = String::from_utf8_lossy(&output.stdout);
        for line in lines.lines() {
            if line.starts_with(hex_id) {
                let parts: Vec<&str> = line.split_whitespace().collect();
                // `wmctrl -lp` columns: id desktop pid host title…
                // Empty-host fallback to parts[3..]; see get_all_windows.
                if parts.len() < 4 {
                    return None;
                }
                let title_start = if parts.len() >= 5 { 4 } else { 3 };
                return Some(parts[title_start..].join(" "));
            }
        }
        None
    }
}

impl WindowManager for KWinManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for (id_str, pid, title) in windows {
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process filter: reject anything titled "EVE - …" whose
            // owning process isn't the actual game (`exefile.exe`).
            // Browser tabs / Discord / etc. all get filtered here.
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            // Parse hex window ID (e.g., "0x06e00008") to u32.
            let id = if let Some(hex) = id_str.strip_prefix("0x") {
                u32::from_str_radix(hex, 16).unwrap_or(0)
            } else {
                id_str.parse::<u32>().unwrap_or(0)
            };
            if id == 0 {
                continue;
            }
            eve_windows.push(EveWindow {
                id,
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        // Shared XWayland/EWMH activation. `wmctrl -i -a` silently no-ops
        // under KDE Wayland for XWayland clients, so we send the
        // `_NET_ACTIVE_WINDOW` ClientMessage ourselves (with the
        // xdg-activation token bridge); see `ewmh_activate`.
        ewmh_activate(
            &self.conn,
            self.screen_num,
            self.net_active_window_atom,
            window_id,
        )?;
        // Pointer-focus nudge: KWin won't re-target clicks to the newly
        // raised client until the pointer moves. See `pointer_nudge`.
        schedule_nudge(&self.pointer_nudger);
        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width;
        let height = config.display_height - config.panel_height;

        for window in windows {
            // Convert u32 to hex format for wmctrl
            let hex_id = format!("0x{:08x}", window.id);

            // Move and resize window using wmctrl
            Command::new("wmctrl")
                .arg("-i")
                .arg("-r")
                .arg(&hex_id)
                .arg("-e")
                .arg(format!("0,{},{},{},{}", x, y, width, height))
                .output()?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        // Read _NET_ACTIVE_WINDOW directly via x11rb under XWayland —
        // matches what X11Manager does and avoids an `xdotool` runtime dep.
        ewmh_active_window(&self.conn, self.screen_num, self.net_active_window_atom)
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        // Iconify on our own X connection (see `ewmh_iconify`) so it's
        // ordered after the activation of the newly-focused client, instead
        // of racing it via a separate `xdotool` connection.
        ewmh_iconify(
            &self.conn,
            self.screen_num,
            self.wm_change_state_atom,
            window_id,
        )
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        let hex_id = format!("0x{:08x}", window_id);
        // wmctrl -i -a activates and restores from minimized state
        Command::new("wmctrl")
            .args(["-i", "-a", &hex_id])
            .output()
            .context("Failed to restore window")?;
        Ok(())
    }
}

// ============================================================================
// Sway Backend (via swaymsg)
// ============================================================================

pub struct SwayManager;

impl SwayManager {
    pub fn new() -> Result<Self> {
        // Verify swaymsg is available
        Command::new("swaymsg")
            .arg("--version")
            .output()
            .context("swaymsg not found. Make sure you're running Sway")?;

        Ok(Self)
    }

    fn get_all_windows(&self) -> Result<Vec<Value>> {
        let output = Command::new("swaymsg")
            .arg("-t")
            .arg("get_tree")
            .output()
            .context("Failed to execute swaymsg")?;

        if !output.status.success() {
            anyhow::bail!(
                "swaymsg failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let tree: Value =
            serde_json::from_slice(&output.stdout).context("Failed to parse swaymsg output")?;

        let mut windows = Vec::new();
        Self::extract_windows(&tree, &mut windows);

        Ok(windows)
    }

    fn extract_windows(node: &Value, windows: &mut Vec<Value>) {
        if let Some(node_type) = node.get("type").and_then(|t| t.as_str()) {
            if node_type == "con" || node_type == "floating_con" {
                if let Some(app_id) = node.get("app_id") {
                    if !app_id.is_null() {
                        windows.push(node.clone());
                    }
                } else if let Some(window_properties) = node.get("window_properties") {
                    if !window_properties.is_null() {
                        windows.push(node.clone());
                    }
                }
            }
        }

        if let Some(nodes) = node.get("nodes").and_then(|n| n.as_array()) {
            for child in nodes {
                Self::extract_windows(child, windows);
            }
        }

        if let Some(floating_nodes) = node.get("floating_nodes").and_then(|n| n.as_array()) {
            for child in floating_nodes {
                Self::extract_windows(child, windows);
            }
        }
    }

    fn get_window_title(window: &Value) -> Option<String> {
        window
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string())
    }

    fn get_window_id(window: &Value) -> Option<u32> {
        window.get("id").and_then(|i| i.as_u64()).map(|i| i as u32)
    }

    /// Sway's IPC tree exposes the X11/XWayland or native pid on each
    /// container node. Used to filter EVE windows by process identity
    /// rather than title alone — see eve_match::pid_is_eve_client.
    fn get_window_pid(window: &Value) -> Option<u32> {
        window.get("pid").and_then(|p| p.as_u64()).map(|p| p as u32)
    }
}

impl WindowManager for SwayManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for window in windows {
            let Some(title) = Self::get_window_title(&window) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate before title-only acceptance — see KWin
            // backend above for the rationale.
            let Some(pid) = Self::get_window_pid(&window) else {
                continue;
            };
            if !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            let Some(id) = Self::get_window_id(&window) else {
                continue;
            };
            eve_windows.push(EveWindow {
                id,
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        let output = Command::new("swaymsg")
            .arg(format!("[con_id={}] focus", window_id))
            .output()
            .context("Failed to activate window")?;

        if !output.status.success() {
            anyhow::bail!(
                "Failed to activate window: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }

        Ok(())
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width as i32;
        let height = (config.display_height - config.panel_height) as i32;

        for window in windows {
            // Sway uses floating mode for positioning
            Command::new("swaymsg")
                .arg(format!("[con_id={}] floating enable", window.id))
                .output()?;

            Command::new("swaymsg")
                .arg(format!("[con_id={}] move position {} {}", window.id, x, y))
                .output()?;

            Command::new("swaymsg")
                .arg(format!(
                    "[con_id={}] resize set {} {}",
                    window.id, width, height
                ))
                .output()?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        let windows = self.get_all_windows()?;

        for window in windows {
            if let Some(focused) = window.get("focused").and_then(|f| f.as_bool()) {
                if focused {
                    if let Some(id) = Self::get_window_id(&window) {
                        return Ok(id);
                    }
                }
            }
        }

        anyhow::bail!("No active window found")
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        Command::new("swaymsg")
            .arg(format!("[con_id={}] move scratchpad", window_id))
            .output()
            .context("Failed to minimize window")?;
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        // Show from scratchpad restores it
        Command::new("swaymsg")
            .arg(format!("[con_id={}] scratchpad show", window_id))
            .output()
            .context("Failed to restore window")?;
        Ok(())
    }
}

// ============================================================================
// Hyprland Backend (via hyprctl)
// ============================================================================
//
// Hyprland identifies windows by their 64-bit address (e.g.
// `0x55eb4b6c12e0`, a heap pointer inside the compositor), but the
// cross-platform `EveWindow.id` is a `u32`. We use the address's low 32
// bits as the id (the same convention Hyprland's own toplevel-export
// protocol uses for window handles) and keep a low32 → full-address map so
// dispatches can name the real window. Parsing the full address straight
// into a `u32` overflowed for every real window, collapsing all ids to 0
// and making every dispatch target `address:0x0`.
//
// Hyprland 0.57 moved to a Lua config: `hyprctl dispatch <args>` now
// evaluates `hl.dispatch(<args>)`, so the classic `dispatch focuswindow
// address:0x…` form is a Lua syntax error. The syntax is detected once at
// startup (`hyprctl eval` only exists on Lua builds) and every dispatch is
// built in both forms. Lua dispatches always name their target window
// explicitly: some (e.g. `hl.dsp.window.float`) act on the focused window
// when not told otherwise. hyprctl reports errors on stdout, so that's what
// failures surface.

/// Parse a Hyprland address string (`0x…` hex) into its full 64-bit value.
fn parse_hypr_address(address: &str) -> Option<u64> {
    let hex = address.strip_prefix("0x").unwrap_or(address);
    u64::from_str_radix(hex, 16).ok()
}

/// Low 32 bits of a Hyprland address — the `u32` window id we hand out.
fn hypr_window_id(address: u64) -> u32 {
    address as u32
}

/// How `hyprctl dispatch` takes its arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DispatchSyntax {
    /// `hyprctl dispatch focuswindow address:0x…` (Hyprland < 0.57).
    Legacy,
    /// `hyprctl dispatch 'hl.dsp.focus({ window = "address:0x…" })'`.
    Lua,
}

/// One dispatcher call, in both syntaxes. `sel` is an `address:0x…`
/// selector (hex digits only, so safe inside a Lua string literal).
#[derive(Debug, Clone, PartialEq, Eq)]
struct HyprDispatch {
    legacy: Vec<String>,
    lua: String,
}

impl HyprDispatch {
    fn focus(sel: &str) -> Self {
        Self {
            legacy: vec!["focuswindow".into(), sel.into()],
            lua: format!(r#"hl.dsp.focus({{ window = "{sel}" }})"#),
        }
    }

    /// Make the window floating (idempotent, unlike `togglefloating`).
    fn float(sel: &str) -> Self {
        Self {
            legacy: vec!["setfloating".into(), sel.into()],
            lua: format!(r#"hl.dsp.window.float({{ action = "enable", window = "{sel}" }})"#),
        }
    }

    fn move_to(sel: &str, x: i32, y: i32) -> Self {
        Self {
            legacy: vec!["movewindowpixel".into(), format!("exact {x} {y},{sel}")],
            lua: format!(r#"hl.dsp.window.move({{ x = {x}, y = {y}, window = "{sel}" }})"#),
        }
    }

    fn resize_to(sel: &str, w: i32, h: i32) -> Self {
        Self {
            legacy: vec!["resizewindowpixel".into(), format!("exact {w} {h},{sel}")],
            lua: format!(r#"hl.dsp.window.resize({{ x = {w}, y = {h}, window = "{sel}" }})"#),
        }
    }

    /// Hide on the special workspace without switching to it.
    fn to_special(sel: &str) -> Self {
        Self {
            legacy: vec!["movetoworkspacesilent".into(), format!("special,{sel}")],
            lua: format!(r#"hl.dsp.window.move({{ workspace = "special", window = "{sel}" }})"#),
        }
    }

    /// Bring back to the current workspace.
    fn to_current_workspace(sel: &str) -> Self {
        Self {
            legacy: vec!["movetoworkspace".into(), format!("e+0,{sel}")],
            lua: format!(r#"hl.dsp.window.move({{ workspace = "e+0", window = "{sel}" }})"#),
        }
    }

    fn args(&self, syntax: DispatchSyntax) -> Vec<String> {
        let mut args = vec!["dispatch".to_string()];
        match syntax {
            DispatchSyntax::Legacy => args.extend(self.legacy.iter().cloned()),
            DispatchSyntax::Lua => args.push(self.lua.clone()),
        }
        args
    }
}

/// Did a `hyprctl` command fail? hyprctl prints errors (bad dispatch
/// syntax, unknown window, unreachable socket) on stdout, sometimes with
/// a zero exit status, so both are checked. Returns the message to report.
fn hyprctl_failure(success: bool, stdout: &str, stderr: &str) -> Option<String> {
    let out = stdout.trim();
    let bad_line = out.lines().any(|line| {
        let line = line.trim_start().to_ascii_lowercase();
        ["error", "warning", "invalid", "couldn't connect", "no such"]
            .iter()
            .any(|p| line.starts_with(p))
    });
    (!success || bad_line).then(|| {
        let msg = if out.is_empty() { stderr.trim() } else { out };
        if msg.is_empty() {
            "hyprctl failed with no output".to_string()
        } else {
            msg.to_string()
        }
    })
}

/// Run `hyprctl <args>`, returning stdout or the reported failure.
fn run_hyprctl(args: &[String]) -> Result<Vec<u8>> {
    let output = Command::new("hyprctl")
        .args(args)
        .output()
        .context("Failed to execute hyprctl")?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    // JSON replies can contain these words inside values; only text
    // replies are scanned for error lines.
    let is_json = args.iter().any(|a| a == "-j");
    let failure = if is_json {
        (!output.status.success()).then(|| {
            hyprctl_failure(false, &stdout, &String::from_utf8_lossy(&output.stderr))
                .unwrap_or_default()
        })
    } else {
        hyprctl_failure(
            output.status.success(),
            &stdout,
            &String::from_utf8_lossy(&output.stderr),
        )
    };
    match failure {
        Some(msg) => anyhow::bail!("hyprctl {}: {msg}", args.join(" ")),
        None => Ok(output.stdout),
    }
}

pub struct HyprlandManager {
    /// low32 id → full address, refreshed on every enumeration.
    addresses: std::sync::Mutex<std::collections::HashMap<u32, u64>>,
    syntax: DispatchSyntax,
}

impl HyprlandManager {
    pub fn new() -> Result<Self> {
        run_hyprctl(&["version".into()]).context(
            "hyprctl can't reach Hyprland (not running, or a stale \
             HYPRLAND_INSTANCE_SIGNATURE in this shell?)",
        )?;
        // `eval` exists only on Lua-config Hyprland (0.57+), where it
        // replies "ok"; older builds reject it as an unknown request.
        let syntax = match run_hyprctl(&["eval".into(), "return 1".into()]) {
            Ok(out) if String::from_utf8_lossy(&out).trim() == "ok" => DispatchSyntax::Lua,
            _ => DispatchSyntax::Legacy,
        };
        println!("Hyprland dispatch syntax: {syntax:?}");

        Ok(Self {
            addresses: std::sync::Mutex::new(std::collections::HashMap::new()),
            syntax,
        })
    }

    fn dispatch(&self, d: HyprDispatch) -> Result<()> {
        run_hyprctl(&d.args(self.syntax)).map(|_| ())
    }

    fn get_all_windows(&self) -> Result<Vec<Value>> {
        let out = run_hyprctl(&["clients".into(), "-j".into()])?;
        serde_json::from_slice(&out).context("Failed to parse hyprctl output")
    }

    /// Full address of the focused window, if any.
    fn active_address(&self) -> Option<u64> {
        let out = run_hyprctl(&["activewindow".into(), "-j".into()]).ok()?;
        let window: Value = serde_json::from_slice(&out).ok()?;
        window
            .get("address")
            .and_then(|a| a.as_str())
            .and_then(parse_hypr_address)
    }

    /// Record `address` under its low32 id and return that id.
    fn remember(&self, address: u64) -> u32 {
        let id = hypr_window_id(address);
        self.addresses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id, address);
        id
    }

    /// Resolve a `u32` window id back to the `address:0x…` selector
    /// hyprctl dispatches expect. Falls back to a fresh `hyprctl clients`
    /// scan when the id isn't cached (e.g. a one-shot CLI command).
    fn selector(&self, window_id: u32) -> Result<String> {
        let cached = self
            .addresses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&window_id)
            .copied();
        let address = match cached {
            Some(a) => a,
            None => self
                .get_all_windows()?
                .iter()
                .filter_map(|w| w.get("address").and_then(|a| a.as_str()))
                .filter_map(parse_hypr_address)
                .find(|a| hypr_window_id(*a) == window_id)
                .inspect(|&a| {
                    self.remember(a);
                })
                .ok_or_else(|| anyhow::anyhow!("no Hyprland window with id {window_id:#x}"))?,
        };
        Ok(format!("address:0x{address:x}"))
    }
}

impl WindowManager for HyprlandManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let windows = self.get_all_windows()?;
        let mut eve_windows = Vec::new();

        for window in windows {
            let Some(title) = window.get("title").and_then(|t| t.as_str()) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate. Hyprland's IPC reports each client's pid
            // via the "pid" field; verify it's the actual EVE client
            // (`exefile.exe`) before accepting.
            let pid = window.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            let Some(address) = window
                .get("address")
                .and_then(|a| a.as_str())
                .and_then(parse_hypr_address)
            else {
                continue;
            };
            eve_windows.push(EveWindow {
                id: self.remember(address),
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }

        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        self.dispatch(HyprDispatch::focus(&self.selector(window_id)?))
            .context("Failed to activate window")
    }

    fn stack_windows(&self, windows: &[EveWindow], config: &Config) -> Result<()> {
        let x = ((config.display_width - config.eve_width) / 2) as i32;
        let y = 0;
        let width = config.eve_width as i32;
        let height = (config.display_height - config.panel_height) as i32;

        for window in windows {
            let sel = self.selector(window.id)?;
            self.dispatch(HyprDispatch::float(&sel))?;
            // Resize before moving: the Lua resize keeps the window's
            // centre, which would shift an already-placed window.
            self.dispatch(HyprDispatch::resize_to(&sel, width, height))?;
            self.dispatch(HyprDispatch::move_to(&sel, x, y))?;
        }

        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        match self.active_address() {
            Some(address) => Ok(self.remember(address)),
            None => anyhow::bail!("Failed to get active window ID"),
        }
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        let sel = self.selector(window_id)?;
        // The Lua workspace move focuses the moved window (there's no
        // "silent" variant), which would undo the activation the cycle
        // just did; hand focus back to whatever had it.
        let previous = match self.syntax {
            DispatchSyntax::Lua => self.active_address(),
            DispatchSyntax::Legacy => None,
        };
        self.dispatch(HyprDispatch::to_special(&sel))
            .context("Failed to minimize window")?;
        if let Some(prev) = previous.filter(|a| hypr_window_id(*a) != window_id) {
            self.dispatch(HyprDispatch::focus(&format!("address:0x{prev:x}")))?;
        }
        Ok(())
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        self.dispatch(HyprDispatch::to_current_workspace(
            &self.selector(window_id)?,
        ))
        .context("Failed to restore window")
    }
}

// ============================================================================
// GNOME / Mutter Backend (XWayland + EWMH, no restack)
// ============================================================================
//
// GNOME Shell on Wayland exposes no client-facing window-management IPC and
// deliberately implements no wlroots control protocol, so — like every other
// backend — we drive EVE (which runs as XWayland X11 windows under Proton)
// through the X server directly. Enumeration reads `_NET_CLIENT_LIST` and
// filters by the EVE title + process identity; activation reuses the shared
// EWMH path (xdg-activation token + pager-sourced `_NET_ACTIVE_WINDOW`),
// exactly as KWin does.
//
// We deliberately go straight to x11rb rather than shelling out to
// wmctrl/xdotool the way the KWin backend does for some calls: under Mutter's
// XWayland several EWMH conveniences are only partly wired up (e.g. `wmctrl
// -m`'s supporting-WM check fails), so the direct property reads are the
// reliable path — the same approach other Linux-native EVE tools take.
//
// `stack_windows` is a no-op: Wayland forbids a client positioning another
// window and there is no extension-free way around it on Mutter. The config
// panel hides the Restack button on this session (see
// `window_manager::restack_supported`).

pub struct GnomeManager {
    conn: Arc<RustConnection>,
    screen_num: usize,
    net_active_window_atom: u32,
    net_client_list_atom: u32,
    net_wm_pid_atom: u32,
    net_wm_name_atom: u32,
    utf8_string_atom: u32,
    wm_change_state_atom: u32,
    /// Lazily-created virtual pointer for the Wayland pointer-focus nudge
    /// (see `pointer_nudge`). Same rationale as the KWin backend — Mutter
    /// also only re-targets clicks on pointer motion.
    pointer_nudger: OnceLock<Option<PointerNudger>>,
}

impl GnomeManager {
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .context("X11 connect failed — is XWayland running under GNOME?")?;
        let conn = Arc::new(conn);

        let net_active_window_atom = conn
            .intern_atom(false, b"_NET_ACTIVE_WINDOW")?
            .reply()?
            .atom;
        let net_client_list_atom = conn.intern_atom(false, b"_NET_CLIENT_LIST")?.reply()?.atom;
        let net_wm_pid_atom = conn.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        let net_wm_name_atom = conn.intern_atom(false, b"_NET_WM_NAME")?.reply()?.atom;
        let utf8_string_atom = conn.intern_atom(false, b"UTF8_STRING")?.reply()?.atom;
        let wm_change_state_atom = conn.intern_atom(false, b"WM_CHANGE_STATE")?.reply()?.atom;

        Ok(Self {
            conn,
            screen_num,
            net_active_window_atom,
            net_client_list_atom,
            net_wm_pid_atom,
            net_wm_name_atom,
            utf8_string_atom,
            wm_change_state_atom,
            pointer_nudger: OnceLock::new(),
        })
    }

    /// Read a window's title, preferring `_NET_WM_NAME` (UTF-8) and falling
    /// back to the legacy `WM_NAME` (Latin-1). EVE sets the former.
    fn window_title(&self, window: u32) -> Option<String> {
        if let Some(reply) = self
            .conn
            .get_property(
                false,
                window,
                self.net_wm_name_atom,
                self.utf8_string_atom,
                0,
                u32::MAX,
            )
            .ok()
            .and_then(|c| c.reply().ok())
        {
            if !reply.value.is_empty() {
                if let Ok(s) = String::from_utf8(reply.value) {
                    return Some(s);
                }
            }
        }
        if let Some(reply) = self
            .conn
            .get_property(
                false,
                window,
                AtomEnum::WM_NAME,
                AtomEnum::STRING,
                0,
                u32::MAX,
            )
            .ok()
            .and_then(|c| c.reply().ok())
        {
            if !reply.value.is_empty() {
                return Some(String::from_utf8_lossy(&reply.value).into_owned());
            }
        }
        None
    }

    /// Read a window's owning PID from `_NET_WM_PID` (0 if unset).
    fn window_pid(&self, window: u32) -> u32 {
        self.conn
            .get_property(
                false,
                window,
                self.net_wm_pid_atom,
                AtomEnum::CARDINAL,
                0,
                1,
            )
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|r| r.value32().and_then(|mut v| v.next()))
            .unwrap_or(0)
    }
}

impl WindowManager for GnomeManager {
    fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
        let root = self.conn.setup().roots[self.screen_num].root;
        let reply = self
            .conn
            .get_property(
                false,
                root,
                self.net_client_list_atom,
                AtomEnum::WINDOW,
                0,
                u32::MAX,
            )
            .context("get_property _NET_CLIENT_LIST")?
            .reply()
            .context("_NET_CLIENT_LIST reply")?;
        let ids: Vec<u32> = reply.value32().map(|it| it.collect()).unwrap_or_default();

        let mut eve_windows = Vec::new();
        for id in ids {
            let Some(title) = self.window_title(id) else {
                continue;
            };
            if !title.starts_with("EVE - ") {
                continue;
            }
            // Process gate: reject anything titled "EVE - …" whose owning
            // process isn't the actual game (`exefile.exe`) — browser tabs,
            // Discord channels, etc. Matches the other backends.
            let pid = self.window_pid(id);
            if pid == 0 || !crate::eve_match::pid_is_eve_client(pid) {
                continue;
            }
            eve_windows.push(EveWindow {
                id,
                title: title.trim_start_matches("EVE - ").to_string(),
            });
        }
        Ok(eve_windows)
    }

    fn activate_window(&self, window_id: u32) -> Result<()> {
        ewmh_activate(
            &self.conn,
            self.screen_num,
            self.net_active_window_atom,
            window_id,
        )?;
        // Pointer-focus nudge — Mutter, like KWin, only re-targets clicks
        // on pointer motion. See `pointer_nudge`.
        schedule_nudge(&self.pointer_nudger);
        Ok(())
    }

    fn stack_windows(&self, _windows: &[EveWindow], _config: &Config) -> Result<()> {
        // No-op on GNOME Wayland: Mutter implements no client-facing window
        // positioning, and there is no extension-free way to move another
        // window to absolute coordinates. The panel hides the Restack button
        // here; the CLI `stack` falls through to this and intentionally does
        // nothing rather than failing loudly.
        eprintln!(
            "Restack is unavailable on GNOME Wayland — the compositor does not allow \
             applications to position windows. Skipping."
        );
        Ok(())
    }

    fn get_active_window(&self) -> Result<u32> {
        ewmh_active_window(&self.conn, self.screen_num, self.net_active_window_atom)
    }

    fn minimize_window(&self, window_id: u32) -> Result<()> {
        // ICCCM iconify (WM_CHANGE_STATE → IconicState). Mutter honors it for
        // XWayland windows. Shared with the KWin backend — see `ewmh_iconify`.
        ewmh_iconify(
            &self.conn,
            self.screen_num,
            self.wm_change_state_atom,
            window_id,
        )
    }

    fn restore_window(&self, window_id: u32) -> Result<()> {
        // Activating an iconified window both un-minimizes and focuses it.
        self.activate_window(window_id)
    }
}

#[cfg(test)]
mod hyprland_tests {
    use super::*;

    #[test]
    fn parses_full_64_bit_address() {
        // Real Hyprland addresses are heap pointers well above u32::MAX.
        assert_eq!(parse_hypr_address("0x55eb4b6c12e0"), Some(0x55eb_4b6c_12e0));
        assert_eq!(parse_hypr_address("55eb4b6c12e0"), Some(0x55eb_4b6c_12e0));
        assert_eq!(parse_hypr_address("0xnothex"), None);
    }

    #[test]
    fn dispatch_syntaxes() {
        let sel = "address:0x55eb4b6c12e0";
        let d = HyprDispatch::focus(sel);
        assert_eq!(
            d.args(DispatchSyntax::Legacy),
            ["dispatch", "focuswindow", sel]
        );
        assert_eq!(
            d.args(DispatchSyntax::Lua),
            [
                "dispatch",
                r#"hl.dsp.focus({ window = "address:0x55eb4b6c12e0" })"#
            ]
        );
        assert_eq!(
            HyprDispatch::to_special(sel).args(DispatchSyntax::Legacy),
            [
                "dispatch",
                "movetoworkspacesilent",
                &format!("special,{sel}")
            ]
        );
        // Every Lua dispatch names its window explicitly.
        for d in [
            HyprDispatch::float(sel),
            HyprDispatch::move_to(sel, 1, 2),
            HyprDispatch::resize_to(sel, 3, 4),
            HyprDispatch::to_special(sel),
            HyprDispatch::to_current_workspace(sel),
        ] {
            assert!(d.lua.starts_with("hl.dsp."), "{}", d.lua);
            assert!(
                d.lua.contains(r#"window = "address:0x55eb4b6c12e0""#),
                "{}",
                d.lua
            );
        }
        assert_eq!(
            HyprDispatch::move_to(sel, 6400, 0).lua,
            r#"hl.dsp.window.move({ x = 6400, y = 0, window = "address:0x55eb4b6c12e0" })"#
        );
    }

    #[test]
    fn hyprctl_errors_on_stdout_are_failures() {
        // Old-style dispatch on a Lua build: exit 7, error on stdout.
        let lua_err = "Error: 3 [string \"return hl.dispatch(focuswindow...\"]:1: ')' expected";
        assert!(hyprctl_failure(false, lua_err, "")
            .unwrap()
            .contains("expected"));
        // Some failures exit 0.
        assert!(hyprctl_failure(true, "warning: hl.focus: window not found", "").is_some());
        assert!(hyprctl_failure(true, "Invalid dispatcher", "").is_some());
        // Stale HYPRLAND_INSTANCE_SIGNATURE.
        assert!(hyprctl_failure(
            false,
            "Couldn't connect to /run/user/1000/hypr/x/.socket.sock. (4)",
            ""
        )
        .unwrap()
        .contains("Couldn't connect"));
        assert_eq!(hyprctl_failure(true, "ok\n", ""), None);
        assert_eq!(
            hyprctl_failure(false, "", "").as_deref(),
            Some("hyprctl failed with no output")
        );
    }

    #[test]
    fn window_id_is_low_32_bits() {
        assert_eq!(hypr_window_id(0x55eb_4b6c_12e0), 0x4b6c_12e0);
        assert_ne!(hypr_window_id(0x55eb_4b6c_12e0), 0);
    }
}
