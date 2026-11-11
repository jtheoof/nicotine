//! Wayland-native preview manager (Hyprland prototype).
//!
//! The XComposite path in `preview_x11` can only see XWayland windows. When
//! EVE runs as a native Wayland client (Proton with `PROTON_ENABLE_WAYLAND=1`)
//! it is invisible to X11, so this backend does the same job with Wayland
//! protocols instead:
//!
//! - **Surfaces:** one `zwlr_layer_shell_v1` surface per EVE client on the
//!   `overlay` layer, which Hyprland draws above fullscreen windows (the
//!   `top` layer is hidden under fullscreen). Positioned with top/left
//!   margins on the output that contains the saved position.
//! - **Live thumbnail:** `ext_image_copy_capture_v1` on a
//!   `ext_foreign_toplevel_image_capture_source_manager_v1` source built from
//!   the EVE window's `ext_foreign_toplevel_handle_v1`. Frames land in
//!   full-size buffers that are attached unchanged to a subsurface; a
//!   `wp_viewport` destination makes the compositor do the downscale on the
//!   GPU, so we never touch pixels on the CPU. Buffers are GPU dmabufs
//!   (allocated with GBM on the device the session advertises, imported via
//!   `zwp_linux_dmabuf_v1`) so the copy stays GPU-to-GPU; shm is the
//!   fallback when that isn't available (`NICOTINE_PREVIEW_DMABUF=0`
//!   forces it).
//! - **Active client:** `zwlr_foreign_toplevel_manager_v1` `activated` state,
//!   matched to characters by window title.
//! - **Opacity:** `wp_alpha_modifier_v1` when the compositor offers it.
//!
//! Chrome (title strip, border, character name) matches the X11 previews.
//! Click-to-activate goes through the daemon's `WindowManager`; drag with
//! snap-to-dock persists to the shared `preview_positions.toml`.
//!
//! List display mode is one more layer surface: a "Nicotine" title strip
//! and one row per running client in cycle order, the active one marked.
//! It can be dragged (position saved as `list_position`) and clicking a
//! row focuses that client.
//!
//! Chrome is rendered at scale 1.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};
use std::os::fd::AsFd;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState, Region},
    delegate_compositor, delegate_foreign_toplevel_list, delegate_layer, delegate_output,
    delegate_pointer, delegate_registry, delegate_relative_pointer, delegate_seat, delegate_shm,
    delegate_subcompositor,
    foreign_toplevel_list::{ForeignToplevelList, ForeignToplevelListHandler},
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        relative_pointer::{RelativeMotionEvent, RelativePointerHandler, RelativePointerState},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{raw::RawPool, slot::SlotPool, Shm, ShmHandler},
    subcompositor::SubcompositorState,
};
use wayland_client::{
    event_created_child,
    globals::{registry_queue_init, GlobalList},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_output::{self, WlOutput},
        wl_pointer::WlPointer,
        wl_seat::WlSeat,
        wl_shm,
        wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum,
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;
use wayland_protocols::ext::image_capture_source::v1::client::{
    ext_foreign_toplevel_image_capture_source_manager_v1::ExtForeignToplevelImageCaptureSourceManagerV1,
    ext_image_capture_source_v1::ExtImageCaptureSourceV1,
};
use wayland_protocols::ext::image_copy_capture::v1::client::{
    ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
    ext_image_copy_capture_manager_v1::{ExtImageCopyCaptureManagerV1, Options},
    ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
};
use wayland_protocols::wp::alpha_modifier::v1::client::{
    wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1, wp_alpha_modifier_v1::WpAlphaModifierV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{self, ZwpLinuxBufferParamsV1},
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols::wp::relative_pointer::zv1::client::zwp_relative_pointer_v1::ZwpRelativePointerV1;
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::config::{DisplayMode, LiveSettings, OutputChoice};
use crate::cycle_state::CycleState;
use crate::preview_common::{
    preview_should_hide, snap_position, DragRect, DragState, DRAG_THRESHOLD_PX, SNAP_THRESHOLD_PX,
};
use crate::preview_positions::PreviewPositions;
use crate::preview_x11::{
    list_window_height, thumbnail_size, BORDER_WIDTH, LIST_BORDER, LIST_ROW_HEIGHT,
    LIST_ROW_LEFT_PAD, LIST_ROW_RIGHT_PAD, LIST_ROW_TOP_PAD, LIST_TITLE_STRIP_HEIGHT, LIST_WIDTH,
    TITLE_STRIP_HEIGHT, TITLE_TEXT_LEFT_PAD,
};
use crate::text_render::{self, Antialias, TextFont, TextMask};
use crate::theme::{PanelPalette, PreviewTheme, Rgb};
use crate::window_manager::{
    detect_display_server, detect_wayland_compositor, DisplayServer, WaylandCompositor,
    WindowManager,
};

/// Poll timeout for the event loop. Capture starts and reconcile run on
/// these ticks; frame delivery itself is event-driven.
const TICK: Duration = Duration::from_millis(8);
/// How often the preview set is reconciled against EVE windows and
/// LiveSettings (sizes, opacity, show/hide).
const RECONCILE_INTERVAL: Duration = Duration::from_millis(100);
/// Default thumbnail refresh cap. Each capture is a full-resolution
/// GPU→shm readback plus re-upload, so this is the main cost knob of the
/// shm path. Override with `NICOTINE_PREVIEW_FPS`.
const DEFAULT_CAPTURE_FPS: u32 = 20;
/// Capture buffers per preview: one on screen, one being filled.
const CAPTURE_BUFFERS: usize = 2;
/// Consecutive failed captures into dmabufs (with none succeeding) before
/// giving up on dmabuf and falling back to shm for the session.
const DMABUF_MAX_FAILURES: u32 = 3;
/// DRM fourcc codes, preferred first. X-formats: the captured window's
/// alpha isn't meaningful and would make the thumbnail see-through.
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
/// `DRM_FORMAT_MOD_INVALID`: "implicit modifier" in modifier lists.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// "Nicotine" wordmark size in the list's title strip (pixels per em;
/// matches the X11 list's look).
const LIST_TITLE_EM_PX: f32 = 35.6;
/// Largest row font that fits a list row.
const LIST_ROW_MAX_EM_PX: f32 = 16.0;

/// `BTN_LEFT` from linux/input-event-codes.h.
const BTN_LEFT: u32 = 0x110;
/// `zwlr_foreign_toplevel_handle_v1.state` value for "activated".
const WLR_STATE_ACTIVATED: u32 = 2;

/// Use the Wayland-native previews when running under Hyprland on Wayland.
/// `NICOTINE_PREVIEW_BACKEND=x11` forces the XComposite path (e.g. if EVE
/// runs under XWayland and the X11 previews are preferred).
pub fn should_use() -> bool {
    if std::env::var("NICOTINE_PREVIEW_BACKEND").is_ok_and(|v| v.eq_ignore_ascii_case("x11")) {
        return false;
    }
    detect_display_server() == DisplayServer::Wayland
        && detect_wayland_compositor() == WaylandCompositor::Hyprland
}

/// Spawn the Wayland preview manager thread. Connects and binds every
/// required global before returning, so an `Err` here means this backend
/// can't run (missing protocol, no Wayland socket) and the caller can fall
/// back to the X11 manager.
/// Title-text settings from config.toml (read at startup).
#[derive(Debug, Clone)]
pub struct FontSettings {
    pub font: Option<String>,
    pub size_px: f32,
    pub antialias: Option<String>,
}

impl FontSettings {
    pub fn from_config(config: &crate::config::Config) -> Self {
        Self {
            font: config.preview_font.clone(),
            size_px: config.preview_font_size,
            antialias: config.preview_font_antialias.clone(),
        }
    }
}

pub fn spawn(
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
    font: FontSettings,
    theme: PreviewTheme,
    list_palette: PanelPalette,
) -> Result<()> {
    let (tx, rx) = mpsc::channel::<Result<()>>();
    std::thread::Builder::new()
        .name("preview-wayland".into())
        .spawn(move || {
            let (mut queue, mut manager) =
                match Manager::connect(wm, state, live, font, theme, list_palette) {
                    Ok(v) => {
                        let _ = tx.send(Ok(()));
                        v
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                };
            if let Err(e) = manager.run(&mut queue) {
                eprintln!("Wayland preview manager exited with error: {e:#}");
            }
        })
        .context("spawn preview-wayland thread")?;
    rx.recv()
        .unwrap_or_else(|_| Err(anyhow::anyhow!("preview-wayland thread died during init")))
}

trait MutexExt<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_recover(&self) -> std::sync::MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// ============================================================================
// Per-preview state
// ============================================================================

/// User data on capture `wl_buffer`s: which preview / slot it belongs to.
struct BufTag {
    preview: u64,
    idx: usize,
}

struct CaptureBuf {
    buffer: WlBuffer,
    /// Attached to the thumbnail surface and not yet released by the
    /// compositor. Must not be captured into until released.
    on_screen: bool,
    /// GPU memory behind a dmabuf buffer; `None` for shm.
    _bo: Option<gbm::BufferObject<()>>,
}

/// One batch of session buffer constraints (everything up to `done`).
#[derive(Debug, Default, Clone, PartialEq)]
struct Constraints {
    size: Option<(u32, u32)>,
    shm_formats: Vec<wl_shm::Format>,
    /// `dev_t` of the DRM device dmabufs must be allocated on.
    dmabuf_device: Option<u64>,
    /// (DRM fourcc, allowed modifiers)
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BufKind {
    Dmabuf,
    Shm,
}

/// One `ext_image_copy_capture` session on an EVE toplevel plus its
/// buffers.
struct Capture {
    source: ExtImageCaptureSourceV1,
    session: ExtImageCopyCaptureSessionV1,
    /// In-flight frame and the buffer slot it's writing into.
    frame: Option<(ExtImageCopyCaptureFrameV1, usize)>,
    /// Constraints being received; moved to `constraints` on `done`.
    pending: Constraints,
    constraints: Constraints,
    /// Size and kind of the allocated `bufs`; `None` until allocated.
    size: Option<(u32, u32)>,
    kind: Option<BufKind>,
    /// Held to keep the memfd pool alive for shm buffers.
    _pool: Option<RawPool>,
    bufs: Vec<CaptureBuf>,
    /// Failed dmabuf captures since the last success.
    dmabuf_failures: u32,
    last_start: Instant,
    stopped: bool,
}

impl Capture {
    fn free_buffers(&mut self) {
        for b in self.bufs.drain(..) {
            b.buffer.destroy();
        }
        self._pool = None;
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let Some((frame, _)) = self.frame.take() {
            frame.destroy();
        }
        self.session.destroy();
        self.source.destroy();
        self.free_buffers();
    }
}

struct Preview {
    id: u64,
    name: String,
    handle: ExtForeignToplevelHandleV1,
    layer: LayerSurface,
    /// Logical origin of the output the layer surface lives on.
    output_origin: (i32, i32),
    output_size: (i32, i32),
    /// Global logical position of the preview's top-left corner.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    configured: bool,
    thumb_surface: WlSurface,
    subsurface: WlSubsurface,
    thumb_viewport: WpViewport,
    alpha: Option<(WpAlphaModifierSurfaceV1, WpAlphaModifierSurfaceV1)>,
    /// Kept alive while the compositor shows it; sctk frees it on release.
    chrome_buf: Option<smithay_client_toolkit::shm::slot::Buffer>,
    title_mask: TextMask,
    is_active: bool,
    capture: Capture,
    drag: DragState,
    press_local: (f64, f64),
    /// Pointer displacement since the press, in global logical pixels.
    drag_delta: (f64, f64),
    /// `drag_delta` changed since the preview was last moved; applied
    /// once per loop tick so a 1000 Hz mouse doesn't flood commits.
    pending_move: bool,
}

impl Drop for Preview {
    fn drop(&mut self) {
        if let Some((a, b)) = self.alpha.take() {
            a.destroy();
            b.destroy();
        }
        self.thumb_viewport.destroy();
        self.subsurface.destroy();
        self.thumb_surface.destroy();
        // `capture` drops after this body (frame/session/buffers), then
        // `layer` destroys the layer surface and its wl_surface.
    }
}

impl Preview {
    fn rect(&self) -> DragRect {
        DragRect {
            left: self.x,
            top: self.y,
            right: self.x + self.width as i32,
            bottom: self.y + self.height as i32,
        }
    }

    fn apply_margins(&self) {
        self.layer.set_margin(
            self.y - self.output_origin.1,
            0,
            0,
            self.x - self.output_origin.0,
        );
    }

    fn thumb_size(&self) -> (u16, u16) {
        thumbnail_size(self.width as u16, self.height as u16)
    }
}

/// The List display mode window.
struct ListWindow {
    layer: LayerSurface,
    output_origin: (i32, i32),
    output_size: (i32, i32),
    /// Global logical position of the top-left corner.
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    configured: bool,
    /// Kept alive while the compositor shows it.
    buf: Option<smithay_client_toolkit::shm::slot::Buffer>,
    /// Character names, one row each, in cycle order.
    names: Vec<String>,
    row_masks: Vec<TextMask>,
    drag: DragState,
    press_local: (f64, f64),
    drag_delta: (f64, f64),
    pending_move: bool,
}

impl ListWindow {
    fn apply_margins(&self) {
        self.layer.set_margin(
            self.y - self.output_origin.1,
            0,
            0,
            self.x - self.output_origin.0,
        );
    }

    fn output(&self) -> OutRect {
        OutRect {
            x: self.output_origin.0,
            y: self.output_origin.1,
            w: self.output_size.0,
            h: self.output_size.1,
        }
    }
}

// ============================================================================
// Manager
// ============================================================================

/// An output plus its logical origin and size.
type OutputGeometry = (WlOutput, (i32, i32), (i32, i32));

/// Logical rectangle of an output in the compositor's global space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct OutRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl OutRect {
    fn contains(&self, px: i32, py: i32) -> bool {
        px >= self.x && py >= self.y && px < self.x + self.w && py < self.y + self.h
    }

    /// Clamp a `w`×`h` box at (x, y) so it stays inside this output.
    fn clamp_box(&self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
        (
            x.clamp(self.x, (self.x + self.w - w).max(self.x)),
            y.clamp(self.y, (self.y + self.h - h).max(self.y)),
        )
    }
}

/// Index of the output containing (px, py), if any. Global layouts need
/// not include the origin (e.g. monitors starting at x = 4960), so callers
/// must not assume (0, 0) lands anywhere.
fn output_index_at(outputs: &[OutRect], px: i32, py: i32) -> Option<usize> {
    outputs.iter().position(|o| o.contains(px, py))
}

/// Default spot for a preview with no saved position: cascaded from the
/// top-left of `base` (the EVE client's own output when known).
fn default_position(base: OutRect, offset: i32) -> (i32, i32) {
    (base.x + 64 + offset, base.y + 64 + offset)
}

/// Where the cursor ends up after moving from `prev` by a relative delta
/// to `next`: compositors keep the cursor inside the output layout, so a
/// target off every output is clamped into the output `prev` was on.
/// Keeps an accumulated relative drag in step with the visible cursor at
/// the layout's outer edges.
fn confine_pointer(outputs: &[OutRect], prev: (f64, f64), next: (f64, f64)) -> (f64, f64) {
    if output_index_at(outputs, next.0.floor() as i32, next.1.floor() as i32).is_some() {
        return next;
    }
    let Some(o) =
        output_index_at(outputs, prev.0.floor() as i32, prev.1.floor() as i32).map(|i| outputs[i])
    else {
        return next;
    };
    (
        next.0.clamp(o.x as f64, (o.x + o.w) as f64 - 1.0),
        next.1.clamp(o.y as f64, (o.y + o.h) as f64 - 1.0),
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DragEnd {
    /// Button released over the preview.
    Release,
    /// A drag whose release never reached us.
    Stale,
}

/// New position for a preview at `pos` (on the monitor whose origin is
/// `current_origin`) that belongs on `target`, or `None` if it's already
/// there. Its offset within its monitor is kept (see `carry_to_output`).
fn relocation(
    target: OutRect,
    current_origin: (i32, i32),
    pos: (i32, i32),
    outputs: &[OutRect],
    size: (i32, i32),
) -> Option<(i32, i32)> {
    if current_origin == (target.x, target.y) {
        return None;
    }
    Some(
        carry_to_output(target, pos, outputs, size.0, size.1)
            .unwrap_or_else(|| default_position(target, 0)),
    )
}

/// Position for a `w`×`h` preview at `pos` moved onto `target`: kept if
/// already there, else its offset within its own monitor is carried over
/// (clamped). `None` if `pos` isn't on any monitor.
fn carry_to_output(
    target: OutRect,
    pos: (i32, i32),
    outputs: &[OutRect],
    w: i32,
    h: i32,
) -> Option<(i32, i32)> {
    if target.contains(pos.0, pos.1) {
        return Some(target.clamp_box(pos.0, pos.1, w, h));
    }
    let from = outputs[output_index_at(outputs, pos.0, pos.1)?];
    Some(target.clamp_box(
        target.x + (pos.0 - from.x),
        target.y + (pos.1 - from.y),
        w,
        h,
    ))
}

/// Picker entry for a monitor: "DP-3 · LG ULTRAGEAR+ (3840×2160)".
fn output_choice(name: &str, model: &str, size: (i32, i32)) -> OutputChoice {
    let model = model.trim();
    let label = if model.is_empty() || model.eq_ignore_ascii_case("unknown") {
        format!("{name} ({}×{})", size.0, size.1)
    } else {
        format!("{name} · {model} ({}×{})", size.0, size.1)
    };
    OutputChoice {
        name: name.to_string(),
        label,
    }
}

#[derive(Default)]
struct WlrToplevel {
    title: String,
    activated: bool,
    /// Outputs the window is currently on (output_enter/leave).
    outputs: Vec<WlOutput>,
    pending_title: Option<String>,
    pending_activated: Option<bool>,
}

struct Manager {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    subcompositor: SubcompositorState,
    shm: Shm,
    layer_shell: LayerShell,
    viewporter: WpViewporter,
    alpha_modifier: Option<WpAlphaModifierV1>,
    toplevel_list: ForeignToplevelList,
    source_manager: ExtForeignToplevelImageCaptureSourceManagerV1,
    capture_manager: ExtImageCopyCaptureManagerV1,
    dmabuf: Option<ZwpLinuxDmabufV1>,
    /// GBM device for dmabuf allocation, keyed by the `dev_t` it was
    /// opened for.
    gbm: Option<(u64, gbm::Device<std::fs::File>)>,
    /// Use shm only: forced by env, or after dmabuf allocation/capture
    /// failed.
    dmabuf_disabled: bool,
    /// Kept bound so its toplevel events keep flowing.
    _wlr_manager: Option<ZwlrForeignToplevelManagerV1>,
    wlr_toplevels: HashMap<wayland_client::backend::ObjectId, WlrToplevel>,
    pointer: Option<WlPointer>,
    relative_pointer_state: RelativePointerState,
    /// Drives drags when available: relative motion doesn't depend on
    /// where the (moving) preview surface currently is.
    relative_pointer: Option<ZwpRelativePointerV1>,
    chrome_pool: SlotPool,

    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
    live: Arc<Mutex<LiveSettings>>,
    positions: Arc<Mutex<PreviewPositions>>,

    previews: HashMap<u64, Preview>,
    next_id: u64,
    next_offset: i32,
    /// Character name of the focused EVE client, if any.
    active_character: Option<String>,
    capture_interval: Duration,
    title_font: TextFont,
    title_font_px: f32,
    theme: PreviewTheme,
    /// `None`: pick per output from its reported subpixel layout.
    antialias: Option<Antialias>,
    last_applied_opacity: u32,
    needs_reconcile: bool,
    last_reconcile: Instant,
    /// List display mode window, when that mode is on.
    list: Option<ListWindow>,
    list_palette: PanelPalette,
    /// "Nicotine" wordmark for the list's title strip.
    list_title: TextMask,
}

impl Manager {
    fn connect(
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
        live: Arc<Mutex<LiveSettings>>,
        font: FontSettings,
        theme: PreviewTheme,
        list_palette: PanelPalette,
    ) -> Result<(EventQueue<Self>, Self)> {
        let title_font = TextFont::load_or_bundled(font.font.as_deref());
        let antialias = match font.antialias.as_deref() {
            None => None,
            Some(v) => Antialias::from_setting(v).unwrap_or_else(|| {
                eprintln!("Unknown preview_font_antialias '{v}'; using auto");
                None
            }),
        };
        println!(
            "Preview title font: {} at {} px/em",
            title_font.source, font.size_px
        );
        let conn = Connection::connect_to_env().context("connect to Wayland display")?;
        let (globals, mut queue) = registry_queue_init::<Self>(&conn).context("registry init")?;
        let qh = queue.handle();

        let compositor = CompositorState::bind(&globals, &qh).context("wl_compositor")?;
        let subcompositor =
            SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
                .context("wl_subcompositor")?;
        let shm = Shm::bind(&globals, &qh).context("wl_shm")?;
        let layer_shell = LayerShell::bind(&globals, &qh).context("zwlr_layer_shell_v1")?;
        let viewporter: WpViewporter = bind(&globals, &qh, "wp_viewporter")?;
        let source_manager: ExtForeignToplevelImageCaptureSourceManagerV1 = bind(
            &globals,
            &qh,
            "ext_foreign_toplevel_image_capture_source_manager_v1",
        )?;
        let capture_manager: ExtImageCopyCaptureManagerV1 =
            bind(&globals, &qh, "ext_image_copy_capture_manager_v1")?;
        let alpha_modifier: Option<WpAlphaModifierV1> = globals.bind(&qh, 1..=1, ()).ok();
        // v3: modifiers in `add`, and no feedback objects to manage.
        let dmabuf: Option<ZwpLinuxDmabufV1> = globals.bind(&qh, 3..=3, ()).ok();
        let dmabuf_disabled =
            dmabuf.is_none() || std::env::var("NICOTINE_PREVIEW_DMABUF").is_ok_and(|v| v == "0");
        // Bind wl_outputs BEFORE the wlr toplevel manager: the compositor
        // only sends a toplevel's output_enter for outputs this client has
        // already bound, and doesn't replay them for outputs bound later.
        // Binding in the other order left every EVE window with no known
        // output, so new previews fell back to the first output.
        let output_state = OutputState::new(&globals, &qh);
        let wlr_manager: Option<ZwlrForeignToplevelManagerV1> = globals.bind(&qh, 1..=3, ()).ok();
        if !globals.contents().with_list(|l| {
            l.iter()
                .any(|g| g.interface == "ext_foreign_toplevel_list_v1")
        }) {
            anyhow::bail!("compositor lacks ext_foreign_toplevel_list_v1");
        }
        let toplevel_list = ForeignToplevelList::new(&globals, &qh);
        let chrome_pool = SlotPool::new(512 * 1024, &shm).context("chrome shm pool")?;

        let capture_fps = std::env::var("NICOTINE_PREVIEW_FPS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .filter(|f| (1..=240).contains(f))
            .unwrap_or(DEFAULT_CAPTURE_FPS);

        let mut manager = Manager {
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state,
            compositor,
            subcompositor,
            shm,
            layer_shell,
            viewporter,
            alpha_modifier,
            toplevel_list,
            source_manager,
            capture_manager,
            dmabuf,
            gbm: None,
            dmabuf_disabled,
            _wlr_manager: wlr_manager,
            wlr_toplevels: HashMap::new(),
            pointer: None,
            relative_pointer_state: RelativePointerState::bind(&globals, &qh),
            relative_pointer: None,
            chrome_pool,
            wm,
            state,
            live,
            positions: Arc::new(Mutex::new(PreviewPositions::load())),
            previews: HashMap::new(),
            next_id: 1,
            next_offset: 0,
            active_character: None,
            capture_interval: Duration::from_secs(1) / capture_fps,
            title_font,
            title_font_px: font.size_px,
            theme,
            antialias,
            last_applied_opacity: 0,
            needs_reconcile: true,
            last_reconcile: Instant::now(),
            list: None,
            list_palette,
            list_title: text_render::rasterize(
                &TextFont::marlboro(),
                "Nicotine",
                LIST_TITLE_EM_PX,
                Antialias::Grayscale,
            ),
        };
        // Two roundtrips: the first delivers globals' initial events
        // (outputs, toplevels), the second the follow-ups they trigger
        // (xdg-output info, toplevel done).
        queue.roundtrip(&mut manager).context("initial roundtrip")?;
        queue.roundtrip(&mut manager).context("second roundtrip")?;
        manager.publish_outputs();
        println!(
            "Wayland preview manager started (layer-shell overlay + ext-image-copy-capture, {} buffers, {capture_fps} fps cap)",
            if manager.dmabuf_disabled { "shm" } else { "dmabuf" }
        );
        Ok((queue, manager))
    }

    fn run(&mut self, queue: &mut EventQueue<Self>) -> Result<()> {
        loop {
            self.pump_events(queue)?;
            self.tick(&queue.handle());
        }
    }

    /// Flush requests, wait up to `TICK` for compositor events, and
    /// dispatch whatever arrived.
    fn pump_events(&mut self, queue: &mut EventQueue<Self>) -> Result<()> {
        queue.dispatch_pending(self).context("dispatch")?;
        queue.flush().context("flush")?;
        if let Some(guard) = queue.prepare_read() {
            let readable = {
                let fd = guard.connection_fd();
                let mut fds = [nix::poll::PollFd::new(
                    fd.as_fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                match nix::poll::poll(
                    &mut fds,
                    nix::poll::PollTimeout::from(TICK.as_millis() as u16),
                ) {
                    Ok(n) => n > 0,
                    Err(nix::errno::Errno::EINTR) => false,
                    Err(e) => return Err(e).context("poll wayland fd"),
                }
            };
            if readable {
                match guard.read() {
                    Ok(_) => {}
                    Err(wayland_client::backend::WaylandError::Io(e))
                        if e.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e).context("read wayland events"),
                }
            }
        }
        queue.dispatch_pending(self).context("dispatch")?;
        Ok(())
    }

    /// Periodic work: reconcile when due, then start any captures whose
    /// frame interval has elapsed.
    fn tick(&mut self, qh: &QueueHandle<Self>) {
        if self.needs_reconcile || self.last_reconcile.elapsed() >= RECONCILE_INTERVAL {
            self.reconcile(qh);
            self.needs_reconcile = false;
            self.last_reconcile = Instant::now();
        }
        self.apply_drag_moves();
        self.start_captures(qh);
    }

    // ---- reconcile -------------------------------------------------------

    fn reconcile(&mut self, qh: &QueueHandle<Self>) {
        let (show, mode, hide_active, width, height, opacity) = {
            let l = self.live.lock_recover();
            (
                l.show_previews,
                l.display_mode,
                l.hide_active_preview,
                l.preview_width,
                l.preview_height,
                l.preview_opacity,
            )
        };
        if !show {
            self.previews.clear();
            self.list = None;
            return;
        }
        if mode == DisplayMode::List {
            self.previews.clear();
            self.reconcile_list(qh);
            return;
        }
        self.list = None;
        self.relocate_previews();

        // Characters the daemon has already process-gated (pid →
        // exefile.exe via hyprctl), so a browser tab titled "EVE - …"
        // never gets a preview.
        let eligible: HashSet<String> = self
            .state
            .lock_recover()
            .get_windows()
            .iter()
            .map(|w| w.title.clone())
            .collect();

        let mut desired: HashMap<String, ExtForeignToplevelHandleV1> = HashMap::new();
        for handle in self.toplevel_list.toplevels() {
            let Some(info) = self.toplevel_list.info(handle) else {
                continue;
            };
            let Some(name) = info.title.strip_prefix("EVE - ") else {
                continue;
            };
            if !eligible.contains(name) {
                continue;
            }
            let is_active = self.active_character.as_deref() == Some(name);
            if preview_should_hide(hide_active, is_active) {
                continue;
            }
            desired.insert(name.to_string(), handle.clone());
        }

        self.previews.retain(|_, p| {
            !p.capture.stopped && desired.get(&p.name).is_some_and(|h| *h == p.handle)
        });
        let existing: HashSet<String> = self.previews.values().map(|p| p.name.clone()).collect();
        for (name, handle) in desired {
            if existing.contains(&name) {
                continue;
            }
            if let Err(e) = self.create_preview(qh, &name, handle, width, height) {
                eprintln!("Create Wayland preview '{name}' failed: {e:#}");
            }
        }

        // Live size changes from the config panel.
        for p in self.previews.values_mut() {
            if (p.width, p.height) != (width, height) && width > 0 && height > 0 {
                p.width = width;
                p.height = height;
                p.layer.set_size(width, height);
                let (tw, th) = p.thumb_size();
                p.thumb_viewport.set_destination(tw as i32, th as i32);
                p.layer.commit();
                // The configure that follows repaints the chrome.
            }
        }

        if opacity != self.last_applied_opacity {
            self.last_applied_opacity = opacity;
            for p in self.previews.values() {
                apply_opacity(p, opacity);
            }
        }
    }

    /// Antialiasing for text on `output`: subpixel when it reports a
    /// horizontal RGB/BGR layout, grayscale otherwise.
    fn output_antialias(&self, output: &WlOutput) -> Antialias {
        match self.output_state.info(output).map(|i| i.subpixel) {
            Some(wl_output::Subpixel::HorizontalRgb) => Antialias::SubpixelRgb,
            Some(wl_output::Subpixel::HorizontalBgr) => Antialias::SubpixelBgr,
            _ => Antialias::Grayscale,
        }
    }

    /// Every output with known logical geometry, in a stable order.
    fn output_rects(&self) -> Vec<(WlOutput, OutRect)> {
        self.output_state
            .outputs()
            .filter_map(|output| {
                let info = self.output_state.info(&output)?;
                let (pos, size) = (info.logical_position?, info.logical_size?);
                Some((
                    output,
                    OutRect {
                        x: pos.0,
                        y: pos.1,
                        w: size.0,
                        h: size.1,
                    },
                ))
            })
            .collect()
    }

    /// The monitor chosen in the panel ("Preview monitor"), if it's set and
    /// connected. `None` = auto placement.
    fn fixed_output(&self) -> Option<OutRect> {
        let name = self.live.lock_recover().preview_output.clone()?;
        self.output_state.outputs().find_map(|o| {
            let info = self.output_state.info(&o)?;
            if info.name.as_deref() != Some(name.as_str()) {
                return None;
            }
            let (pos, size) = (info.logical_position?, info.logical_size?);
            Some(OutRect {
                x: pos.0,
                y: pos.1,
                w: size.0,
                h: size.1,
            })
        })
    }

    /// Share the connected monitors with the config panel's picker.
    fn publish_outputs(&self) {
        let mut outputs: Vec<(i32, i32, OutputChoice)> = self
            .output_state
            .outputs()
            .filter_map(|o| {
                let info = self.output_state.info(&o)?;
                let name = info.name.clone()?;
                let (pos, size) = (info.logical_position?, info.logical_size?);
                Some((pos.0, pos.1, output_choice(&name, &info.model, size)))
            })
            .collect();
        // Left-to-right, top-to-bottom, like the physical layout.
        outputs.sort_by_key(|(x, y, _)| (*x, *y));
        self.live.lock_recover().outputs = outputs.into_iter().map(|(_, _, c)| c).collect();
    }

    /// Monitor preview `name` belongs on: the "Preview monitor" choice,
    /// or with Auto the monitor its EVE client is on. `None` while that
    /// isn't known yet (the preview then stays put).
    fn target_output(&self, name: &str) -> Option<OutRect> {
        self.fixed_output().or_else(|| self.eve_output(name))
    }

    /// Move every preview that isn't on its target monitor there, keeping
    /// its offset within its monitor, persist the new positions, and
    /// rebuild those previews (a layer surface's output is fixed at
    /// creation). Runs each reconcile, so it follows a changed "Preview
    /// monitor" choice and, in Auto, an EVE client moved to another
    /// monitor.
    fn relocate_previews(&mut self) {
        let rects: Vec<OutRect> = self.output_rects().into_iter().map(|(_, r)| r).collect();
        let moves: Vec<(u64, String, (i32, i32))> = self
            .previews
            .values()
            // Never yank a preview out from under an active drag.
            .filter(|p| !p.drag.active)
            .filter_map(|p| {
                let target = self.target_output(&p.name)?;
                let size = (p.width as i32, p.height as i32);
                relocation(target, p.output_origin, (p.x, p.y), &rects, size)
                    .map(|pos| (p.id, p.name.clone(), pos))
            })
            .collect();
        if moves.is_empty() {
            return;
        }
        let mut positions = self.positions.lock_recover();
        for (id, name, (x, y)) in moves {
            positions.set(name, x, y);
            self.previews.remove(&id);
        }
        positions.save();
        drop(positions);
        self.needs_reconcile = true;
    }

    /// Output containing (x, y), else the first known output.
    fn pick_output(&self, x: i32, y: i32) -> Option<OutputGeometry> {
        let outputs = self.output_rects();
        let rects: Vec<OutRect> = outputs.iter().map(|(_, r)| *r).collect();
        let idx = output_index_at(&rects, x, y).unwrap_or(0);
        outputs
            .into_iter()
            .nth(idx)
            .map(|(o, r)| (o, (r.x, r.y), (r.w, r.h)))
    }

    /// Output the EVE client `name` is on, per wlr foreign-toplevel
    /// output_enter events.
    fn eve_output(&self, name: &str) -> Option<OutRect> {
        let title = format!("EVE - {name}");
        let window = self.wlr_toplevels.values().find(|t| t.title == title)?;
        let outputs = self.output_rects();
        window
            .outputs
            .iter()
            .find_map(|wo| outputs.iter().find(|(o, _)| o == wo).map(|(_, r)| *r))
    }

    fn create_preview(
        &mut self,
        qh: &QueueHandle<Self>,
        name: &str,
        handle: ExtForeignToplevelHandleV1,
        width: u32,
        height: u32,
    ) -> Result<()> {
        let saved = self.positions.lock_recover().get(name);
        let rects: Vec<OutRect> = self.output_rects().into_iter().map(|(_, r)| r).collect();
        let (pw, ph) = (width as i32, height as i32);
        let placed = match self.target_output(name) {
            // On its target monitor: keep the saved spot if it's there,
            // else carry its offset over from whatever monitor it was on.
            Some(t) => Some(
                saved
                    .and_then(|pos| carry_to_output(t, pos, &rects, pw, ph))
                    .unwrap_or_else(|| {
                        let off = self.next_offset;
                        self.next_offset = (self.next_offset + 32) % 320;
                        default_position(t, off)
                    }),
            ),
            None => saved.filter(|(x, y)| output_index_at(&rects, *x, *y).is_some()),
        };
        let (x, y) = match placed {
            Some(p) => p,
            None => {
                // Target monitor unknown yet: the first output (no output
                // need contain the origin). Relocated once it's known.
                let base = rects
                    .first()
                    .copied()
                    .ok_or_else(|| anyhow::anyhow!("no outputs with known geometry"))?;
                let off = self.next_offset;
                self.next_offset = (self.next_offset + 32) % 320;
                default_position(base, off)
            }
        };
        let (output, output_origin, output_size) = self
            .pick_output(x, y)
            .ok_or_else(|| anyhow::anyhow!("no outputs with known geometry"))?;

        let id = self.next_id;
        self.next_id += 1;

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("nicotine-preview"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(width, height);
        // -1: position relative to the output edge, ignoring bars'
        // exclusive zones, so saved positions mean the same thing
        // regardless of panel layout.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        let (subsurface, thumb_surface) = self
            .subcompositor
            .create_subsurface(layer.wl_surface().clone(), qh);
        subsurface.set_position(BORDER_WIDTH as i32, TITLE_STRIP_HEIGHT as i32);
        subsurface.set_desync();
        // Let clicks on the thumbnail fall through to the parent surface
        // so all pointer handling keys off the layer surface.
        if let Ok(region) = Region::new(&self.compositor) {
            thumb_surface.set_input_region(Some(region.wl_region()));
        }
        let thumb_viewport = self.viewporter.get_viewport(&thumb_surface, qh, ());
        let (tw, th) = thumbnail_size(width as u16, height as u16);
        thumb_viewport.set_destination(tw as i32, th as i32);
        thumb_surface.commit();

        let alpha = self.alpha_modifier.as_ref().map(|m| {
            (
                m.get_surface(layer.wl_surface(), qh, ()),
                m.get_surface(&thumb_surface, qh, ()),
            )
        });

        let source = self.source_manager.create_source(&handle, qh, ());
        let session = self
            .capture_manager
            .create_session(&source, Options::empty(), qh, id);

        let preview = Preview {
            id,
            name: name.to_string(),
            handle,
            layer,
            output_origin,
            output_size,
            x,
            y,
            width,
            height,
            configured: false,
            thumb_surface,
            subsurface,
            thumb_viewport,
            alpha,
            chrome_buf: None,
            title_mask: text_render::rasterize(
                &self.title_font,
                name,
                self.title_font_px,
                self.antialias
                    .unwrap_or_else(|| self.output_antialias(&output)),
            ),
            is_active: self.active_character.as_deref() == Some(name),
            capture: Capture {
                source,
                session,
                frame: None,
                pending: Constraints::default(),
                constraints: Constraints::default(),
                size: None,
                kind: None,
                _pool: None,
                bufs: Vec::new(),
                dmabuf_failures: 0,
                last_start: Instant::now() - Duration::from_secs(1),
                stopped: false,
            },
            drag: DragState::default(),
            press_local: (0.0, 0.0),
            drag_delta: (0.0, 0.0),
            pending_move: false,
        };
        preview.apply_margins();
        apply_opacity(&preview, self.live.lock_recover().preview_opacity);
        preview.layer.commit();
        self.previews.insert(id, preview);
        Ok(())
    }

    // ---- chrome ----------------------------------------------------------

    fn draw_chrome(&mut self, id: u64) {
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        if !p.configured {
            return;
        }
        let (w, h) = (p.width as i32, p.height as i32);
        let stride = w * 4;
        let Ok((buffer, canvas)) =
            self.chrome_pool
                .create_buffer(w, h, stride, wl_shm::Format::Argb8888)
        else {
            eprintln!("Wayland preview: chrome buffer allocation failed");
            return;
        };
        let (chrome, text) = self.theme.chrome(p.is_active);
        paint_chrome(
            canvas,
            w as usize,
            h as usize,
            chrome,
            text,
            self.theme.background,
            &p.title_mask,
        );

        let surface = p.layer.wl_surface();
        if buffer.attach_to(surface).is_err() {
            return;
        }
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        p.chrome_buf = Some(buffer);
    }

    fn set_active_character(&mut self, name: Option<String>) {
        if name == self.active_character {
            return;
        }
        self.active_character = name;
        let ids: Vec<u64> = self.previews.keys().copied().collect();
        for id in ids {
            let Some(p) = self.previews.get_mut(&id) else {
                continue;
            };
            let active = self.active_character.as_deref() == Some(p.name.as_str());
            if p.is_active != active {
                p.is_active = active;
                self.draw_chrome(id);
            }
        }
        self.draw_list();
        // hide_active_preview depends on this; reconcile right away.
        self.needs_reconcile = true;
    }

    // ---- list mode -------------------------------------------------------

    /// Rows for the list: running clients in cycle order.
    fn list_names(&self) -> Vec<String> {
        self.state
            .lock_recover()
            .get_ordered_windows()
            .into_iter()
            .map(|w| w.title)
            .collect()
    }

    /// Monitor the list belongs on: the "Preview monitor" choice, or with
    /// Auto the first listed client's monitor (stable across cycling, so
    /// the list doesn't hop between monitors when clients are spread out).
    fn list_target_output(&self, names: &[String]) -> Option<OutRect> {
        self.fixed_output()
            .or_else(|| names.first().and_then(|n| self.eve_output(n)))
    }

    fn reconcile_list(&mut self, qh: &QueueHandle<Self>) {
        let names = self.list_names();
        let target = self.list_target_output(&names);

        // Follow the target monitor (rebuild there, offset kept).
        if let (Some(list), Some(t)) = (&self.list, target) {
            if !list.drag.active {
                let rects: Vec<OutRect> = self.output_rects().into_iter().map(|(_, r)| r).collect();
                let size = (list.width as i32, list.height as i32);
                if let Some(pos) = relocation(t, list.output_origin, (list.x, list.y), &rects, size)
                {
                    let mut positions = self.positions.lock_recover();
                    positions.list_position = Some(pos);
                    positions.save();
                    drop(positions);
                    self.list = None;
                }
            }
        }

        let Some(list) = &mut self.list else {
            if let Err(e) = self.create_list(qh, names, target) {
                eprintln!("Create Wayland list window failed: {e:#}");
            }
            return;
        };
        if list.names == names {
            return;
        }
        let row_px = self.title_font_px.min(LIST_ROW_MAX_EM_PX);
        let aa = self.antialias.unwrap_or(Antialias::Grayscale);
        list.row_masks = names
            .iter()
            .map(|n| text_render::rasterize(&self.title_font, n, row_px, aa))
            .collect();
        let height = list_window_height(names.len()) as u32;
        list.names = names;
        if height != list.height {
            // Redrawn on the configure that follows.
            list.height = height;
            list.layer.set_size(list.width, height);
            list.layer.commit();
        } else {
            self.draw_list();
        }
    }

    fn create_list(
        &mut self,
        qh: &QueueHandle<Self>,
        names: Vec<String>,
        target: Option<OutRect>,
    ) -> Result<()> {
        let (w, h) = (LIST_WIDTH as u32, list_window_height(names.len()) as u32);
        let saved = self.positions.lock_recover().list_position;
        let rects: Vec<OutRect> = self.output_rects().into_iter().map(|(_, r)| r).collect();
        let (x, y) = match target {
            Some(t) => saved
                .and_then(|pos| carry_to_output(t, pos, &rects, w as i32, h as i32))
                .unwrap_or_else(|| default_position(t, 0)),
            None => saved
                .filter(|(x, y)| output_index_at(&rects, *x, *y).is_some())
                .or_else(|| rects.first().map(|r| default_position(*r, 0)))
                .ok_or_else(|| anyhow::anyhow!("no outputs with known geometry"))?,
        };
        let (output, output_origin, output_size) = self
            .pick_output(x, y)
            .ok_or_else(|| anyhow::anyhow!("no outputs with known geometry"))?;

        let surface = self.compositor.create_surface(qh);
        let layer = self.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("nicotine-list"),
            Some(&output),
        );
        layer.set_anchor(Anchor::TOP | Anchor::LEFT);
        layer.set_size(w, h);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);

        let row_px = self.title_font_px.min(LIST_ROW_MAX_EM_PX);
        let aa = self
            .antialias
            .unwrap_or_else(|| self.output_antialias(&output));
        let row_masks = names
            .iter()
            .map(|n| text_render::rasterize(&self.title_font, n, row_px, aa))
            .collect();

        let list = ListWindow {
            layer,
            output_origin,
            output_size,
            x,
            y,
            width: w,
            height: h,
            configured: false,
            buf: None,
            names,
            row_masks,
            drag: DragState::default(),
            press_local: (0.0, 0.0),
            drag_delta: (0.0, 0.0),
            pending_move: false,
        };
        list.apply_margins();
        list.layer.commit();
        self.list = Some(list);
        Ok(())
    }

    fn draw_list(&mut self) {
        let Some(list) = &mut self.list else {
            return;
        };
        if !list.configured {
            return;
        }
        let (w, h) = (list.width as i32, list.height as i32);
        let Ok((buffer, canvas)) =
            self.chrome_pool
                .create_buffer(w, h, w * 4, wl_shm::Format::Argb8888)
        else {
            eprintln!("Wayland list: buffer allocation failed");
            return;
        };
        let active = self
            .active_character
            .as_deref()
            .and_then(|a| list.names.iter().position(|n| n == a));
        paint_list(
            canvas,
            w as usize,
            h as usize,
            &self.list_palette,
            &self.list_title,
            &list.row_masks,
            active,
        );
        let surface = list.layer.wl_surface();
        if buffer.attach_to(surface).is_err() {
            return;
        }
        surface.damage_buffer(0, 0, w, h);
        surface.commit();
        list.buf = Some(buffer);
    }

    /// Release (or a lost release) on the list: a drag saves its
    /// position, a click on a row focuses that client.
    fn finish_list_drag(&mut self, end: DragEnd) {
        self.apply_drag_moves();
        let Some(list) = &mut self.list else {
            return;
        };
        if !list.drag.active {
            return;
        }
        let dragged = list.drag.dragged;
        list.drag = DragState::default();
        if dragged {
            let mut positions = self.positions.lock_recover();
            positions.list_position = Some((list.x, list.y));
            positions.save();
            return;
        }
        if end != DragEnd::Release {
            return;
        }
        let row = list_row_at(list.press_local.1, list.names.len());
        if let Some(name) = row.map(|i| list.names[i].clone()) {
            self.activate(&name);
        }
    }

    fn handle_list_pointer(&mut self, event: &PointerEvent) {
        let locked = self.live.lock_recover().positions_locked;
        match event.kind {
            PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                if let Some(l) = &mut self.list {
                    l.drag = DragState {
                        active: true,
                        dragged: false,
                        origin_screen: (
                            l.x + event.position.0 as i32,
                            l.y + event.position.1 as i32,
                        ),
                        origin_window: (l.x, l.y),
                    };
                    l.press_local = event.position;
                    l.drag_delta = (0.0, 0.0);
                    l.pending_move = false;
                }
            }
            PointerEventKind::Motion { .. } if self.relative_pointer.is_none() => {
                let Some(l) = &mut self.list else {
                    return;
                };
                if !l.drag.active || locked {
                    return;
                }
                l.drag_delta = (
                    (l.x as f64 + event.position.0) - l.drag.origin_screen.0 as f64,
                    (l.y as f64 + event.position.1) - l.drag.origin_screen.1 as f64,
                );
                l.pending_move = true;
            }
            PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                self.finish_list_drag(DragEnd::Release);
            }
            PointerEventKind::Enter { .. } => self.settle_stale_drags(),
            _ => {}
        }
    }

    /// A drag still marked active when the pointer enters one of our
    /// surfaces lost its release to another surface (Hyprland can stop
    /// delivering events to a layer surface once the cursor leaves it):
    /// settle it.
    fn settle_stale_drags(&mut self) {
        let stale: Vec<u64> = self
            .previews
            .values()
            .filter(|p| p.drag.active)
            .map(|p| p.id)
            .collect();
        for id in stale {
            self.finish_drag(id, DragEnd::Stale);
        }
        if self.list.as_ref().is_some_and(|l| l.drag.active) {
            self.finish_list_drag(DragEnd::Stale);
        }
    }

    // ---- capture ---------------------------------------------------------

    fn on_constraints_done(&mut self, id: u64, qh: &QueueHandle<Self>) {
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        let cap = &mut p.capture;
        cap.constraints = std::mem::take(&mut cap.pending);
        let want_kind = if self.dmabuf_disabled {
            BufKind::Shm
        } else {
            BufKind::Dmabuf
        };
        if cap.size == cap.constraints.size && cap.kind == Some(want_kind) && !cap.bufs.is_empty() {
            return;
        }
        self.allocate_capture_buffers(id, qh);
    }

    /// (Re)allocate a preview's capture buffers for its current
    /// constraints: dmabufs unless disabled, falling back to shm.
    fn allocate_capture_buffers(&mut self, id: u64, qh: &QueueHandle<Self>) {
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        p.capture.free_buffers();
        p.capture.kind = None;
        let constraints = p.capture.constraints.clone();
        let Some((w, h)) = constraints.size else {
            return;
        };
        let name = p.name.clone();

        if !self.dmabuf_disabled {
            match self.alloc_dmabuf_buffers(id, &constraints, qh) {
                Ok(bufs) => {
                    let cap = &mut self.previews.get_mut(&id).unwrap().capture;
                    cap.bufs = bufs;
                    cap.size = Some((w, h));
                    cap.kind = Some(BufKind::Dmabuf);
                    cap.dmabuf_failures = 0;
                    return;
                }
                Err(e) => {
                    eprintln!("Wayland preview: dmabuf capture unavailable ({e:#}); using shm");
                    self.dmabuf_disabled = true;
                }
            }
        }

        let Some(format) = pick_shm_format(&constraints.shm_formats) else {
            eprintln!("Wayland preview '{name}': no shm capture format offered");
            return;
        };
        let (wi, hi) = (w as i32, h as i32);
        let stride = wi * 4;
        let frame_len = (stride * hi) as usize;
        let mut pool = match RawPool::new(frame_len * CAPTURE_BUFFERS, &self.shm) {
            Ok(pool) => pool,
            Err(e) => {
                eprintln!("Wayland preview '{name}': capture pool: {e}");
                return;
            }
        };
        let cap = &mut self.previews.get_mut(&id).unwrap().capture;
        for idx in 0..CAPTURE_BUFFERS {
            let buffer = pool.create_buffer(
                (idx * frame_len) as i32,
                wi,
                hi,
                stride,
                format,
                BufTag { preview: id, idx },
                qh,
            );
            cap.bufs.push(CaptureBuf {
                buffer,
                on_screen: false,
                _bo: None,
            });
        }
        cap._pool = Some(pool);
        cap.size = Some((w, h));
        cap.kind = Some(BufKind::Shm);
    }

    /// GBM device for `dev`, opened on first use (render node preferred).
    fn gbm_device(&mut self, dev: u64) -> Result<&gbm::Device<std::fs::File>> {
        if self.gbm.as_ref().is_none_or(|(d, _)| *d != dev) {
            let node = open_render_node(dev)?;
            let device = gbm::Device::new(node).context("gbm_create_device")?;
            self.gbm = Some((dev, device));
        }
        Ok(&self.gbm.as_ref().unwrap().1)
    }

    fn alloc_dmabuf_buffers(
        &mut self,
        id: u64,
        constraints: &Constraints,
        qh: &QueueHandle<Self>,
    ) -> Result<Vec<CaptureBuf>> {
        let dmabuf = self
            .dmabuf
            .clone()
            .context("compositor lacks zwp_linux_dmabuf_v1")?;
        let (w, h) = constraints.size.context("no buffer size")?;
        let dev = constraints
            .dmabuf_device
            .context("session offered no dmabuf device")?;
        let (fourcc, modifiers) =
            pick_dmabuf_format(&constraints.dmabuf_formats).context("no usable dmabuf format")?;
        let format = gbm::Format::try_from(fourcc)
            .map_err(|_| anyhow::anyhow!("unknown fourcc {fourcc:#x}"))?;
        let explicit: Vec<gbm::Modifier> = modifiers
            .iter()
            .filter(|m| **m != DRM_FORMAT_MOD_INVALID)
            .map(|m| gbm::Modifier::from(*m))
            .collect();
        let device = self.gbm_device(dev)?;

        let mut bufs = Vec::with_capacity(CAPTURE_BUFFERS);
        for idx in 0..CAPTURE_BUFFERS {
            // The compositor renders into it and then samples it.
            let usage = gbm::BufferObjectFlags::RENDERING;
            let bo: gbm::BufferObject<()> = if explicit.is_empty() {
                device.create_buffer_object(w, h, format, usage)
            } else {
                device.create_buffer_object_with_modifiers2(
                    w,
                    h,
                    format,
                    explicit.iter().copied(),
                    usage,
                )
            }
            .context("gbm buffer allocation")?;

            let params = dmabuf.create_params(qh, ());
            let modifier = u64::from(bo.modifier());
            for plane in 0..bo.plane_count() as i32 {
                let fd = bo
                    .fd_for_plane(plane)
                    .map_err(|e| anyhow::anyhow!("export dmabuf plane {plane}: {e}"))?;
                // The fd is dup()ed when the request is serialized, so
                // dropping ours afterwards is fine.
                params.add(
                    fd.as_fd(),
                    plane as u32,
                    bo.offset(plane),
                    bo.stride_for_plane(plane),
                    (modifier >> 32) as u32,
                    modifier as u32,
                );
            }
            let buffer = params.create_immed(
                w as i32,
                h as i32,
                fourcc,
                zwp_linux_buffer_params_v1::Flags::empty(),
                qh,
                BufTag { preview: id, idx },
            );
            params.destroy();
            bufs.push(CaptureBuf {
                buffer,
                on_screen: false,
                _bo: Some(bo),
            });
        }
        Ok(bufs)
    }

    fn start_captures(&mut self, qh: &QueueHandle<Self>) {
        let interval = self.capture_interval;
        for p in self.previews.values_mut() {
            if !p.configured {
                continue;
            }
            let cap = &mut p.capture;
            if cap.stopped || cap.frame.is_some() || cap.last_start.elapsed() < interval {
                continue;
            }
            let Some((w, h)) = cap.size else {
                continue;
            };
            let Some(idx) = cap.bufs.iter().position(|b| !b.on_screen) else {
                continue;
            };
            let frame = cap.session.create_frame(qh, p.id);
            frame.attach_buffer(&cap.bufs[idx].buffer);
            frame.damage_buffer(0, 0, w as i32, h as i32);
            frame.capture();
            cap.frame = Some((frame, idx));
            cap.last_start = Instant::now();
        }
    }

    fn on_frame_ready(&mut self, id: u64) {
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        let Some((frame, idx)) = p.capture.frame.take() else {
            return;
        };
        frame.destroy();
        p.capture.dmabuf_failures = 0;
        let Some(buf) = p.capture.bufs.get_mut(idx) else {
            return;
        };
        buf.on_screen = true;
        p.thumb_surface.attach(Some(&buf.buffer), 0, 0);
        p.thumb_surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        p.thumb_surface.commit();
    }

    fn on_frame_failed(&mut self, id: u64, reason: WEnum<FailureReason>, qh: &QueueHandle<Self>) {
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        if let Some((frame, _)) = p.capture.frame.take() {
            frame.destroy();
        }
        match reason {
            WEnum::Value(FailureReason::Stopped) => {
                p.capture.stopped = true;
                self.needs_reconcile = true;
            }
            // New constraints + done follow and reallocate.
            WEnum::Value(FailureReason::BufferConstraints) => {}
            // Unknown: retried next tick, unless dmabuf captures keep
            // failing, in which case switch everything to shm.
            _ if p.capture.kind == Some(BufKind::Dmabuf) => {
                p.capture.dmabuf_failures += 1;
                if p.capture.dmabuf_failures >= DMABUF_MAX_FAILURES {
                    eprintln!("Wayland preview: dmabuf captures keep failing; using shm");
                    self.dmabuf_disabled = true;
                    let ids: Vec<u64> = self
                        .previews
                        .values()
                        .filter(|p| p.capture.kind == Some(BufKind::Dmabuf))
                        .map(|p| p.id)
                        .collect();
                    for id in ids {
                        if let Some(p) = self.previews.get_mut(&id) {
                            if let Some((frame, _)) = p.capture.frame.take() {
                                frame.destroy();
                            }
                        }
                        self.allocate_capture_buffers(id, qh);
                    }
                }
            }
            _ => {}
        }
    }

    // ---- input -----------------------------------------------------------

    fn preview_for_surface(&self, surface: &WlSurface) -> Option<u64> {
        self.previews
            .values()
            .find(|p| p.layer.wl_surface() == surface)
            .map(|p| p.id)
    }

    fn activate(&self, name: &str) {
        let target = self
            .state
            .lock_recover()
            .get_windows()
            .iter()
            .find(|w| w.title == name)
            .map(|w| w.id);
        let Some(window_id) = target else {
            return;
        };
        // hyprctl round-trip; keep it off the render loop.
        let wm = Arc::clone(&self.wm);
        std::thread::spawn(move || {
            if let Err(e) = wm.activate_window(window_id) {
                eprintln!("Preview click activate failed: {e:#}");
            }
        });
    }

    /// Accumulate relative pointer motion into the active drag.
    fn handle_relative_motion(&mut self, event: &RelativeMotionEvent) {
        if self.live.lock_recover().positions_locked {
            return;
        }
        let rects: Vec<OutRect> = self.output_rects().into_iter().map(|(_, r)| r).collect();
        for p in self.previews.values_mut().filter(|p| p.drag.active) {
            let origin = (p.drag.origin_screen.0 as f64, p.drag.origin_screen.1 as f64);
            let prev = (origin.0 + p.drag_delta.0, origin.1 + p.drag_delta.1);
            let next = confine_pointer(
                &rects,
                prev,
                (prev.0 + event.delta.0, prev.1 + event.delta.1),
            );
            p.drag_delta = (next.0 - origin.0, next.1 - origin.1);
            p.pending_move = true;
        }
        if let Some(l) = self.list.as_mut().filter(|l| l.drag.active) {
            let origin = (l.drag.origin_screen.0 as f64, l.drag.origin_screen.1 as f64);
            let prev = (origin.0 + l.drag_delta.0, origin.1 + l.drag_delta.1);
            let next = confine_pointer(
                &rects,
                prev,
                (prev.0 + event.delta.0, prev.1 + event.delta.1),
            );
            l.drag_delta = (next.0 - origin.0, next.1 - origin.1);
            l.pending_move = true;
        }
    }

    /// Move dragged previews to `origin + drag_delta`, snapped to other
    /// previews and clamped to their output. The position only depends on
    /// the press origin and accumulated displacement, never on where the
    /// surface currently is, so there's no feedback loop with the
    /// compositor applying earlier moves.
    fn apply_drag_moves(&mut self) {
        let ids: Vec<u64> = self
            .previews
            .values()
            .filter(|p| p.pending_move)
            .map(|p| p.id)
            .collect();
        for id in ids {
            let others: Vec<DragRect> = self
                .previews
                .values()
                .filter(|o| o.id != id)
                .map(Preview::rect)
                .collect();
            let Some(p) = self.previews.get_mut(&id) else {
                continue;
            };
            p.pending_move = false;
            if !p.drag.active {
                continue;
            }
            let (dx, dy) = p.drag_delta;
            if !p.drag.dragged {
                if dx.abs() < DRAG_THRESHOLD_PX as f64 && dy.abs() < DRAG_THRESHOLD_PX as f64 {
                    continue;
                }
                p.drag.dragged = true;
            }
            let current = OutRect {
                x: p.output_origin.0,
                y: p.output_origin.1,
                w: p.output_size.0,
                h: p.output_size.1,
            };
            let (x, y) = dragged_position(
                p.drag.origin_window,
                (dx, dy),
                (p.width as i32, p.height as i32),
                &others,
                current,
            );
            if (x, y) != (p.x, p.y) {
                (p.x, p.y) = (x, y);
                p.apply_margins();
                p.layer.commit();
            }
        }

        let others: Vec<DragRect> = self.previews.values().map(Preview::rect).collect();
        if let Some(l) = self.list.as_mut().filter(|l| l.pending_move) {
            l.pending_move = false;
            if !l.drag.active {
                return;
            }
            let (dx, dy) = l.drag_delta;
            if !l.drag.dragged {
                if dx.abs() < DRAG_THRESHOLD_PX as f64 && dy.abs() < DRAG_THRESHOLD_PX as f64 {
                    return;
                }
                l.drag.dragged = true;
            }
            let (x, y) = dragged_position(
                l.drag.origin_window,
                (dx, dy),
                (l.width as i32, l.height as i32),
                &others,
                l.output(),
            );
            if (x, y) != (l.x, l.y) {
                (l.x, l.y) = (x, y);
                l.apply_margins();
                l.layer.commit();
            }
        }
    }

    /// End a press/drag on preview `id`: a press without a drag is a
    /// click (activate its EVE client, unless the release was lost), a
    /// drag saves the new position.
    fn finish_drag(&mut self, id: u64, end: DragEnd) {
        self.apply_drag_moves();
        let Some(p) = self.previews.get_mut(&id) else {
            return;
        };
        if !p.drag.active {
            return;
        }
        let dragged = p.drag.dragged;
        p.drag = DragState::default();
        let (name, x, y) = (p.name.clone(), p.x, p.y);
        if !dragged {
            if end == DragEnd::Release {
                self.activate(&name);
            }
            return;
        }
        let mut positions = self.positions.lock_recover();
        positions.set(name, x, y);
        positions.save();
    }

    fn handle_pointer(&mut self, event: &PointerEvent) {
        if self
            .list
            .as_ref()
            .is_some_and(|l| l.layer.wl_surface() == &event.surface)
        {
            self.handle_list_pointer(event);
            return;
        }
        let Some(id) = self.preview_for_surface(&event.surface) else {
            return;
        };
        let locked = self.live.lock_recover().positions_locked;
        match event.kind {
            PointerEventKind::Press { button, .. } if button == BTN_LEFT => {
                if let Some(p) = self.previews.get_mut(&id) {
                    p.drag = DragState {
                        active: true,
                        dragged: false,
                        origin_screen: (
                            p.x + event.position.0 as i32,
                            p.y + event.position.1 as i32,
                        ),
                        origin_window: (p.x, p.y),
                    };
                    p.press_local = event.position;
                    p.drag_delta = (0.0, 0.0);
                    p.pending_move = false;
                }
            }
            PointerEventKind::Motion { .. } if self.relative_pointer.is_none() => {
                // Fallback without zwp_relative_pointer: derive the
                // displacement from surface-local motion. Can wobble,
                // because events in flight are relative to where the
                // surface was before our last move landed.
                let Some(p) = self.previews.get_mut(&id) else {
                    return;
                };
                if !p.drag.active || locked {
                    return;
                }
                p.drag_delta = (
                    (p.x as f64 + event.position.0) - p.drag.origin_screen.0 as f64,
                    (p.y as f64 + event.position.1) - p.drag.origin_screen.1 as f64,
                );
                p.pending_move = true;
            }
            PointerEventKind::Release { button, .. } if button == BTN_LEFT => {
                self.finish_drag(id, DragEnd::Release);
            }
            // Hyprland may stop sending a layer surface pointer events once
            // the cursor leaves it mid-drag, so the release can go to
            // another surface. Entering any preview while a drag is still
            // marked active means that happened: settle it.
            PointerEventKind::Enter { .. } => self.settle_stale_drags(),
            _ => {}
        }
    }
}

/// Where a surface dragged by `delta` from `origin_window` goes: snapped
/// to the edges of `others`, then kept inside `output` (a layer surface
/// can't straddle monitors, and the "Preview monitor" setting decides
/// which one it's on).
fn dragged_position(
    origin_window: (i32, i32),
    delta: (f64, f64),
    size: (i32, i32),
    others: &[DragRect],
    output: OutRect,
) -> (i32, i32) {
    let (sx, sy) = snap_position(
        origin_window.0 + delta.0.round() as i32,
        origin_window.1 + delta.1.round() as i32,
        size.0,
        size.1,
        others,
        SNAP_THRESHOLD_PX,
    );
    output.clamp_box(sx, sy, size.0, size.1)
}

/// First list row's top edge, in list-surface pixels.
fn list_rows_top() -> f64 {
    (LIST_BORDER + LIST_TITLE_STRIP_HEIGHT) as f64 + LIST_ROW_TOP_PAD as f64
}

/// Row under surface-local `y` in a list of `rows` rows, if any.
fn list_row_at(y: f64, rows: usize) -> Option<usize> {
    let offset = y - list_rows_top();
    if offset < 0.0 {
        return None;
    }
    let row = (offset / LIST_ROW_HEIGHT as f64) as usize;
    (row < rows).then_some(row)
}

/// Paint the list window: `subtle` border, `accent` title strip with the
/// centred wordmark in `on_accent`, `background` body, rows in `text`,
/// the active row in `accent` with a ▸ marker (drawn, so any row font
/// works). `canvas` is ARGB8888 little-endian.
fn paint_list(
    canvas: &mut [u8],
    w: usize,
    h: usize,
    pal: &PanelPalette,
    title: &TextMask,
    rows: &[TextMask],
    active: Option<usize>,
) {
    let px = |c: Rgb| [c.2, c.1, c.0, 0xff];
    let border = LIST_BORDER as usize;
    let strip_bottom = border + LIST_TITLE_STRIP_HEIGHT as usize;
    for y in 0..h {
        for x in 0..w {
            let c = if x < border || y < border || x + border >= w || y + border >= h {
                pal.subtle
            } else if y < strip_bottom {
                pal.accent
            } else {
                pal.background
            };
            let off = (y * w + x) * 4;
            canvas[off..off + 4].copy_from_slice(&px(c));
        }
    }

    let title_x = w.saturating_sub(title.width) / 2;
    let title_y = border + (LIST_TITLE_STRIP_HEIGHT as usize).saturating_sub(title.height) / 2;
    let title_clip = TextMask {
        height: title.height.min(strip_bottom.saturating_sub(title_y)),
        ..title.clone()
    };
    text_render::blend_mask(
        canvas,
        w,
        h,
        title_x,
        title_y,
        w.saturating_sub(title_x + border),
        &title_clip,
        pal.on_accent,
    );

    let left = LIST_ROW_LEFT_PAD as usize;
    let text_w = w.saturating_sub(left + LIST_ROW_RIGHT_PAD as usize);
    let row_h = LIST_ROW_HEIGHT as usize;
    for (i, mask) in rows.iter().enumerate() {
        let row_top = list_rows_top() as usize + i * row_h;
        if row_top + row_h > h.saturating_sub(border) {
            break;
        }
        let is_active = active == Some(i);
        let color = if is_active { pal.accent } else { pal.text };
        if is_active {
            // ▸ in the left padding, vertically centred in the row.
            let (mw, mh) = (5usize, 9usize);
            let mx = left.saturating_sub(mw + 5);
            let my = row_top + (row_h - mh) / 2;
            let half = mh / 2;
            for dy in 0..mh {
                let span = (half - dy.abs_diff(half)) * mw / half + 1;
                for dx in 0..span.min(mw) {
                    let off = ((my + dy) * w + mx + dx) * 4;
                    canvas[off..off + 4].copy_from_slice(&px(color));
                }
            }
        }
        let text_top = row_top + row_h.saturating_sub(mask.height) / 2;
        let clipped = TextMask {
            height: mask.height.min((row_top + row_h).saturating_sub(text_top)),
            ..mask.clone()
        };
        text_render::blend_mask(canvas, w, h, left, text_top, text_w, &clipped, color);
    }
}

fn bind<I>(globals: &GlobalList, qh: &QueueHandle<Manager>, name: &str) -> Result<I>
where
    I: Proxy + 'static,
    Manager: Dispatch<I, ()>,
{
    globals
        .bind(qh, 1..=1, ())
        .with_context(|| format!("compositor lacks {name}"))
}

/// Preferred shm capture format among those offered.
fn pick_shm_format(offered: &[wl_shm::Format]) -> Option<wl_shm::Format> {
    [wl_shm::Format::Xrgb8888, wl_shm::Format::Argb8888]
        .into_iter()
        .find(|f| offered.contains(f))
        .or_else(|| offered.first().copied())
}

/// Preferred dmabuf capture format (XRGB, then ARGB) among those offered.
fn pick_dmabuf_format(offered: &[(u32, Vec<u64>)]) -> Option<(u32, &[u64])> {
    [DRM_FORMAT_XRGB8888, DRM_FORMAT_ARGB8888]
        .into_iter()
        .find_map(|want| {
            offered
                .iter()
                .find(|(f, _)| *f == want)
                .map(|(f, m)| (*f, m.as_slice()))
        })
}

/// Decode a little-endian u64 array (modifiers, `dev_t`) from a protocol
/// byte array.
fn u64s_from_bytes(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|c| u64::from_ne_bytes(*c))
        .collect()
}

/// Open the DRM node for `dev` (a `dev_t` from `dmabuf_device`),
/// preferring the same GPU's render node: allocation needs no DRM master,
/// and the advertised node may be the primary one.
fn open_render_node(dev: u64) -> Result<std::fs::File> {
    use std::os::unix::fs::MetadataExt;
    let open = |path: &std::path::Path| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("open {}", path.display()))
    };
    let (major, minor) = (nix::sys::stat::major(dev), nix::sys::stat::minor(dev));
    let sys = format!("/sys/dev/char/{major}:{minor}/device/drm");
    if let Ok(entries) = std::fs::read_dir(&sys) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name.to_string_lossy().starts_with("renderD") {
                return open(&std::path::Path::new("/dev/dri").join(name));
            }
        }
    }
    for entry in std::fs::read_dir("/dev/dri")
        .context("read /dev/dri")?
        .flatten()
    {
        if entry.metadata().is_ok_and(|m| m.rdev() == dev) {
            return open(&entry.path());
        }
    }
    anyhow::bail!("no DRM node for device {major}:{minor}")
}

fn apply_opacity(p: &Preview, percent: u32) {
    if let Some((chrome, thumb)) = &p.alpha {
        let factor = ((percent.clamp(1, 100) as u64 * u32::MAX as u64) / 100) as u32;
        chrome.set_multiplier(factor);
        thumb.set_multiplier(factor);
        // Double-buffered: takes effect on the next commit of each surface.
        p.thumb_surface.commit();
        p.layer.commit();
    }
}

/// Paint title strip + border in `chrome`, the thumbnail well in black,
/// and blend the pre-rasterized title text in cream. `canvas` is
/// ARGB8888 little-endian (B, G, R, A byte order).
/// Paint title strip + border in `chrome`, the thumbnail well in
/// `background`, and blend the title text in `text_color`. `canvas` is
/// ARGB8888 little-endian (B, G, R, A byte order).
fn paint_chrome(
    canvas: &mut [u8],
    w: usize,
    h: usize,
    chrome: Rgb,
    text_color: Rgb,
    background: Rgb,
    text: &TextMask,
) {
    let px = |c: Rgb| [c.2, c.1, c.0, 0xff];
    let chrome_px = px(chrome);
    let well_px = px(background);
    let title_h = TITLE_STRIP_HEIGHT as usize;
    let border = BORDER_WIDTH as usize;
    for y in 0..h {
        for x in 0..w {
            let in_well = y >= title_h
                && y < h.saturating_sub(border)
                && x >= border
                && x < w.saturating_sub(border);
            let off = (y * w + x) * 4;
            canvas[off..off + 4].copy_from_slice(if in_well { &well_px } else { &chrome_px });
        }
    }

    // Vertically centre the text box in the strip; clip to the strip and
    // to the left/right padding.
    let left = TITLE_TEXT_LEFT_PAD as usize;
    let top = title_h.saturating_sub(text.height) / 2;
    let clipped = TextMask {
        height: text.height.min(title_h.saturating_sub(top)),
        ..text.clone()
    };
    text_render::blend_mask(
        canvas,
        w,
        h,
        left,
        top,
        w.saturating_sub(left * 2),
        &clipped,
        text_color,
    );
}

// ============================================================================
// sctk handlers
// ============================================================================

impl CompositorHandler for Manager {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlSurface, _: u32) {}
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &WlOutput,
    ) {
    }
}

impl OutputHandler for Manager {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {
        self.publish_outputs();
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {
        self.publish_outputs();
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlOutput) {
        self.publish_outputs();
    }
}

impl LayerShellHandler for Manager {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        // Output went away (or the compositor closed us); drop the
        // surface and let reconcile recreate it on a live output.
        self.previews.retain(|_, p| &p.layer != layer);
        if self.list.as_ref().is_some_and(|l| &l.layer == layer) {
            self.list = None;
        }
        self.needs_reconcile = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(list) = self.list.as_mut().filter(|l| &l.layer == layer) {
            let (cw, ch) = configure.new_size;
            if cw > 0 && ch > 0 {
                list.width = cw;
                list.height = ch;
            }
            list.configured = true;
            self.draw_list();
            return;
        }
        let Some(id) = self
            .previews
            .values()
            .find(|p| &p.layer == layer)
            .map(|p| p.id)
        else {
            return;
        };
        if let Some(p) = self.previews.get_mut(&id) {
            let (cw, ch) = configure.new_size;
            if cw > 0 && ch > 0 {
                p.width = cw;
                p.height = ch;
            }
            p.configured = true;
        }
        self.draw_chrome(id);
    }
}

impl ShmHandler for Manager {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl SeatHandler for Manager {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            if let Some(pointer) = &self.pointer {
                self.relative_pointer = self
                    .relative_pointer_state
                    .get_relative_pointer(pointer, qh)
                    .ok();
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Pointer {
            if let Some(rp) = self.relative_pointer.take() {
                rp.destroy();
            }
            if let Some(p) = self.pointer.take() {
                p.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl PointerHandler for Manager {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            self.handle_pointer(event);
        }
    }
}

impl RelativePointerHandler for Manager {
    fn relative_pointer_motion(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpRelativePointerV1,
        _: &WlPointer,
        event: RelativeMotionEvent,
    ) {
        self.handle_relative_motion(&event);
    }
}

impl ForeignToplevelListHandler for Manager {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelList {
        &mut self.toplevel_list
    }
    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: ExtForeignToplevelHandleV1,
    ) {
        self.needs_reconcile = true;
    }
    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: ExtForeignToplevelHandleV1,
    ) {
        // Title changes (e.g. "EVE" → "EVE - <name>" after login).
        self.needs_reconcile = true;
    }
    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: ExtForeignToplevelHandleV1,
    ) {
        self.needs_reconcile = true;
    }
}

impl ProvidesRegistryState for Manager {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Manager);
delegate_subcompositor!(Manager);
delegate_output!(Manager);
delegate_shm!(Manager);
delegate_seat!(Manager);
delegate_pointer!(Manager);
delegate_relative_pointer!(Manager);
delegate_layer!(Manager);
delegate_registry!(Manager);
delegate_foreign_toplevel_list!(Manager);

// ============================================================================
// Raw protocol dispatch (no sctk helpers for these)
// ============================================================================

impl Dispatch<WpViewporter, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &WpViewporter,
        _: <WpViewporter as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpViewport, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &WpViewport,
        _: <WpViewport as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpAlphaModifierV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &WpAlphaModifierV1,
        _: <WpAlphaModifierV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WpAlphaModifierSurfaceV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &WpAlphaModifierSurfaceV1,
        _: <WpAlphaModifierSurfaceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtForeignToplevelImageCaptureSourceManagerV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ExtForeignToplevelImageCaptureSourceManagerV1,
        _: <ExtForeignToplevelImageCaptureSourceManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCaptureSourceV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ExtImageCaptureSourceV1,
        _: <ExtImageCaptureSourceV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ExtImageCopyCaptureManagerV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ExtImageCopyCaptureManagerV1,
        _: <ExtImageCopyCaptureManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxDmabufV1,
        _: <ZwpLinuxDmabufV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // v3 format/modifier advertisements; capture sessions tell us
        // exactly which formats to use instead.
    }
}

impl Dispatch<ZwpLinuxBufferParamsV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ZwpLinuxBufferParamsV1,
        _: <ZwpLinuxBufferParamsV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Only create_immed is used; its params are destroyed right away.
    }
}

impl Dispatch<ExtImageCopyCaptureSessionV1, u64> for Manager {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        id: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                if let Some(p) = state.previews.get_mut(id) {
                    p.capture.pending.size = Some((width, height));
                }
            }
            Event::ShmFormat {
                format: WEnum::Value(format),
            } => {
                if let Some(p) = state.previews.get_mut(id) {
                    p.capture.pending.shm_formats.push(format);
                }
            }
            Event::DmabufDevice { device } => {
                if let Some(p) = state.previews.get_mut(id) {
                    p.capture.pending.dmabuf_device = u64s_from_bytes(&device).first().copied();
                }
            }
            Event::DmabufFormat { format, modifiers } => {
                if let Some(p) = state.previews.get_mut(id) {
                    p.capture
                        .pending
                        .dmabuf_formats
                        .push((format, u64s_from_bytes(&modifiers)));
                }
            }
            Event::Done => state.on_constraints_done(*id, qh),
            Event::Stopped => {
                if let Some(p) = state.previews.get_mut(id) {
                    p.capture.stopped = true;
                    state.needs_reconcile = true;
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtImageCopyCaptureFrameV1, u64> for Manager {
    fn event(
        state: &mut Self,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        id: &u64,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Ready => state.on_frame_ready(*id),
            Event::Failed { reason } => state.on_frame_failed(*id, reason, qh),
            _ => {}
        }
    }
}

impl Dispatch<WlBuffer, BufTag> for Manager {
    fn event(
        state: &mut Self,
        buffer: &WlBuffer,
        event: wl_buffer::Event,
        tag: &BufTag,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_buffer::Event::Release = event {
            if let Some(b) = state
                .previews
                .get_mut(&tag.preview)
                .and_then(|p| p.capture.bufs.get_mut(tag.idx))
                .filter(|b| &b.buffer == buffer)
            {
                b.on_screen = false;
            }
        }
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for Manager {
    fn event(
        _: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        _: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }

    event_created_child!(Manager, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for Manager {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let key = handle.id();
        match event {
            Event::Title { title } => {
                state.wlr_toplevels.entry(key).or_default().pending_title = Some(title);
            }
            Event::State { state: raw } => {
                let activated = raw
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|c| u32::from_ne_bytes(*c))
                    .any(|s| s == WLR_STATE_ACTIVATED);
                state
                    .wlr_toplevels
                    .entry(key)
                    .or_default()
                    .pending_activated = Some(activated);
            }
            Event::Done => {
                let t = state.wlr_toplevels.entry(key).or_default();
                if let Some(title) = t.pending_title.take() {
                    t.title = title;
                }
                if let Some(a) = t.pending_activated.take() {
                    t.activated = a;
                }
                let active = state
                    .wlr_toplevels
                    .values()
                    .find(|t| t.activated)
                    .and_then(|t| t.title.strip_prefix("EVE - "))
                    .map(str::to_string);
                state.set_active_character(active);
            }
            Event::OutputEnter { output } => {
                let t = state.wlr_toplevels.entry(key).or_default();
                if !t.outputs.contains(&output) {
                    t.outputs.push(output);
                }
            }
            Event::OutputLeave { output } => {
                state
                    .wlr_toplevels
                    .entry(key)
                    .or_default()
                    .outputs
                    .retain(|o| *o != output);
            }
            Event::Closed => {
                state.wlr_toplevels.remove(&key);
                handle.destroy();
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reporter's layout: portrait DP-1 left of DP-3 (EVE), DP-2 to
    /// the right, nothing at the global origin.
    const DP1: OutRect = OutRect {
        x: 4960,
        y: 0,
        w: 1440,
        h: 2560,
    };
    const DP3: OutRect = OutRect {
        x: 6400,
        y: 234,
        w: 3840,
        h: 2160,
    };
    const DP2: OutRect = OutRect {
        x: 10240,
        y: 234,
        w: 3840,
        h: 2160,
    };

    #[test]
    fn origin_is_on_no_output_in_offset_layouts() {
        let outs = [DP1, DP3, DP2];
        assert_eq!(output_index_at(&outs, 0, 0), None);
        assert_eq!(output_index_at(&outs, 5642, 281), Some(0));
        assert_eq!(output_index_at(&outs, 7000, 500), Some(1));
        assert_eq!(output_index_at(&outs, 12000, 500), Some(2));
        // Right/bottom edges are exclusive.
        assert_eq!(output_index_at(&outs, 6400, 300), Some(1));
        assert_eq!(output_index_at(&outs, 6399, 300), Some(0));
    }

    #[test]
    fn default_position_starts_on_given_output() {
        let (x, y) = default_position(DP3, 32);
        assert_eq!((x, y), (6400 + 96, 234 + 96));
        assert!(DP3.contains(x, y));
    }

    #[test]
    fn dmabuf_format_prefers_xrgb_then_argb() {
        let offered = vec![
            (DRM_FORMAT_ARGB8888, vec![1, 2]),
            (DRM_FORMAT_XRGB8888, vec![3]),
        ];
        assert_eq!(
            pick_dmabuf_format(&offered),
            Some((DRM_FORMAT_XRGB8888, &[3u64][..]))
        );
        assert_eq!(
            pick_dmabuf_format(&offered[..1]),
            Some((DRM_FORMAT_ARGB8888, &[1u64, 2][..]))
        );
        assert_eq!(pick_dmabuf_format(&[(0x1234, vec![])]), None);
        // The fourcc constants match the codes Hyprland sends.
        assert_eq!(DRM_FORMAT_XRGB8888, 875713112);
        assert_eq!(DRM_FORMAT_ARGB8888, 875713089);
    }

    #[test]
    fn shm_format_prefers_xrgb() {
        use wl_shm::Format::*;
        assert_eq!(pick_shm_format(&[Argb8888, Xrgb8888]), Some(Xrgb8888));
        assert_eq!(pick_shm_format(&[Argb8888]), Some(Argb8888));
        assert_eq!(pick_shm_format(&[]), None);
    }

    #[test]
    fn decodes_u64_arrays() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&7u64.to_ne_bytes());
        bytes.extend_from_slice(&DRM_FORMAT_MOD_INVALID.to_ne_bytes());
        bytes.push(0xff); // trailing partial chunk is ignored
        assert_eq!(u64s_from_bytes(&bytes), vec![7, DRM_FORMAT_MOD_INVALID]);
    }

    #[test]
    fn confine_pointer_matches_cursor_clamping() {
        let outs = [DP1, DP3, DP2];
        // Moving within or across outputs is untouched.
        assert_eq!(
            confine_pointer(&outs, (7000.0, 500.0), (7100.0, 520.0)),
            (7100.0, 520.0)
        );
        assert_eq!(
            confine_pointer(&outs, (6390.0, 500.0), (6410.0, 500.0)),
            (6410.0, 500.0)
        );
        // Pushing past DP-3's top (into the gap above it) pins to its edge.
        assert_eq!(
            confine_pointer(&outs, (7000.0, 240.0), (7000.0, 100.0)),
            (7000.0, 234.0)
        );
        // Past the layout's right edge pins to DP-2's last column.
        assert_eq!(
            confine_pointer(&outs, (14000.0, 500.0), (14200.0, 500.0)),
            (14079.0, 500.0)
        );
    }

    #[test]
    fn list_rows_hit_test() {
        let top = list_rows_top();
        let row_h = LIST_ROW_HEIGHT as f64;
        // Title strip and the padding above the first row: no row.
        assert_eq!(list_row_at(5.0, 3), None);
        assert_eq!(list_row_at(top - 1.0, 3), None);
        assert_eq!(list_row_at(top, 3), Some(0));
        assert_eq!(list_row_at(top + row_h - 0.5, 3), Some(0));
        assert_eq!(list_row_at(top + row_h, 3), Some(1));
        assert_eq!(list_row_at(top + 2.5 * row_h, 3), Some(2));
        // Below the last row (bottom padding) or an empty list.
        assert_eq!(list_row_at(top + 3.0 * row_h, 3), None);
        assert_eq!(list_row_at(top, 0), None);
    }

    fn pixel(canvas: &[u8], w: usize, x: usize, y: usize) -> Rgb {
        let off = (y * w + x) * 4;
        (canvas[off + 2], canvas[off + 1], canvas[off])
    }

    #[test]
    fn list_paints_theme_roles_and_marks_active_row() {
        for pal in [
            crate::theme::Theme::NICOTINE.panel,
            crate::theme::Theme::NORD_DARK.panel,
        ] {
            let rows = 3;
            let (w, h) = (LIST_WIDTH as usize, list_window_height(rows) as usize);
            let mut canvas = vec![0u8; w * h * 4];
            let solid = TextMask {
                width: 6,
                height: 6,
                baseline: 6,
                subpixel: false,
                data: vec![255; 36],
            };
            let masks = vec![solid.clone(); rows];
            paint_list(
                &mut canvas,
                w,
                h,
                &pal,
                &TextMask::default(),
                &masks,
                Some(1),
            );

            // Border, title strip, body.
            assert_eq!(pixel(&canvas, w, 0, 0), pal.subtle);
            assert_eq!(pixel(&canvas, w, w - 1, h - 1), pal.subtle);
            assert_eq!(pixel(&canvas, w, w / 2, 10), pal.accent);
            assert_eq!(pixel(&canvas, w, w - 10, h - 6), pal.background);

            let row_h = LIST_ROW_HEIGHT as usize;
            let left = LIST_ROW_LEFT_PAD as usize;
            let text_y = |i: usize| list_rows_top() as usize + i * row_h + (row_h - 6) / 2 + 2;
            // Inactive rows' text in `text`, the active row's in `accent`.
            assert_eq!(pixel(&canvas, w, left + 2, text_y(0)), pal.text);
            assert_eq!(pixel(&canvas, w, left + 2, text_y(1)), pal.accent);
            assert_eq!(pixel(&canvas, w, left + 2, text_y(2)), pal.text);
            // ▸ marker only on the active row (its middle row is widest).
            let marker_x = left - 10;
            let mid = |i: usize| list_rows_top() as usize + i * row_h + (row_h - 9) / 2 + 4;
            assert_eq!(pixel(&canvas, w, marker_x, mid(1)), pal.accent);
            assert_eq!(pixel(&canvas, w, marker_x, mid(0)), pal.background);
        }
    }

    #[test]
    fn dragged_position_snaps_then_stays_on_output() {
        let size = (200, 100);
        // Plain move.
        assert_eq!(
            dragged_position((7000, 500), (50.4, -20.6), size, &[], DP3),
            (7050, 479)
        );
        // Snaps to a neighbour's left edge within the threshold.
        let other = DragRect {
            left: 7260,
            top: 900,
            right: 7460,
            bottom: 1000,
        };
        assert_eq!(
            dragged_position((7000, 500), (55.0, 0.0), size, &[other], DP3).0,
            7060
        );
        // Dragged past the monitor's edge: clamped to it.
        assert_eq!(
            dragged_position((7000, 500), (-5000.0, -900.0), size, &[], DP3),
            (DP3.x, DP3.y)
        );
    }

    /// Live: a List-mode window on the running compositor gets configured
    /// and drawn. Run with
    /// `cargo test --bin Nicotine live_hyprland_list_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a live Hyprland session"]
    fn live_hyprland_list_smoke() {
        struct NoWm;
        impl WindowManager for NoWm {
            fn get_eve_windows(&self) -> Result<Vec<crate::window_manager::EveWindow>> {
                Ok(Vec::new())
            }
            fn activate_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn stack_windows(
                &self,
                _: &[crate::window_manager::EveWindow],
                _: &crate::config::Config,
            ) -> Result<()> {
                Ok(())
            }
            fn get_active_window(&self) -> Result<u32> {
                Ok(0)
            }
            fn minimize_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn restore_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
        }
        let state = Arc::new(Mutex::new(CycleState::new()));
        state
            .lock()
            .unwrap()
            .update_windows(vec![crate::window_manager::EveWindow {
                id: 1,
                title: "List Smoke".into(),
            }]);
        let live = Arc::new(Mutex::new(LiveSettings {
            preview_width: 480,
            preview_height: 270,
            preview_opacity: 100,
            display_mode: DisplayMode::List,
            positions_locked: true,
            show_previews: true,
            hide_active_preview: false,
            preview_output: None,
            outputs: Vec::new(),
        }));
        let (mut queue, mut m) = Manager::connect(
            Arc::new(NoWm),
            state,
            live,
            FontSettings {
                font: None,
                size_px: 14.0,
                antialias: None,
            },
            crate::theme::Theme::NORD_DARK.preview,
            crate::theme::Theme::NORD_DARK.panel,
        )
        .expect("bind Wayland globals");
        // Don't touch the user's saved list position.
        m.positions = Arc::new(Mutex::new(PreviewPositions::default()));
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            m.pump_events(&mut queue).expect("wayland connection");
            m.tick(&queue.handle());
        }
        let list = m.list.as_ref().expect("list window created");
        println!(
            "list at ({}, {}) {}x{} rows={:?} configured={} drawn={}",
            list.x,
            list.y,
            list.width,
            list.height,
            list.names,
            list.configured,
            list.buf.is_some()
        );
        assert!(m.previews.is_empty(), "no previews in List mode");
        assert!(list.configured && list.buf.is_some());
        assert_eq!(list.names, ["List Smoke"]);
        m.list = None;
        m.pump_events(&mut queue).unwrap();
    }

    #[test]
    fn relocation_moves_previews_onto_their_target_monitor() {
        let outs = [DP1, DP3, DP2];
        let size = (207, 131);
        // Saved on DP-1 while EVE is on DP-3 (Auto): moved, offset kept.
        assert_eq!(
            relocation(DP3, (DP1.x, DP1.y), (DP1.x + 600, DP1.y + 40), &outs, size),
            Some((DP3.x + 600, DP3.y + 40))
        );
        // Already on the target: stays.
        assert_eq!(
            relocation(DP3, (DP3.x, DP3.y), (7000, 500), &outs, size),
            None
        );
        // Position off every monitor: default spot on the target.
        assert_eq!(
            relocation(DP3, (DP1.x, DP1.y), (0, 0), &outs, size),
            Some(default_position(DP3, 0))
        );
    }

    #[test]
    fn carry_keeps_offset_within_monitor() {
        let outs = [DP1, DP3, DP2];
        // 100,50 into DP-1 → 100,50 into DP-3.
        assert_eq!(
            carry_to_output(DP3, (DP1.x + 100, DP1.y + 50), &outs, 207, 131),
            Some((DP3.x + 100, DP3.y + 50))
        );
        // Already on the target: unchanged.
        assert_eq!(
            carry_to_output(DP3, (7000, 500), &outs, 207, 131),
            Some((7000, 500))
        );
        // An offset past the narrower (portrait) DP-1 is clamped on x;
        // y = 2100 still fits its 2560 px height.
        assert_eq!(
            carry_to_output(DP1, (DP3.x + 3800, DP3.y + 2100), &outs, 207, 131),
            Some((DP1.x + DP1.w - 207, DP1.y + 2100))
        );
        // From the taller DP-1 to DP-3, a low offset is clamped on y.
        assert_eq!(
            carry_to_output(DP3, (DP1.x + 100, DP1.y + 2500), &outs, 207, 131),
            Some((DP3.x + 100, DP3.y + DP3.h - 131))
        );
        // Not on any monitor.
        assert_eq!(carry_to_output(DP3, (0, 0), &outs, 207, 131), None);
    }

    #[test]
    fn output_choice_labels() {
        assert_eq!(
            output_choice("DP-3", "LG ULTRAGEAR+", (3840, 2160)).label,
            "DP-3 · LG ULTRAGEAR+ (3840×2160)"
        );
        assert_eq!(
            output_choice("HDMI-A-1", "", (1920, 1080)).label,
            "HDMI-A-1 (1920×1080)"
        );
        assert_eq!(output_choice("DP-1", "x", (1, 1)).name, "DP-1");
    }

    #[test]
    fn clamp_box_keeps_preview_inside_output() {
        assert_eq!(DP3.clamp_box(6200, 300, 480, 270), (6400, 300));
        assert_eq!(
            DP3.clamp_box(10000, 2300, 480, 270),
            (10240 - 480, 234 + 2160 - 270)
        );
        assert_eq!(DP3.clamp_box(7000, 500, 480, 270), (7000, 500));
    }

    /// Live check against the running compositor: binds every protocol,
    /// finds a native EVE toplevel, shows one preview for it (top-left of
    /// the first output for a few seconds) and waits for captured frames.
    /// Run with:
    /// `cargo test --bin Nicotine live_hyprland_preview_smoke -- --ignored --nocapture`
    #[test]
    #[ignore = "needs a live Hyprland session with an EVE client running"]
    fn live_hyprland_preview_smoke() {
        struct NoWm;
        impl WindowManager for NoWm {
            fn get_eve_windows(&self) -> Result<Vec<crate::window_manager::EveWindow>> {
                Ok(Vec::new())
            }
            fn activate_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn stack_windows(
                &self,
                _: &[crate::window_manager::EveWindow],
                _: &crate::config::Config,
            ) -> Result<()> {
                Ok(())
            }
            fn get_active_window(&self) -> Result<u32> {
                Ok(0)
            }
            fn minimize_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
            fn restore_window(&self, _: u32) -> Result<()> {
                Ok(())
            }
        }
        let live = Arc::new(Mutex::new(LiveSettings {
            preview_width: 480,
            preview_height: 270,
            preview_opacity: 100,
            display_mode: DisplayMode::Previews,
            positions_locked: true,
            show_previews: true,
            hide_active_preview: false,
            preview_output: None,
            outputs: Vec::new(),
        }));
        let font = FontSettings {
            font: std::env::var("NICOTINE_TEST_FONT").ok(),
            size_px: 14.0,
            antialias: None,
        };
        let (mut queue, mut m) = Manager::connect(
            Arc::new(NoWm),
            Arc::new(Mutex::new(CycleState::new())),
            live,
            font,
            PreviewTheme::from_config(std::env::var("NICOTINE_TEST_THEME").ok().as_deref(), None),
            crate::theme::Theme::NICOTINE.panel,
        )
        .expect("bind Wayland globals");
        let qh = queue.handle();

        let toplevels: Vec<(String, String, ExtForeignToplevelHandleV1)> = m
            .toplevel_list
            .toplevels()
            .iter()
            .filter_map(|h| {
                let info = m.toplevel_list.info(h)?;
                Some((info.title, info.app_id, h.clone()))
            })
            .collect();
        for (title, app_id, _) in &toplevels {
            println!("toplevel: app_id={app_id:?} title={title:?}");
        }
        println!("active EVE character (wlr): {:?}", m.active_character);
        for t in m
            .wlr_toplevels
            .values()
            .filter(|t| t.title.starts_with("EVE - "))
        {
            println!("wlr {:?}: {} output(s) entered", t.title, t.outputs.len());
        }
        let (title, _, handle) = toplevels
            .into_iter()
            .find(|(t, _, _)| t.starts_with("EVE - "))
            .expect("no 'EVE - …' toplevel in ext-foreign-toplevel-list");
        let name = title.trim_start_matches("EVE - ").to_string();
        println!("EVE output for {name:?}: {:?}", m.eve_output(&name));

        // Bypass reconcile (NoWm has no pid-gated window list) and
        // create the preview directly.
        m.live.lock_recover().show_previews = false;
        m.create_preview(&qh, &name, handle, 480, 270)
            .expect("create preview");
        let id = *m.previews.keys().next().unwrap();

        let mut frames = 0u32;
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            m.pump_events(&mut queue).expect("wayland connection");
            let before = m.previews[&id].capture.frame.is_some();
            m.start_captures(&qh);
            m.pump_events(&mut queue).expect("wayland connection");
            let p = &m.previews[&id];
            assert!(!p.capture.stopped, "capture session stopped");
            if before && p.capture.frame.is_none() && p.capture.bufs.iter().any(|b| b.on_screen) {
                frames += 1;
            }
        }
        let p = &m.previews[&id];
        println!(
            "configured={} capture size={:?} kind={:?} frames~{}",
            p.configured, p.capture.size, p.capture.kind, frames
        );
        assert!(p.configured, "layer surface never configured");
        assert!(p.capture.size.is_some(), "no capture constraints received");
        assert!(frames > 0, "no frames captured");
        m.previews.clear();
        m.pump_events(&mut queue).unwrap();
    }

    #[test]
    fn chrome_fills_strip_border_and_well() {
        let (w, h) = (40usize, 40usize);
        let mut canvas = vec![0u8; w * h * 4];
        let t = crate::theme::Theme::NORD_DARK.preview;
        paint_chrome(
            &mut canvas,
            w,
            h,
            t.active,
            t.active_text,
            t.background,
            &TextMask::default(),
        );
        let at = |x: usize, y: usize| &canvas[(y * w + x) * 4..(y * w + x) * 4 + 4];
        let frost = [t.active.2, t.active.1, t.active.0, 0xff];
        // Title strip, left border, bottom border are chrome-colored.
        assert_eq!(at(20, 0), frost);
        assert_eq!(at(0, 30), frost);
        assert_eq!(at(20, h - 1), frost);
        // Thumbnail well is the theme background, opaque.
        let bg = [t.background.2, t.background.1, t.background.0, 0xff];
        assert_eq!(at(20, TITLE_STRIP_HEIGHT as usize + 2), bg);
    }

    #[test]
    fn chrome_text_uses_theme_text_colour() {
        let (w, h) = (40usize, 40usize);
        let mut canvas = vec![0u8; w * h * 4];
        let text = TextMask {
            width: 2,
            height: 2,
            baseline: 2,
            subpixel: false,
            data: vec![255; 4],
        };
        let t = crate::theme::Theme::NICOTINE.preview;
        paint_chrome(&mut canvas, w, h, t.inactive, t.text, t.background, &text);
        let top = (TITLE_STRIP_HEIGHT as usize - 2) / 2;
        let off = (top * w + TITLE_TEXT_LEFT_PAD as usize) * 4;
        assert_eq!(&canvas[off..off + 3], &[t.text.2, t.text.1, t.text.0]);
    }
}
