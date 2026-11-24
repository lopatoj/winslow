//! MIDI output: drains a stream of MIDI messages into an ALSA rawmidi device,
//! such as the USB MIDI gadget (`hw:winslow`, after the gadget's card id).
//!
//! ALSA calls block, so the writer runs on tokio's blocking thread pool and
//! pulls the next message from the stream itself. That gives natural
//! backpressure: nothing is pulled while a write is waiting.
//!
//! The device is opened non-blocking. When its buffer is full (no host
//! reading: unplugged, or no app has the port open) a write waits up to
//! [`WRITE_TIMEOUT`] for room, then the message is dropped, so a missing host
//! can never stall the daemon indefinitely. While messages are being dropped,
//! new ones are dropped at once instead of each waiting again; otherwise the
//! inputs would back up behind a host that isn't listening.

use std::error::Error;
use std::io;
use std::time::{Duration, Instant};

use alsa::Direction;
use alsa::poll::{self, Descriptors, Flags, pollfd};
use alsa::rawmidi::Rawmidi;
use hardware::source::EventSource;
use tokio::runtime::Handle;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use wmidi::MidiMessage;

/// How long to wait for room in the device buffer before dropping a message.
const WRITE_TIMEOUT: Duration = Duration::from_millis(100);

pub type OutputError = Box<dyn Error + Send + Sync>;

/// Open `device` (an ALSA name such as `"hw:winslow"`) and spawn a task that
/// writes every message from `source` to it.
///
/// Opening errors are returned immediately. The task finishes with `Ok(())`
/// when the stream ends, or with an error if the device fails (e.g. the USB
/// gadget was unbound), so the caller can decide whether to reopen.
///
/// Must be called from within a tokio runtime.
pub fn output(
    mut source: Box<dyn EventSource<MidiMessage<'static>>>,
    device: &str,
) -> Result<JoinHandle<Result<(), OutputError>>, OutputError> {
    let mut writer = Writer::open(device)?;
    // The stream is 'static, so it owns everything it needs; `source` itself
    // can be dropped here.
    let mut events = source.events();
    let runtime = Handle::try_current()?;
    log::info!("MIDI output opened on {device}");

    Ok(tokio::task::spawn_blocking(move || {
        // Blocking on the next item is fine here: this is a blocking-pool
        // thread, and the runtime keeps driving the sources' timers elsewhere.
        while let Some(item) = runtime.block_on(events.next()) {
            match item {
                Ok(message) => writer.send(&message)?,
                // hardware::error::Error carries no details yet.
                Err(_) => log::warn!("event source reported an error; skipping"),
            }
        }
        log::info!("event stream ended, closing {}", writer.device);
        Ok(())
    }))
}

struct Writer {
    device: String,
    midi: Rawmidi,
    /// Poll descriptors for waiting until the device can accept more bytes.
    fds: Vec<pollfd>,
    /// Reused serialization buffer.
    buf: Vec<u8>,
    dropped: u64,
}

enum WriteError {
    /// The buffer stayed full for the whole timeout.
    Timeout,
    /// The device failed; it needs to be reopened.
    Fatal(OutputError),
}

impl Writer {
    fn open(device: &str) -> Result<Self, OutputError> {
        let midi = Rawmidi::new(device, Direction::Playback, true)
            .map_err(|e| format!("cannot open MIDI output {device}: {e}"))?;
        let fds = midi.get()?;
        Ok(Self {
            device: device.to_owned(),
            midi,
            fds,
            buf: Vec::with_capacity(3),
            dropped: 0,
        })
    }

    /// Serialize and write one message. Only device failures are errors; a
    /// message dropped because nobody is reading is logged and skipped.
    fn send(&mut self, message: &MidiMessage) -> Result<(), OutputError> {
        self.buf.resize(message.bytes_size(), 0);
        message.copy_to_slice(&mut self.buf)?;

        match self.write_all() {
            Ok(()) => {
                if self.dropped > 0 {
                    log::info!(
                        "MIDI output flowing again after dropping {} message(s)",
                        self.dropped
                    );
                    self.dropped = 0;
                }
                log::trace!("sent {message:?}");
                Ok(())
            }
            Err(WriteError::Timeout) => {
                self.dropped += 1;
                if self.dropped == 1 {
                    log::warn!(
                        "MIDI output buffer full for {WRITE_TIMEOUT:?} (is the host reading?); \
                         dropping messages"
                    );
                }
                Ok(())
            }
            Err(WriteError::Fatal(e)) => {
                Err(format!("MIDI output {} failed: {e}", self.device).into())
            }
        }
    }

    /// Write the whole buffer, waiting for room when the device is full.
    ///
    /// If the timeout hits partway through a message, its tail is lost. The
    /// receiver resynchronizes at the next status byte, so this only garbles
    /// that one message.
    fn write_all(&mut self) -> Result<(), WriteError> {
        // Already dropping: only take what fits right now.
        let timeout = if self.dropped > 0 {
            Duration::ZERO
        } else {
            WRITE_TIMEOUT
        };
        let deadline = Instant::now() + timeout;
        let mut sent = 0;

        while sent < self.buf.len() {
            match self.midi.write(&self.buf[sent..]) {
                Ok(written) => sent += written,
                Err(e) if is_would_block(&e) => self.wait_writable(deadline)?,
                Err(e) => return Err(WriteError::Fatal(e.into())),
            }
        }
        Ok(())
    }

    /// Block until the device can accept bytes or `deadline` passes.
    fn wait_writable(&mut self, deadline: Instant) -> Result<(), WriteError> {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(WriteError::Timeout);
        }
        // Round up so a sub-millisecond remainder still waits instead of spinning.
        let timeout_ms = i32::try_from(left.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);

        let ready =
            poll::poll(&mut self.fds, timeout_ms).map_err(|e| WriteError::Fatal(e.into()))?;
        if ready == 0 {
            return Err(WriteError::Timeout);
        }
        let flags = self
            .midi
            .revents(&self.fds)
            .map_err(|e| WriteError::Fatal(e.into()))?;
        if flags.intersects(Flags::ERR | Flags::HUP | Flags::NVAL) {
            return Err(WriteError::Fatal(
                format!("device reported {flags:?} (disconnected?)").into(),
            ));
        }
        Ok(())
    }
}

/// ALSA reports errors as positive errno values; EAGAIN means "buffer full".
fn is_would_block(e: &alsa::Error) -> bool {
    io::Error::from_raw_os_error(e.errno()).kind() == io::ErrorKind::WouldBlock
}
