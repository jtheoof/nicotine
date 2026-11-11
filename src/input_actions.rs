//! Work the evdev input listeners hand off so they never stall reading.
//!
//! Cycling or switching clients goes through the window manager, which on
//! Wayland compositors means spawning `hyprctl`/`swaymsg` (tens of ms per
//! call). Doing that on a listener thread lets fast devices (a 1 kHz mouse
//! node, which also advertises keyboard keys and LEDs) overflow their
//! kernel buffers while the thread is blocked, producing `SYN_DROPPED`.
//! evdev 0.12.2 then looped forever replaying an LED/switch change after
//! the resync, and an unbounded `collect()` turned that into a runaway
//! allocation (tens of GB in minutes). So:
//!
//! - listeners push [`InputAction`]s to an [`ActionQueue`] whose worker
//!   thread does the window-manager work, and
//! - they read events with [`fetch_bounded`], which caps one wakeup's
//!   batch (anything left stays buffered for the next poll).

use crate::cycle_state::CycleState;
use crate::window_manager::WindowManager;
use anyhow::Result;
use evdev::{Device, InputEvent};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

/// Most events taken from one device per wakeup. A real device delivers a
/// few dozen per wakeup; anything beyond this waits for the next poll
/// instead of growing a buffer without bound.
pub const MAX_EVENTS_PER_READ: usize = 1024;

/// Pending actions the worker hasn't started yet. Small on purpose: if a
/// held key auto-repeats faster than the window manager can switch
/// clients, extra steps are dropped rather than replayed late.
const QUEUE_DEPTH: usize = 4;

/// Read up to [`MAX_EVENTS_PER_READ`] events from `device`.
pub fn fetch_bounded(device: &mut Device) -> std::io::Result<Vec<InputEvent>> {
    Ok(device.fetch_events()?.take(MAX_EVENTS_PER_READ).collect())
}

/// Something an input binding asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputAction {
    Forward {
        minimize_inactive: bool,
    },
    Backward {
        minimize_inactive: bool,
    },
    SwitchTo {
        name: String,
        minimize_inactive: bool,
    },
}

/// Hands [`InputAction`]s to a worker thread that runs them in order.
pub struct ActionQueue {
    tx: SyncSender<InputAction>,
}

impl ActionQueue {
    /// Start the worker. `label` names the thread and log lines.
    pub fn spawn(
        label: &'static str,
        wm: Arc<dyn WindowManager>,
        state: Arc<Mutex<CycleState>>,
    ) -> Self {
        let (tx, rx) = sync_channel(QUEUE_DEPTH);
        std::thread::Builder::new()
            .name(format!("{label}-actions"))
            .spawn(move || run_worker(label, rx, wm, state))
            .expect("spawn input action worker");
        Self { tx }
    }

    /// Queue `action`; returns false if it was dropped because the worker
    /// is still busy with earlier ones.
    pub fn push(&self, action: InputAction) -> bool {
        match self.tx.try_send(action) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => false,
            // Worker gone (only on panic); nothing useful to do.
            Err(TrySendError::Disconnected(_)) => false,
        }
    }
}

fn run_worker(
    label: &str,
    rx: Receiver<InputAction>,
    wm: Arc<dyn WindowManager>,
    state: Arc<Mutex<CycleState>>,
) {
    for action in rx {
        if let Err(e) = run_action(&*wm, &state, &action) {
            eprintln!("{label}: {action:?} failed: {e:#}");
        }
    }
}

/// Lock → sync with the compositor's active window → act.
///
/// For cycling, the `get_active_window` round-trip is skipped inside the
/// activation grace window, where `sync_with_active` would no-op anyway,
/// so it's pure latency on a fast burst. Direct switches always sync.
pub fn run_action(
    wm: &dyn WindowManager,
    state: &Mutex<CycleState>,
    action: &InputAction,
) -> Result<()> {
    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
    let is_switch = matches!(action, InputAction::SwitchTo { .. });
    if is_switch || !state.in_activation_grace() {
        if let Ok(active) = wm.get_active_window() {
            state.sync_with_active(active);
        }
    }
    match action {
        InputAction::Forward { minimize_inactive } => state.cycle_forward(wm, *minimize_inactive),
        InputAction::Backward { minimize_inactive } => state.cycle_backward(wm, *minimize_inactive),
        InputAction::SwitchTo {
            name,
            minimize_inactive,
        } => state.switch_to_character(name, wm, *minimize_inactive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::window_manager::EveWindow;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Records activations; optionally slow, like a hyprctl round-trip.
    struct RecordingWm {
        activated: Mutex<Vec<u32>>,
        delay: Duration,
        calls: AtomicUsize,
    }

    impl WindowManager for RecordingWm {
        fn get_eve_windows(&self) -> Result<Vec<EveWindow>> {
            Ok(Vec::new())
        }
        fn activate_window(&self, id: u32) -> Result<()> {
            std::thread::sleep(self.delay);
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.activated.lock().unwrap().push(id);
            Ok(())
        }
        fn stack_windows(&self, _: &[EveWindow], _: &crate::config::Config) -> Result<()> {
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

    fn three_clients() -> Arc<Mutex<CycleState>> {
        let state = Arc::new(Mutex::new(CycleState::new()));
        state.lock().unwrap().update_windows(
            (1..=3)
                .map(|id| EveWindow {
                    id,
                    title: format!("C{id}"),
                })
                .collect(),
        );
        state
    }

    fn wm(delay: Duration) -> Arc<RecordingWm> {
        Arc::new(RecordingWm {
            activated: Mutex::new(Vec::new()),
            delay,
            calls: AtomicUsize::new(0),
        })
    }

    fn wait_for(calls: &AtomicUsize, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while calls.load(Ordering::SeqCst) < n && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn worker_runs_actions_in_order() {
        let state = three_clients();
        let rec = wm(Duration::ZERO);
        let q = ActionQueue::spawn("test", rec.clone(), Arc::clone(&state));
        let m = false;
        assert!(q.push(InputAction::Forward {
            minimize_inactive: m
        }));
        assert!(q.push(InputAction::SwitchTo {
            name: "C3".into(),
            minimize_inactive: m
        }));
        wait_for(&rec.calls, 2);
        // Forward from C1 → C2, then a direct jump to C3.
        assert_eq!(*rec.activated.lock().unwrap(), vec![2, 3]);
    }

    #[test]
    fn push_never_blocks_and_drops_when_the_worker_is_busy() {
        let rec = wm(Duration::from_millis(200));
        let q = ActionQueue::spawn("test", rec.clone(), three_clients());
        let started = Instant::now();
        let accepted = (0..50)
            .filter(|_| {
                q.push(InputAction::Forward {
                    minimize_inactive: false,
                })
            })
            .count();
        // The listener thread is never held up by slow switching...
        assert!(started.elapsed() < Duration::from_millis(100));
        // ...and a flood (key auto-repeat) is bounded, not queued forever.
        assert!(
            accepted <= QUEUE_DEPTH + 1,
            "accepted {accepted} of 50 while busy"
        );
        assert!(accepted >= 1);
    }

    /// Regression for the runaway allocation: with evdev 0.12.2, a switch
    /// (or LED) change followed by a buffer overflow made the synced
    /// `fetch_events()` iterator replay that change forever. Builds a
    /// throwaway uinput device with two obscure switches (ignored by
    /// compositors) and MSC_SCAN events; sends no keys.
    /// Needs write access to /dev/uinput (e.g. the `input` group):
    /// `cargo test --bin Nicotine fetch_after_dropped_events_terminates -- --ignored`
    #[test]
    #[ignore = "needs /dev/uinput"]
    fn fetch_after_dropped_events_terminates() {
        use evdev::uinput::VirtualDevice;
        use evdev::{AttributeSet, EventType, MiscCode, SwitchCode};

        let mut switches = AttributeSet::<SwitchCode>::new();
        switches.insert(SwitchCode::SW_JACK_PHYSICAL_INSERT); // 7
        switches.insert(SwitchCode::SW_PEN_INSERTED); // 15: relative index 7 from 8
        let mut msc = AttributeSet::<MiscCode>::new();
        msc.insert(MiscCode::MSC_SCAN);
        let mut vdev = VirtualDevice::builder()
            .unwrap()
            .name("nicotine-evdev-regression")
            .with_switches(&switches)
            .unwrap()
            .with_msc(&msc)
            .unwrap()
            .build()
            .unwrap();
        let path = vdev
            .enumerate_dev_nodes_blocking()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        std::thread::sleep(Duration::from_millis(200));
        let mut dev = Device::open(&path).unwrap();

        vdev.emit(&[InputEvent::new(
            EventType::SWITCH.0,
            SwitchCode::SW_JACK_PHYSICAL_INSERT.0,
            1,
        )])
        .unwrap();
        // Overflow the per-client kernel buffer without reading.
        for i in 0..4000 {
            vdev.emit(&[InputEvent::new(EventType::MISC.0, MiscCode::MSC_SCAN.0, i)])
                .unwrap();
        }
        std::thread::sleep(Duration::from_millis(100));

        // Count with a hard cap so a regression fails instead of OOMing.
        // The first fetch sees SYN_DROPPED and stops; the next resyncs
        // (where 0.12.2 looped). A synced fetch blocks on an empty kernel
        // buffer, so give each later round one event to read.
        let mut total = 0;
        for round in 0..3 {
            if round > 0 {
                vdev.emit(&[InputEvent::new(EventType::MISC.0, MiscCode::MSC_SCAN.0, 1)])
                    .unwrap();
            }
            let n = dev.fetch_events().unwrap().take(1_000_000).count();
            assert!(n < 1_000_000, "fetch_events() never terminated");
            total += n;
            if n > 0 {
                break;
            }
        }
        // The switch change is reported once after the resync.
        assert!(total > 0 && total < 100, "unexpected {total} events");
    }
}
