//! Daemon configuration: which inputs exist and where they go in MIDI.
//!
//! The config is a TOML file. Under systemd, `ConfigurationDirectory=winslow`
//! in the unit makes systemd create `/etc/winslow` and pass it to us as
//! `$CONFIGURATION_DIRECTORY`, so the file is found there without hard-coding
//! paths. Run by hand, pass a path as the first argument instead.
//!
//! Loading is split in two so a bad file can be rejected before any hardware
//! is touched (important on reload, where the old setup keeps running if the
//! new file is invalid):
//! 1. [`Config::load`] parses and validates the file.
//! 2. [`Config::build`] opens the hardware and registers every input.
//!
//! ```toml
//! [midi]
//! device = "hw:winslow"   # ALSA rawmidi device
//! channel = 1             # 1-16
//! usb_gadget = true       # create the USB MIDI gadget at startup
//!
//! [adc]
//! poll_ms = 5
//! full_scale = 4095       # raw reading at the top of the pot's travel
//!
//! [[input]]
//! type = "pot"
//! channel = "voltage0"    # AIN0
//! control = 16            # optional: 0-31, else the next free one
//!
//! [[input]]
//! type = "button"
//! chip = "gpiochip0"
//! line = 23
//! note = 60               # optional: else the next free one from 36
//! ```

use std::collections::BTreeMap;
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hardware::event::Event;
use hardware::gpio::CDevGpio;
use hardware::iio::IioDevice;
use industrial_io::Context;
use serde::Deserialize;
use wmidi::{Channel, ControlFunction, Note, U7};

use crate::collector::{MidiCollector, MidiType};

pub type ConfigError = Box<dyn Error>;

/// File name inside the configuration directory.
const FILE_NAME: &str = "winslow.toml";
/// Used when not started by systemd and no path is given.
const FALLBACK_DIR: &str = "/etc/winslow";
/// The AM335x ADC's IIO device name on the PocketBeagle.
const DEFAULT_ADC: &str = "TI-am335x-adc.0.auto";

#[derive(Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub midi: MidiConfig,
    #[serde(default)]
    pub adc: AdcConfig,
    #[serde(default, rename = "input")]
    pub inputs: Vec<Input>,
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MidiConfig {
    /// ALSA rawmidi device; `hw:winslow` is the USB gadget (its `midi.id`).
    pub device: String,
    /// MIDI channel, 1-16 as printed on gear (sent as 0-15).
    pub channel: u8,
    /// Create the USB MIDI gadget (card id `winslow`) at startup. Turn off
    /// on machines without a USB device controller, e.g. a laptop with a
    /// virtual MIDI card. Only read at startup, not on reload.
    pub usb_gadget: bool,
}

impl Default for MidiConfig {
    fn default() -> Self {
        Self {
            device: "hw:winslow".into(),
            channel: 1,
            usb_gadget: true,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AdcConfig {
    /// How often each IIO device is sampled.
    pub poll_ms: u64,
    /// Raw reading that maps to the top of the range (4095 for 12 bits).
    pub full_scale: u32,
}

impl Default for AdcConfig {
    fn default() -> Self {
        Self {
            poll_ms: 5,
            full_scale: 4095,
        }
    }
}

#[derive(Debug, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Input {
    /// An analog input read through IIO, sent as 14-bit CC.
    Pot {
        /// Label for log messages.
        name: Option<String>,
        /// IIO device name.
        #[serde(default = "default_adc")]
        device: String,
        /// IIO channel id, e.g. `voltage0` for AIN0.
        channel: String,
        /// Controller 0-31 (the LSB goes on +32); next free one if omitted.
        control: Option<u8>,
    },
    /// A GPIO line, sent as note on (press) / note off (release).
    Button {
        name: Option<String>,
        /// `gpiochip0` or a full `/dev/gpiochip0` path.
        chip: String,
        line: u32,
        /// Note 0-127; next free one from 36 if omitted.
        note: Option<u8>,
    },
}

fn default_adc() -> String {
    DEFAULT_ADC.into()
}

impl Input {
    fn label(&self, index: usize) -> String {
        let (name, kind) = match self {
            Input::Pot { name, .. } => (name, "pot"),
            Input::Button { name, .. } => (name, "button"),
        };
        match name {
            Some(name) => format!("input {} ({kind} \"{name}\")", index + 1),
            None => format!("input {} ({kind})", index + 1),
        }
    }

    fn position(&self) -> Result<MidiType, String> {
        Ok(match *self {
            Input::Pot { control: None, .. } => MidiType::NextControl,
            Input::Pot {
                control: Some(c), ..
            } => {
                let c = U7::new(c).map_err(|_| format!("control {c} is out of range"))?;
                MidiType::Control(ControlFunction(c))
            }
            Input::Button { note: None, .. } => MidiType::NextNote,
            Input::Button { note: Some(n), .. } => MidiType::Note(
                Note::try_from(n).map_err(|_| format!("note {n} is out of range (0-127)"))?,
            ),
        })
    }
}

/// The hardware opened from a config, ready to hand to the output.
pub struct Io {
    pub collector: MidiCollector<Event>,
    /// One per distinct IIO device; [`IioDevice::spawn`] each to start
    /// sampling its pots.
    pub iio_devices: Vec<IioDevice>,
}

impl Config {
    /// Where to read the config from: an explicit path, else systemd's
    /// configuration directory, else `/etc/winslow`.
    pub fn path(explicit: Option<PathBuf>) -> PathBuf {
        explicit.unwrap_or_else(|| {
            let dir = std::env::var_os("CONFIGURATION_DIRECTORY")
                // systemd passes a colon-separated list if several are set.
                .and_then(|dirs| {
                    dirs.to_str()
                        .and_then(|d| d.split(':').next())
                        .map(PathBuf::from)
                })
                .unwrap_or_else(|| FALLBACK_DIR.into());
            dir.join(FILE_NAME)
        })
    }

    /// Read, parse and validate a config file without touching hardware.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    pub fn parse(text: &str) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// Catch everything that can be checked without hardware, including MIDI
    /// position clashes, by doing a dry-run registration.
    fn validate(&self) -> Result<(), ConfigError> {
        self.channel()?;
        if self.adc.poll_ms == 0 {
            return Err("adc.poll_ms must be at least 1".into());
        }
        if self.adc.full_scale == 0 {
            return Err("adc.full_scale must be at least 1".into());
        }
        if self.inputs.is_empty() {
            return Err("no inputs configured (add an [[input]] section)".into());
        }
        let mut dry_run = MidiCollector::<Event>::new(Channel::Ch1);
        for (i, input) in self.inputs.iter().enumerate() {
            let position = input
                .position()
                .map_err(|e| format!("{}: {e}", input.label(i)))?;
            dry_run
                .register(Box::new(Unused), position)
                .map_err(|e| format!("{}: {e}", input.label(i)))?;
        }
        Ok(())
    }

    pub fn channel(&self) -> Result<Channel, ConfigError> {
        match self.midi.channel {
            c @ 1..=16 => Ok(Channel::from_index(c - 1).expect("0-15 is a valid channel")),
            c => Err(format!("midi.channel {c} is out of range (1-16)").into()),
        }
    }

    /// Open every configured input and register it with a new collector.
    pub fn build(&self) -> Result<Io, ConfigError> {
        let mut collector = MidiCollector::new(self.channel()?);
        // Only opened if there are pots, so button-only setups don't need IIO.
        let mut ctx: Option<Context> = None;
        // Pots on the same IIO device share one poller.
        let mut iio: BTreeMap<&str, IioDevice> = BTreeMap::new();

        for (i, input) in self.inputs.iter().enumerate() {
            let label = input.label(i);
            let in_context = |e: &dyn std::fmt::Display| format!("{label}: {e}");

            let source: Box<dyn hardware::source::EventSource<Event> + Send> = match input {
                Input::Pot {
                    device, channel, ..
                } => {
                    if !iio.contains_key(device.as_str()) {
                        if ctx.is_none() {
                            let c = Context::new()
                                .map_err(|e| format!("cannot open IIO context: {e}"))?;
                            ctx = Some(c);
                        }
                        let dev = ctx
                            .as_ref()
                            .expect("opened above")
                            .find_device(device)
                            .ok_or_else(|| in_context(&format!("no IIO device \"{device}\"")))?;
                        let period = Duration::from_millis(self.adc.poll_ms);
                        let full_scale = self.adc.full_scale as f32;
                        iio.insert(device, IioDevice::new(dev, period, full_scale));
                    }
                    iio.get_mut(device.as_str())
                        .expect("inserted above")
                        .channel(channel.as_str())
                        .map_err(|e| in_context(&e))?
                }
                Input::Button { chip, line, .. } => Box::new(
                    CDevGpio::new(i as u64, chip_path(chip), *line).map_err(|e| {
                        in_context(&format!("cannot request {chip} line {line}: {e}"))
                    })?,
                ),
            };
            let position = input.position().map_err(|e| in_context(&e))?;
            collector
                .register(source, position)
                .map_err(|e| in_context(&e))?;
            log::info!("{label} set up");
        }

        Ok(Io {
            collector,
            iio_devices: iio.into_values().collect(),
        })
    }
}

/// Accept `gpiochip0` as shorthand for `/dev/gpiochip0`.
fn chip_path(chip: &str) -> PathBuf {
    if chip.contains('/') {
        chip.into()
    } else {
        Path::new("/dev").join(chip)
    }
}

/// Stand-in source for the dry-run registration in [`Config::validate`].
struct Unused;

impl hardware::source::EventSource<Event> for Unused {
    fn events(
        &mut self,
    ) -> futures_core::stream::BoxStream<'static, hardware::error::Result<Event>> {
        Box::pin(tokio_stream::empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(text: &str) -> String {
        Config::parse(text).unwrap_err().to_string()
    }

    #[test]
    fn full_example_parses() {
        let config = Config::parse(
            r#"
            [midi]
            device = "hw:other"
            channel = 10

            [adc]
            poll_ms = 2

            [[input]]
            type = "pot"
            name = "volume"
            channel = "voltage0"
            control = 7

            [[input]]
            type = "button"
            chip = "gpiochip1"
            line = 23
            "#,
        )
        .unwrap();
        assert_eq!(config.midi.device, "hw:other");
        assert_eq!(config.channel().unwrap(), Channel::Ch10);
        assert_eq!(config.adc.poll_ms, 2);
        assert_eq!(
            config.inputs,
            vec![
                Input::Pot {
                    name: Some("volume".into()),
                    device: DEFAULT_ADC.into(),
                    channel: "voltage0".into(),
                    control: Some(7),
                },
                Input::Button {
                    name: None,
                    chip: "gpiochip1".into(),
                    line: 23,
                    note: None,
                },
            ]
        );
    }

    #[test]
    fn sections_default() {
        let config = Config::parse("[[input]]\ntype = \"pot\"\nchannel = \"voltage0\"").unwrap();
        assert_eq!(config.midi, MidiConfig::default());
        assert_eq!(config.adc, AdcConfig::default());
    }

    #[test]
    fn typos_are_rejected() {
        assert!(
            err("[midi]\nchanel = 2\n[[input]]\ntype = \"pot\"\nchannel = \"v\"")
                .contains("chanel")
        );
        assert!(err("[[input]]\ntype = \"pot\"\nchannel = \"v\"\ncontrl = 3").contains("contrl"));
        assert!(err("[[input]]\ntype = \"slider\"\nchannel = \"v\"").contains("slider"));
    }

    #[test]
    fn bad_values_are_rejected() {
        let pot = "[[input]]\ntype = \"pot\"\nchannel = \"v\"";
        assert!(err(&format!("[midi]\nchannel = 0\n{pot}")).contains("1-16"));
        assert!(err(&format!("[midi]\nchannel = 17\n{pot}")).contains("1-16"));
        assert!(err(&format!("[adc]\npoll_ms = 0\n{pot}")).contains("poll_ms"));
        assert!(err("").contains("no inputs"));
    }

    #[test]
    fn midi_clashes_name_the_input() {
        let e = err(r#"
            [[input]]
            type = "pot"
            channel = "voltage0"
            control = 16
            [[input]]
            type = "pot"
            name = "second"
            channel = "voltage1"
            control = 16
            "#);
        assert!(e.contains("input 2 (pot \"second\")"), "{e}");
        assert!(e.contains("already registered"), "{e}");

        let e = err("[[input]]\ntype = \"pot\"\nchannel = \"v\"\ncontrol = 40");
        assert!(e.contains("14-bit"), "{e}");
        let e = err("[[input]]\ntype = \"button\"\nchip = \"c\"\nline = 1\nnote = 200");
        assert!(e.contains("note 200"), "{e}");
    }

    #[test]
    fn shipped_example_is_valid() {
        let config = Config::parse(include_str!("../../../systemd/winslow.toml")).unwrap();
        assert_eq!(config.inputs.len(), 2);
    }

    #[test]
    fn path_resolution() {
        assert_eq!(
            Config::path(Some("/tmp/x.toml".into())),
            PathBuf::from("/tmp/x.toml")
        );
        assert_eq!(chip_path("gpiochip0"), PathBuf::from("/dev/gpiochip0"));
        assert_eq!(chip_path("/dev/gpiochip2"), PathBuf::from("/dev/gpiochip2"));
    }
}
