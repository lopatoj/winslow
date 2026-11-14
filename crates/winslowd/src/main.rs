use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures_core::stream::BoxStream;
use hardware::source::EventSource;
use systemd::JournalLog;
use systemd::daemon::{self, STATE_READY, STATE_RELOADING, STATE_STATUS, STATE_STOPPING};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use wmidi::MidiMessage;

use crate::config::Config;
use crate::output::{OutputError, output};

mod adapter;
mod collector;
mod config;
mod gadget;
mod output;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // systemd sets JOURNAL_STREAM for services; run by hand, log to stderr.
    if std::env::var_os("JOURNAL_STREAM").is_some() {
        JournalLog::init()?;
    } else {
        log::set_logger(&StderrLog)?;
    }
    log::set_max_level(log::LevelFilter::Info);

    let path = Config::path(std::env::args_os().nth(1).map(PathBuf::from));
    let mut config = Config::load(&path)?;

    // Created once: recreating it on reload would make the host re-enumerate.
    // Dropping it removes the gadget, so it lives until main returns.
    let _gadget = if config.midi.usb_gadget {
        let gadget = gadget::setup_gadget().map_err(|e| format!("USB gadget setup failed: {e}"))?;
        wait_for_card(&config.midi.device, Duration::from_secs(2));
        Some(gadget)
    } else {
        None
    };

    let mut running = Pipeline::start(&config)?;
    notify(&[(STATE_READY, "1"), (STATE_STATUS, &running.status)]);

    // `systemctl reload` sends SIGHUP (ExecReload in the unit), `stop` SIGTERM.
    let mut hangup = signal(SignalKind::hangup())?;
    let mut terminate = signal(SignalKind::terminate())?;

    loop {
        tokio::select! {
            _ = hangup.recv() => {
                notify(&[(STATE_RELOADING, "1"), (STATE_STATUS, "reloading")]);
                match Config::load(&path) {
                    Ok(new) => {
                        running.stop().await;
                        running = match Pipeline::start(&new) {
                            Ok(p) => { config = new; p }
                            Err(e) => {
                                log::error!("new config failed, restoring previous: {e}");
                                Pipeline::start(&config)?
                            }
                        };
                    }
                    // Nothing was touched; the current setup keeps running.
                    Err(e) => log::error!("reload rejected: {e}"),
                }
                notify(&[(STATE_READY, "1"), (STATE_STATUS, &running.status)]);
            }
            _ = terminate.recv() => break,
            _ = tokio::signal::ctrl_c() => break,
            // The output only ends by itself if the device failed. Exit with an
            // error and let systemd's Restart= bring us back.
            result = &mut running.task => {
                let e: Box<dyn Error> = match result {
                    Ok(Ok(())) => "MIDI output ended unexpectedly".into(),
                    Ok(Err(e)) => e,
                    Err(e) => e.into(),
                };
                return Err(e);
            }
        }
    }

    notify(&[(STATE_STOPPING, "1")]);
    running.stop().await;
    Ok(())
}

/// Best effort: outside systemd there's no one to notify, which is fine.
fn notify(state: &[(&str, &str)]) {
    if let Err(e) = daemon::notify(state.iter()) {
        log::warn!("sd_notify failed: {e}");
    }
}

/// The gadget's sound card is created when it binds; give it a moment to
/// appear before the output tries to open it. `device` is like `hw:winslow`.
fn wait_for_card(device: &str, timeout: Duration) {
    let id = device.trim_start_matches("hw:");
    let id = id.split(',').next().unwrap_or(id);
    let card = Path::new("/proc/asound").join(id);
    let deadline = Instant::now() + timeout;
    while !card.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Minimal logger for running outside systemd.
struct StderrLog;

impl log::Log for StderrLog {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= log::max_level()
    }

    fn log(&self, record: &log::Record) {
        if self.enabled(record.metadata()) {
            eprintln!("{:<5} {}", record.level(), record.args());
        }
    }

    fn flush(&self) {}
}

/// Everything built from one config: the inputs and the task writing them out.
struct Pipeline {
    stop: oneshot::Sender<()>,
    task: JoinHandle<Result<(), OutputError>>,
    status: String,
}

impl Pipeline {
    fn start(config: &Config) -> Result<Self, Box<dyn Error>> {
        let io = config.build()?;
        let (stop, stopped) = oneshot::channel();
        let source = Stoppable {
            inner: Box::new(io.collector),
            stopped: Some(stopped),
        };
        let task =
            output(Box::new(source), &config.midi.device).map_err(|e| -> Box<dyn Error> { e })?;
        // Sampler threads exit by themselves when the stream is dropped.
        for device in io.iio_devices {
            device.spawn()?;
        }
        let status = format!(
            "{} input(s) on {} channel {}",
            config.inputs.len(),
            config.midi.device,
            config.midi.channel
        );
        log::info!("running: {status}");
        Ok(Self { stop, task, status })
    }

    /// End the stream, wait for the output to close the device (so it can be
    /// reopened), then release the inputs.
    async fn stop(self) {
        let _ = self.stop.send(());
        match self.task.await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => log::warn!("MIDI output had failed: {e}"),
            Err(e) => log::warn!("MIDI output task panicked: {e}"),
        }
    }
}

/// Wraps a source so its stream ends when `stopped` fires (or its sender is
/// dropped), even if the inner sources would run forever.
struct Stoppable {
    inner: Box<dyn EventSource<MidiMessage<'static>> + Send>,
    stopped: Option<oneshot::Receiver<()>>,
}

impl EventSource<MidiMessage<'static>> for Stoppable {
    fn events(&mut self) -> BoxStream<'static, hardware::error::Result<MidiMessage<'static>>> {
        let mut inner = self.inner.events();
        let Some(mut stopped) = self.stopped.take() else {
            return Box::pin(tokio_stream::empty());
        };
        Box::pin(async_stream::stream! {
            loop {
                tokio::select! {
                    biased;
                    _ = &mut stopped => break,
                    item = inner.next() => match item {
                        Some(item) => yield item,
                        None => break,
                    },
                }
            }
        })
    }
}
