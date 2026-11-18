//! Background audio worker for the logo easter-egg jingle.
//!
//! The OS audio handle (`MixerDeviceSink`) is `!Send`, so it lives entirely
//! on this worker thread; the rest of the app only holds the channel `Sender`.
//!
//! The output device is opened only while the jingle plays and closed once
//! it ends. Holding it open for the whole session would keep a live stream
//! running against a device that can disappear (headset unplugged, sound
//! server restarted). When that happens cpal reports an error on every poll,
//! and Nicotine used to print each one, writing gigabytes of identical lines
//! to its log.

use std::io::Cursor;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::Duration;

/// The jingle, embedded so it ships with the binary (like the fonts/icon).
const JINGLE: &[u8] = include_bytes!("../assets/nicotinecountry.mp3");

/// How often the worker checks, while playing, whether the jingle ended or
/// the stream failed.
const POLL: Duration = Duration::from_millis(250);

/// An open output device playing the jingle.
struct Playback {
    // Field order matters: the player must drop before the device it
    // plays through.
    player: rodio::Player,
    _sink: rodio::MixerDeviceSink,
    failed: Arc<AtomicBool>,
}

impl Playback {
    fn start() -> Result<Self, String> {
        let failed = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&failed);
        let on_error = move |e: rodio::cpal::StreamError| {
            // cpal can report the same error on every poll once a device
            // goes bad; log the first and let the worker close the stream.
            if !flag.swap(true, Ordering::Relaxed) {
                eprintln!("audio: stream error ({e}); closing the output device");
            }
        };
        let mut sink = open_sink(on_error)?;
        // Closing the device after each play is intended, not worth a log line.
        sink.log_on_drop(false);
        let player = rodio::play(sink.mixer(), Cursor::new(JINGLE)).map_err(|e| e.to_string())?;
        Ok(Self {
            player,
            _sink: sink,
            failed,
        })
    }

    fn done(&self) -> bool {
        self.player.empty() || self.failed.load(Ordering::Relaxed)
    }
}

/// Open the default output device, falling back to the first other device
/// that works, like `rodio::DeviceSinkBuilder::open_default_sink` (which
/// can't take a custom error callback). The fallback matters: on some
/// systems the ALSA default is a dmix device that can't open while
/// PipeWire holds the hardware, but the `pipewire` device can.
fn open_sink<E>(on_error: E) -> Result<rodio::MixerDeviceSink, String>
where
    E: FnMut(rodio::cpal::StreamError) + Clone + Send + 'static,
{
    use rodio::cpal::traits::{DeviceTrait, HostTrait};
    use rodio::DeviceSinkBuilder;

    let first_err = match DeviceSinkBuilder::from_default_device()
        .and_then(|b| b.with_error_callback(on_error.clone()).open_stream())
    {
        Ok(sink) => return Ok(sink),
        Err(e) => e.to_string(),
    };
    rodio::cpal::default_host()
        .output_devices()
        .map_err(|_| first_err.clone())?
        .filter(|dev| {
            dev.description()
                .is_ok_and(|d| d.driver().is_some_and(|driver| driver != "null"))
        })
        .find_map(|dev| {
            DeviceSinkBuilder::from_device(dev)
                .ok()?
                .with_error_callback(on_error.clone())
                .open_sink_or_fallback()
                .ok()
        })
        .ok_or(first_err)
}

/// Spawn the audio worker. Returns a sender — send `()` to play the jingle
/// from the start, restarting it if it's already playing.
///
/// Best-effort: if no output device opens (or decoding fails) the request
/// is dropped without affecting the rest of the app, and the next one tries
/// again.
pub fn spawn() -> Sender<()> {
    let (tx, rx) = channel::<()>();
    std::thread::spawn(move || {
        let mut current: Option<Playback> = None;
        loop {
            let request = if current.is_some() {
                rx.recv_timeout(POLL)
            } else {
                rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
            };
            match request {
                Ok(()) => {
                    // Close the previous stream first, so a restart doesn't
                    // briefly hold two devices open.
                    current = None;
                    match Playback::start() {
                        Ok(playback) => current = Some(playback),
                        Err(e) => eprintln!("audio: couldn't play the logo jingle ({e})"),
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    if current.as_ref().is_some_and(Playback::done) {
                        current = None;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    tx
}
