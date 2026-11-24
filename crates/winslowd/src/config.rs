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
//! deadband = 8            # smallest change reported, in raw counts
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
//!
//! # Inputs on the Pico board, by their index in its firmware.
//! [pico]
//! port = "/dev/ttyS4"
//!
//! [[input]]
//! type = "pico-button"
//! index = 0
//! note = 40               # optional, as for "button"
//!
//! [[input]]
//! type = "pico-encoder"
//! index = 0
//! control = 102           # optional: 0-119, else the next free from 102
//!
//! [[input]]
//! type = "pico-pot"
//! index = 0
//! control = 20            # optional, as for "pot"
//! ```

use std::collections::{BTreeMap, HashSet};
use std::error::Error;
use std::path::{Path, PathBuf};
use std::time::Duration;

use hardware::event::Event;
use hardware::gpio::CDevGpio;
use hardware::iio::IioDevice;
use hardware::pico::PicoBoard;
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
    #[serde(default)]
    pub pico: PicoConfig,
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
    pub full_scale: u16,
    /// Smallest change reported, in raw counts. Readings jitter by a count or
    /// two with the pot untouched; 8 of 4095 is ~0.2% of the travel.
    pub deadband: u16,
}

impl Default for AdcConfig {
    fn default() -> Self {
        Self {
            poll_ms: 5,
            full_scale: 4095,
            deadband: 8,
        }
    }
}

/// The serial link to the Pico board; only opened if a `pico-*` input exists.
#[derive(Debug, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct PicoConfig {
    /// `/dev/ttyS4` is UART4 (P2.05/P2.07); `/dev/ttyS0` is UART0
    /// (P1.30/P1.32), which is the serial console unless moved.
    pub port: String,
    pub baud: u32,
}

impl Default for PicoConfig {
    fn default() -> Self {
        Self {
            port: "/dev/ttyS4".into(),
            baud: 115200,
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
        /// Reverse the direction (for a pot wired the other way round).
        #[serde(default)]
        invert: bool,
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
    /// A button on the Pico board, sent like [`Input::Button`].
    #[serde(rename = "pico-button")]
    PicoButton {
        name: Option<String>,
        /// The button's number in the Pico firmware (0-15).
        index: u8,
        note: Option<u8>,
    },
    /// A rotary encoder on the Pico board, sent as relative CC (two's
    /// complement: 1 = one click clockwise, 127 = one counter-clockwise).
    #[serde(rename = "pico-encoder")]
    PicoEncoder {
        name: Option<String>,
        index: u8,
        /// Controller 0-119; next free one from 102 if omitted.
        control: Option<u8>,
    },
    /// A pot on the Pico board, sent like [`Input::Pot`].
    #[serde(rename = "pico-pot")]
    PicoPot {
        name: Option<String>,
        index: u8,
        control: Option<u8>,
        #[serde(default)]
        invert: bool,
    },
}

fn default_adc() -> String {
    DEFAULT_ADC.into()
}

impl Input {
    fn label(&self, index: usize) -> String {
        let (name, kind) = match self {
            Input::Pot { name, .. } => (name, "pot".into()),
            Input::Button { name, .. } => (name, "button".into()),
            Input::PicoButton { name, index, .. } => (name, format!("Pico button {index}")),
            Input::PicoEncoder { name, index, .. } => (name, format!("Pico encoder {index}")),
            Input::PicoPot { name, index, .. } => (name, format!("Pico pot {index}")),
        };
        match name {
            Some(name) => format!("input {} ({kind} \"{name}\")", index + 1),
            None => format!("input {} ({kind})", index + 1),
        }
    }

    fn position(&self) -> Result<MidiType, String> {
        let control = |c: u8| {
            U7::new(c)
                .map(ControlFunction)
                .map_err(|_| format!("control {c} is out of range"))
        };
        Ok(match *self {
            Input::Pot { control: c, .. } | Input::PicoPot { control: c, .. } => match c {
                None => MidiType::NextControl,
                Some(c) => MidiType::Control(control(c)?),
            },
            Input::Button { note, .. } | Input::PicoButton { note, .. } => match note {
                None => MidiType::NextNote,
                Some(n) => MidiType::Note(
                    Note::try_from(n).map_err(|_| format!("note {n} is out of range (0-127)"))?,
                ),
            },
            Input::PicoEncoder { control: c, .. } => match c {
                None => MidiType::NextRelative,
                Some(c) => MidiType::Relative(control(c)?),
            },
        })
    }

    /// For Pico inputs, (kind, index) to check for duplicates.
    fn pico_id(&self) -> Option<(&'static str, u8)> {
        match *self {
            Input::PicoButton { index, .. } => Some(("button", index)),
            Input::PicoEncoder { index, .. } => Some(("encoder", index)),
            Input::PicoPot { index, .. } => Some(("pot", index)),
            _ => None,
        }
    }
}

/// The hardware opened from a config, ready to hand to the output.
pub struct Io {
    pub collector: MidiCollector<Event>,
    /// One per distinct IIO device; [`IioDevice::spawn`] each to start
    /// sampling its pots.
    pub iio_devices: Vec<IioDevice>,
    /// Set if any Pico input is configured; [`PicoBoard::spawn`] it to
    /// start reading.
    pub pico: Option<PicoBoard>,
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
        if self.pico.baud == 0 {
            return Err("pico.baud must be set".into());
        }
        let mut pico_inputs = HashSet::new();
        let mut dry_run = MidiCollector::<Event>::new(Channel::Ch1);
        for (i, input) in self.inputs.iter().enumerate() {
            if let Some((kind, index)) = input.pico_id() {
                if index > winslow_protocol::MAX_INDEX {
                    return Err(format!("{}: index must be 0-15", input.label(i)).into());
                }
                if !pico_inputs.insert((kind, index)) {
                    return Err(format!("{}: used more than once", input.label(i)).into());
                }
            }
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
        let mut pico: Option<PicoBoard> = None;

        // P2.36 only reaches AIN7 through the PMIC's multiplexer.
        let uses_ain7 = self.inputs.iter().any(|input| {
            matches!(input, Input::Pot { device, channel, .. }
                if device == DEFAULT_ADC && channel == "voltage7")
        });
        if uses_ain7 {
            match hardware::pocketbeagle::enable_ain7() {
                Ok(()) => log::info!("routed P2.36 to AIN7 through the PMIC multiplexer"),
                Err(e) => log::warn!("cannot route P2.36 to AIN7 ({e}); AIN7 will float"),
            }
        }

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
                        let (full_scale, deadband) = (self.adc.full_scale, self.adc.deadband);
                        iio.insert(device, IioDevice::new(dev, period, full_scale, deadband));
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
                Input::PicoButton { index, .. }
                | Input::PicoEncoder { index, .. }
                | Input::PicoPot { index, .. } => {
                    if pico.is_none() {
                        let board = PicoBoard::open(&self.pico.port, self.pico.baud)
                            .map_err(|e| in_context(&e))?;
                        pico = Some(board);
                    }
                    let board = pico.as_mut().expect("opened above");
                    match input {
                        Input::PicoButton { .. } => board.button(*index),
                        Input::PicoEncoder { .. } => board.encoder(*index),
                        _ => board.pot(*index),
                    }
                    .map_err(|e| in_context(&e))?
                }
            };
            let source = match input {
                Input::Pot { invert: true, .. } | Input::PicoPot { invert: true, .. } => {
                    Box::new(Inverted(source))
                }
                _ => source,
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
            pico,
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

/// Reverses a pot: a position `v` becomes `1 - v`.
struct Inverted(Box<dyn hardware::source::EventSource<Event> + Send>);

impl hardware::source::EventSource<Event> for Inverted {
    fn events(
        &mut self,
    ) -> futures_core::stream::BoxStream<'static, hardware::error::Result<Event>> {
        use tokio_stream::StreamExt;
        Box::pin(self.0.events().map(|item| {
            item.map(|event| match event {
                Event::Change(v) => Event::Change(1.0 - v),
                other => other,
            })
        }))
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
                    invert: false,
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
    fn pico_inputs_parse() {
        let config = Config::parse(
            r#"
            [pico]
            port = "/dev/ttyUSB0"

            [[input]]
            type = "pico-button"
            index = 6
            [[input]]
            type = "pico-encoder"
            name = "browse"
            index = 1
            [[input]]
            type = "pico-pot"
            index = 2
            control = 20
            "#,
        )
        .unwrap();
        assert_eq!(config.pico.port, "/dev/ttyUSB0");
        assert_eq!(config.pico.baud, 115200);
        assert_eq!(
            config.inputs[1],
            Input::PicoEncoder {
                name: Some("browse".into()),
                index: 1,
                control: None,
            }
        );
        assert_eq!(config.inputs[1].position(), Ok(MidiType::NextRelative));
    }

    #[test]
    fn pico_inputs_are_checked() {
        let e = err("[[input]]\ntype = \"pico-pot\"\nindex = 16");
        assert!(e.contains("0-15"), "{e}");
        let e = err("[[input]]\ntype = \"pico-button\"\nindex = 1\n\
                     [[input]]\ntype = \"pico-button\"\nindex = 1");
        assert!(
            e.contains("Pico button 1") && e.contains("more than once"),
            "{e}"
        );
        // Same index, different kinds is fine.
        Config::parse(
            "[[input]]\ntype = \"pico-button\"\nindex = 1\n\
             [[input]]\ntype = \"pico-encoder\"\nindex = 1",
        )
        .unwrap();
        let e = err("[[input]]\ntype = \"pico-encoder\"\nindex = 0\ncontrol = 121");
        assert!(e.contains("channel mode"), "{e}");
    }

    #[test]
    fn invert_option() {
        let config = Config::parse(
            "[[input]]\ntype = \"pico-pot\"\nindex = 0\ninvert = true\n\
             [[input]]\ntype = \"pot\"\nchannel = \"v\"",
        )
        .unwrap();
        assert!(matches!(config.inputs[0], Input::PicoPot { invert: true, .. }));
        assert!(matches!(config.inputs[1], Input::Pot { invert: false, .. }));
    }

    #[tokio::test]
    async fn inverted_flips_positions_only() {
        use hardware::source::EventSource;
        use tokio_stream::StreamExt;

        struct Fixed;
        impl EventSource<Event> for Fixed {
            fn events(
                &mut self,
            ) -> futures_core::stream::BoxStream<'static, hardware::error::Result<Event>> {
                Box::pin(tokio_stream::iter([
                    Ok(Event::Change(0.25)),
                    Ok(Event::Change(1.0)),
                    Ok(Event::Rising),
                ]))
            }
        }
        let events: Vec<Event> = Inverted(Box::new(Fixed))
            .events()
            .map(|r| r.ok().unwrap())
            .collect()
            .await;
        assert_eq!(
            events,
            [Event::Change(0.75), Event::Change(0.0), Event::Rising]
        );
    }

    #[test]
    fn shipped_example_is_valid() {
        let config = Config::parse(include_str!("../../../systemd/winslow.toml")).unwrap();
        // 6 PocketBeagle pots + 7 buttons, 3 encoders and 3 pots on the Pico.
        assert_eq!(config.inputs.len(), 19);
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
