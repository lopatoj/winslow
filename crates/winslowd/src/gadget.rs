//! The USB MIDI gadget, whose ALSA card id is `winslow` (so `hw:winslow`).
//!
//! The PocketBeagle has a single USB device port, and BeagleBoard.org images
//! already run a gadget on it (USB networking for 192.168.7.2, a serial
//! console, ...). Only one gadget can be bound at a time, so replacing it
//! would cut off SSH. Instead, if a gadget is already bound, the MIDI
//! function is added to it: the host sees the device re-enumerate once, now
//! with a MIDI port alongside the network interface.
//!
//! At boot the system's gadget may not be bound yet when we start, so we
//! wait for it (up to [`SYSTEM_GADGET_TIMEOUT`]). Only if none appears (e.g.
//! a custom image without one) is a MIDI-only gadget created, and removed
//! again on exit. Creating ours too early would take the port and lock the
//! system's gadget, and with it SSH, out.

use std::fs;
use std::io;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use usb_gadget::{Class, Config, Gadget, Id, Strings, default_udc, function::midi::Midi};

const GADGETS: &str = "/sys/kernel/config/usb_gadget";
/// Function directory name; `midi` is the type, `winslow` the instance.
const FUNCTION: &str = "midi.winslow";
/// ALSA card id, so the device is `hw:winslow` whatever card number it gets.
const CARD_ID: &str = "winslow";
/// How long to wait at startup for the system's gadget to be bound.
const SYSTEM_GADGET_TIMEOUT: Duration = Duration::from_secs(60);

/// Hold until the daemon exits.
///
/// MIDI added to the system's gadget is left in place on exit, so the
/// network isn't interrupted again; it's reused on the next start. Our own
/// MIDI-only gadget is removed when this is dropped.
pub struct UsbGadget {
    _own: Option<usb_gadget::RegGadget>,
}

pub fn setup_gadget() -> io::Result<UsbGadget> {
    if let Some(dir) = wait_for_bound_gadget()? {
        add_midi(&dir)?;
        log::info!("added MIDI to the existing USB gadget {}", dir.display());
        return Ok(UsbGadget { _own: None });
    }
    log::warn!(
        "no system USB gadget appeared within {SYSTEM_GADGET_TIMEOUT:?}; \
         creating a MIDI-only one"
    );

    let mut midi = Midi::builder();
    midi.id = Some(CARD_ID.into());
    midi.in_ports = Some(1);
    midi.out_ports = Some(1);
    let (_midi, handle) = midi.build();

    let udc = default_udc()?;
    let gadget = Gadget::new(
        Class::new(0, 0, 0),
        Id::new(0x1209, 0x0001),
        Strings::new("Winslow", "Winslow MIDI", "0001"),
    )
    .with_config(Config::new("MIDI").with_function(handle))
    .bind(&udc)?;
    log::info!("created a MIDI-only USB gadget");
    Ok(UsbGadget { _own: Some(gadget) })
}

/// Wait for a gadget to be bound to the USB controller.
fn wait_for_bound_gadget() -> io::Result<Option<PathBuf>> {
    let deadline = Instant::now() + SYSTEM_GADGET_TIMEOUT;
    let mut logged = false;
    loop {
        if let Some(dir) = bound_gadget()? {
            return Ok(Some(dir));
        }
        if Instant::now() >= deadline {
            return Ok(None);
        }
        if !logged {
            log::info!("waiting for the system USB gadget to be bound");
            logged = true;
        }
        thread::sleep(Duration::from_millis(250));
    }
}

/// The gadget currently bound to a USB controller, if any.
fn bound_gadget() -> io::Result<Option<PathBuf>> {
    let entries = match fs::read_dir(GADGETS) {
        Ok(entries) => entries,
        // configfs/libcomposite not set up: nobody else has a gadget.
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let dir = entry?.path();
        if !udc_of(&dir)?.is_empty() {
            return Ok(Some(dir));
        }
    }
    Ok(None)
}

fn udc_of(gadget: &Path) -> io::Result<String> {
    Ok(fs::read_to_string(gadget.join("UDC"))?.trim().to_owned())
}

/// Add the MIDI function to every configuration of a bound gadget (the host
/// picks one; e.g. RNDIS for Windows, ECM/NCM for Linux and macOS).
/// Does nothing if it's already there from an earlier run.
fn add_midi(gadget: &Path) -> io::Result<()> {
    let function = gadget.join("functions").join(FUNCTION);
    let configs: Vec<PathBuf> = fs::read_dir(gadget.join("configs"))?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<io::Result<_>>()?;
    let missing: Vec<PathBuf> = configs
        .iter()
        .map(|config| config.join(FUNCTION))
        .filter(|link| !link.exists())
        .collect();
    if function.exists() && missing.is_empty() {
        return Ok(());
    }

    // Functions can only be added while the gadget is unbound.
    let udc = udc_of(gadget)?;
    fs::write(gadget.join("UDC"), "\n")?;

    let result = (|| {
        if !function.exists() {
            fs::create_dir(&function)?;
            fs::write(function.join("id"), CARD_ID)?;
            fs::write(function.join("in_ports"), "1")?;
            fs::write(function.join("out_ports"), "1")?;
        }
        for link in &missing {
            symlink(&function, link)?;
        }
        fs::write(gadget.join("UDC"), &udc)
    })();

    if let Err(e) = result {
        // Most likely the controller ran out of endpoints. Put the gadget
        // back as it was so the network comes back.
        log::error!("adding MIDI to {} failed ({e}); restoring it", gadget.display());
        for link in &missing {
            let _ = fs::remove_file(link);
        }
        let _ = fs::remove_dir(&function);
        fs::write(gadget.join("UDC"), &udc)?;
        return Err(e);
    }
    Ok(())
}
