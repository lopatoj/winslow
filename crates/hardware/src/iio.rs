//! Analog inputs read through IIO (e.g. the AM335x ADC).
//!
//! One [`IioDevice`] samples all of its registered channels on a dedicated
//! thread: sysfs reads block, and libiio's `Channel` can't be sent between
//! threads, so it doesn't fit on the async runtime. Each channel gets its own
//! [`IioChannel`] event source. Readings are smoothed (the ADC jitters by a
//! count or two even with the pot untouched) and only real changes are sent,
//! normalized to 0.0..=1.0.

use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use async_stream::stream;
use futures_core::stream::BoxStream;
use industrial_io::{Device, Direction, Error as IioError};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::ReceiverStream;
use winslow_scan::Smoother;

use crate::{
    error::{Error, NewResult, Result},
    event::Event,
    source::EventSource,
};

impl From<IioError> for Error {
    fn from(_error: IioError) -> Self {
        Self {}
    }
}

struct InnerChannel {
    id: String,
    tx: mpsc::Sender<Result<Event>>,
}

pub struct IioDevice {
    device: Device,
    period: Duration,
    /// Raw reading that maps to 1.0 (4095 for a 12-bit ADC).
    full_scale: u16,
    /// Smallest change in raw counts that is reported.
    deadband: u16,
    channels: Vec<InnerChannel>,
}

impl IioDevice {
    pub fn new(device: Device, period: Duration, full_scale: u16, deadband: u16) -> Self {
        Self {
            device,
            period,
            full_scale: full_scale.max(1),
            deadband,
            channels: vec![],
        }
    }

    /// Register an input channel by id (e.g. `voltage0`). Fails right away if
    /// the device has no such channel.
    pub fn channel(&mut self, id: &str) -> NewResult<Box<IioChannel>> {
        if self.device.find_channel(id, Direction::Input).is_none() {
            let name = self.device.name().unwrap_or_default();
            return Err(format!("IIO device \"{name}\" has no input channel \"{id}\"").into());
        }
        let (tx, rx) = mpsc::channel(32);
        self.channels.push(InnerChannel { id: id.into(), tx });
        Ok(Box::new(IioChannel { rx: Some(rx) }))
    }

    /// Start sampling on a new thread. It exits by itself once every
    /// channel's event stream has been dropped.
    pub fn spawn(self) -> std::io::Result<JoinHandle<()>> {
        let name = self.device.name().unwrap_or_else(|| "iio".into());
        thread::Builder::new()
            .name(format!("iio {name}"))
            .spawn(move || self.run())
    }

    fn run(self) {
        struct Running {
            channel: industrial_io::Channel,
            tx: mpsc::Sender<Result<Event>>,
            smoother: Smoother,
            /// A value the consumer had no room for yet; retried next tick.
            pending: Option<f32>,
            /// Report a failing read once, not on every sample.
            failing: bool,
        }

        let mut running: Vec<Running> = self
            .channels
            .into_iter()
            .filter_map(|c| {
                // Checked in `channel()`, so this only fails if the device
                // vanished in between.
                let channel = self.device.find_channel(&c.id, Direction::Input);
                if channel.is_none() {
                    let _ = c.tx.try_send(Err(Error {}));
                }
                Some(Running {
                    channel: channel?,
                    tx: c.tx,
                    smoother: Smoother::new(self.full_scale, self.deadband),
                    pending: None,
                    failing: false,
                })
            })
            .collect();

        let mut next = Instant::now();
        while !running.is_empty() {
            running.retain_mut(|c| {
                if c.tx.is_closed() {
                    return false;
                }
                match c.channel.attr_read_int("raw") {
                    Ok(raw) => {
                        c.failing = false;
                        let raw = raw.clamp(0, u16::MAX as i64) as u16;
                        if let Some(v) = c.smoother.update(raw) {
                            c.pending = Some(v as f32 / self.full_scale as f32);
                        }
                    }
                    Err(_) if c.failing => {}
                    Err(e) => {
                        c.failing = true;
                        if let Err(TrySendError::Closed(_)) = c.tx.try_send(Err(e.into())) {
                            return false;
                        }
                    }
                }
                if let Some(value) = c.pending {
                    match c.tx.try_send(Ok(Event::Change(value))) {
                        Ok(()) => c.pending = None,
                        // Consumer is behind: keep it (or a newer value) for
                        // the next tick.
                        Err(TrySendError::Full(_)) => {}
                        Err(TrySendError::Closed(_)) => return false,
                    }
                }
                true
            });

            // Fixed rate without drift; if we fell behind, don't try to catch up.
            next += self.period;
            let now = Instant::now();
            if next > now {
                thread::sleep(next - now);
            } else {
                next = now;
            }
        }
    }
}

pub struct IioChannel {
    rx: Option<mpsc::Receiver<Result<Event>>>,
}

impl EventSource<Event> for IioChannel {
    fn events(&mut self) -> BoxStream<'static, Result<Event>> {
        match self.rx.take() {
            Some(rx) => Box::pin(ReceiverStream::new(rx)),
            None => Box::pin(stream! { yield Err(Error {}) }),
        }
    }
}
