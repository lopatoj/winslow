//! Inputs on the Pico board, which reports them over a UART using
//! `winslow-protocol` (see `firmware/`).
//!
//! One [`PicoBoard`] owns the serial port and reads it on a dedicated thread,
//! fanning messages out to an event source per registered input.
//!
//! The Pico only talks while we talk to it (so that it can't disturb U-Boot
//! when wired to UART0). Every 500 ms we send it a heartbeat, or, while it
//! hasn't answered yet, a sync request, which it answers with its current
//! state.

use std::io::{self, Read, Write};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_stream::stream;
use futures_core::stream::BoxStream;
use serialport::SerialPort;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use winslow_protocol::{Decoder, MAX_INDEX, MAX_LEN, Message, VERSION};

use crate::{
    error::{Error, NewResult, Result},
    event::Event,
    source::EventSource,
};

/// The Pico sends a heartbeat every 500 ms; after this long without
/// anything, it's considered gone.
const LINK_TIMEOUT: Duration = Duration::from_secs(2);
/// How often the reader wakes up to check for shutdown and link loss.
const READ_TIMEOUT: Duration = Duration::from_millis(100);
/// How often we send a heartbeat (or a sync request while disconnected).
/// The Pico goes quiet after 2 s without one.
const PING_INTERVAL: Duration = Duration::from_millis(500);

type Slot = Option<mpsc::Sender<Result<Event>>>;
const SLOTS: usize = MAX_INDEX as usize + 1;

pub struct PicoBoard {
    path: String,
    port: Box<dyn SerialPort>,
    buttons: [Slot; SLOTS],
    encoders: [Slot; SLOTS],
    pots: [Slot; SLOTS],
    /// Events dropped since the output last had room.
    dropped: u64,
}

impl PicoBoard {
    /// Open the serial port the Pico is on, e.g. `/dev/ttyS4`.
    pub fn open(path: &str, baud: u32) -> NewResult<Self> {
        let port = serialport::new(path, baud)
            .timeout(READ_TIMEOUT)
            .open()
            .map_err(|e| format!("cannot open {path}: {e}"))?;
        Ok(Self {
            path: path.into(),
            port,
            buttons: Default::default(),
            encoders: Default::default(),
            pots: Default::default(),
            dropped: 0,
        })
    }

    /// Button `index` (0-15): [`Event::Rising`] when pressed,
    /// [`Event::Falling`] when released.
    pub fn button(&mut self, index: u8) -> NewResult<Box<PicoInput>> {
        Self::register(&mut self.buttons, "button", index)
    }

    /// Encoder `index` (0-15): [`Event::Delta`] per movement, in detents.
    pub fn encoder(&mut self, index: u8) -> NewResult<Box<PicoInput>> {
        Self::register(&mut self.encoders, "encoder", index)
    }

    /// Pot `index` (0-15): [`Event::Change`] with the position, 0.0..=1.0.
    pub fn pot(&mut self, index: u8) -> NewResult<Box<PicoInput>> {
        Self::register(&mut self.pots, "pot", index)
    }

    fn register(slots: &mut [Slot; SLOTS], kind: &str, index: u8) -> NewResult<Box<PicoInput>> {
        let slot = slots
            .get_mut(index as usize)
            .ok_or_else(|| format!("Pico {kind} {index} is out of range (0-{MAX_INDEX})"))?;
        if slot.is_some() {
            return Err(format!("Pico {kind} {index} is already registered").into());
        }
        let (tx, rx) = mpsc::channel(64);
        *slot = Some(tx);
        Ok(Box::new(PicoInput { rx: Some(rx) }))
    }

    /// Start reading on a new thread. It exits by itself once every input's
    /// event stream has been dropped, closing the port.
    pub fn spawn(self) -> io::Result<JoinHandle<()>> {
        thread::Builder::new()
            .name("pico".into())
            .spawn(move || self.run())
    }

    fn run(mut self) {
        let mut decoder = Decoder::new();
        let mut buf = [0u8; 64];
        let mut last_heard: Option<Instant> = None;
        let mut read_failing = false;
        let mut next_ping = Instant::now();
        log::info!("waiting for the Pico on {}", self.path);

        while self.any_open() {
            match self.port.read(&mut buf) {
                Ok(n) => {
                    read_failing = false;
                    for &byte in &buf[..n] {
                        if let Some(message) = decoder.push(byte) {
                            if last_heard.is_none() {
                                log::info!("Pico on {} is connected", self.path);
                                // Catch up on anything missed while the link
                                // was down (unless this is the Pico starting
                                // up, which sends its state by itself).
                                if !matches!(message, Message::Hello { .. }) {
                                    self.send(Message::Sync);
                                }
                            }
                            last_heard = Some(Instant::now());
                            self.dispatch(message);
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => {}
                Err(e) => {
                    if !read_failing {
                        log::error!("reading {} failed: {e}", self.path);
                        read_failing = true;
                    }
                    thread::sleep(READ_TIMEOUT);
                }
            }

            if last_heard.is_some_and(|t| t.elapsed() > LINK_TIMEOUT) {
                log::warn!(
                    "Pico on {} stopped responding (no heartbeat for {LINK_TIMEOUT:?})",
                    self.path
                );
                last_heard = None;
            }

            let now = Instant::now();
            if now >= next_ping {
                next_ping = now + PING_INTERVAL;
                self.send(if last_heard.is_some() {
                    Message::Heartbeat
                } else {
                    Message::Sync
                });
            }
        }
    }

    fn dispatch(&mut self, message: Message) {
        let (slots, index, event) = match message {
            Message::ButtonDown(i) => (&mut self.buttons, i, Event::Rising),
            Message::ButtonUp(i) => (&mut self.buttons, i, Event::Falling),
            Message::Encoder { index, delta } => {
                (&mut self.encoders, index, Event::Delta(delta.into()))
            }
            Message::Analog { index, value } => {
                (&mut self.pots, index, Event::Change(value as f32 / 16383.0))
            }
            Message::Hello { version } => {
                if version != VERSION {
                    log::warn!("Pico speaks protocol v{version}, expected v{VERSION}");
                }
                log::info!("Pico on {} (re)started", self.path);
                return;
            }
            Message::Heartbeat | Message::Sync => return,
        };
        let slot = &mut slots[index as usize];
        if let Some(tx) = slot {
            // Never block: this thread also keeps the link alive with
            // heartbeats. The queue only fills if the MIDI output is stuck
            // (nobody reading on the host), and then events are lost anyway.
            match tx.try_send(Ok(event)) {
                Ok(()) => self.dropped = 0,
                Err(mpsc::error::TrySendError::Full(_)) => {
                    if self.dropped == 0 {
                        log::warn!("MIDI output is backed up; dropping Pico events");
                    }
                    self.dropped += 1;
                }
                Err(mpsc::error::TrySendError::Closed(_)) => *slot = None,
            }
        }
    }

    fn send(&mut self, message: Message) {
        let mut buf = [0; MAX_LEN];
        if let Err(e) = self.port.write_all(message.encode(&mut buf)) {
            log::warn!("writing to {} failed: {e}", self.path);
        }
    }

    fn any_open(&self) -> bool {
        [&self.buttons, &self.encoders, &self.pots]
            .iter()
            .flat_map(|slots| slots.iter().flatten())
            .any(|tx| !tx.is_closed())
    }
}

pub struct PicoInput {
    rx: Option<mpsc::Receiver<Result<Event>>>,
}

impl EventSource<Event> for PicoInput {
    fn events(&mut self) -> BoxStream<'static, Result<Event>> {
        match self.rx.take() {
            Some(rx) => Box::pin(ReceiverStream::new(rx)),
            None => Box::pin(stream! { yield Err(Error {}) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;
    use std::fs::File;
    use std::os::fd::FromRawFd;
    use tokio_stream::StreamExt;

    /// A pseudo-terminal pair: (our end, as a file; the path to hand the board).
    fn pty() -> (File, String) {
        let (mut master, mut slave) = (0, 0);
        let mut name = [0 as libc::c_char; 64];
        // SAFETY: all out-pointers are valid; the fds are owned below.
        unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    name.as_mut_ptr(),
                    std::ptr::null(),
                    std::ptr::null(),
                ),
                0
            );
            // The board opens the slave by path; keep our copy closed.
            libc::close(slave);
            let path = CStr::from_ptr(name.as_ptr()).to_str().unwrap().to_owned();
            (File::from_raw_fd(master), path)
        }
    }

    fn encode(messages: &[Message]) -> Vec<u8> {
        let mut out = Vec::new();
        for &m in messages {
            let mut buf = [0; MAX_LEN];
            out.extend_from_slice(m.encode(&mut buf));
        }
        out
    }

    async fn next(events: &mut BoxStream<'static, Result<Event>>) -> Event {
        events.next().await.unwrap().ok().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn events_reach_the_registered_inputs() {
        let (mut pico, path) = pty();
        let mut board = PicoBoard::open(&path, 115200).unwrap();
        let mut button = board.button(2).unwrap().events();
        let mut encoder = board.encoder(0).unwrap().events();
        let mut pot = board.pot(1).unwrap().events();
        let thread = board.spawn().unwrap();

        // The board asks for the current state first.
        let mut byte = [0u8; 1];
        let _pico = tokio::task::spawn_blocking(move || {
            pico.read_exact(&mut byte).unwrap();
            assert_eq!(byte[0], 0xF2);
            pico.write_all(&encode(&[
                Message::Hello { version: VERSION },
                Message::ButtonDown(5), // not registered: ignored
                Message::ButtonDown(2),
                Message::Encoder {
                    index: 0,
                    delta: -3,
                },
                Message::Analog {
                    index: 1,
                    value: 16383,
                },
                Message::Heartbeat,
                Message::ButtonUp(2),
            ]))
            .unwrap();
            pico
        })
        .await
        .unwrap();

        assert_eq!(next(&mut button).await, Event::Rising);
        assert_eq!(next(&mut button).await, Event::Falling);
        assert_eq!(next(&mut encoder).await, Event::Delta(-3));
        assert_eq!(next(&mut pot).await, Event::Change(1.0));

        // Dropping every stream stops the thread (and closes the port).
        drop((button, encoder, pot));
        tokio::task::spawn_blocking(move || thread.join().unwrap())
            .await
            .unwrap();
        // So it can be opened again, e.g. on reload.
        PicoBoard::open(&path, 115200).unwrap();
    }

    #[test]
    fn keeps_asking_until_answered_then_sends_heartbeats() {
        let (mut pico, path) = pty();
        let mut board = PicoBoard::open(&path, 115200).unwrap();
        let events = board.button(0).unwrap().events();
        let thread = board.spawn().unwrap();

        let mut byte = [0u8; 1];
        // Unanswered: a sync request every 500 ms.
        for _ in 0..2 {
            pico.read_exact(&mut byte).unwrap();
            assert_eq!(byte[0], 0xF2);
        }
        // Answer; from then on, heartbeats.
        pico.write_all(&encode(&[Message::Hello { version: VERSION }]))
            .unwrap();
        let mut next = || {
            pico.read_exact(&mut byte).unwrap();
            byte[0]
        };
        // A sync may already have been on its way.
        let mut b = next();
        if b == 0xF2 {
            b = next();
        }
        assert_eq!(b, 0xF1);

        drop(events);
        thread.join().unwrap();
    }

    #[test]
    fn registration_is_checked() {
        let (_pico, path) = pty();
        let mut board = PicoBoard::open(&path, 115200).unwrap();
        assert!(board.button(3).is_ok());
        assert!(board.button(3).is_err(), "duplicate");
        assert!(board.encoder(3).is_ok(), "kinds are separate");
        assert!(board.pot(16).is_err(), "out of range");
    }
}
