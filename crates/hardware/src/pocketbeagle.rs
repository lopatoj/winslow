//! PocketBeagle board specifics.

use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;

/// I2C bus and address of the TPS65217 power-management chip.
const PMIC_BUS: &str = "/dev/i2c-0";
const PMIC_ADDRESS: libc::c_ulong = 0x24;
/// MUXCTRL register: selects what the PMIC's MUX_OUT pin outputs.
const MUXCTRL: u8 = 0x09;
/// MUXCTRL value: pass MUX_IN through to MUX_OUT.
const MUX_IN: u8 = 0x05;
/// ioctl to address a chip even though a kernel driver owns it.
const I2C_SLAVE_FORCE: libc::c_ulong = 0x0706;

/// Connect header pin P2.36 to the ADC's AIN7.
///
/// On the PocketBeagle, P2.36 isn't wired to the ADC directly: it goes to the
/// PMIC's MUX_IN, and the PMIC's MUX_OUT feeds AIN7. The multiplexer is off
/// at power-up, so AIN7 floats until it's set to pass MUX_IN through. The
/// kernel's PMIC driver doesn't touch this register, so we set it ourselves
/// (needs root). Harmless to repeat.
pub fn enable_ain7() -> io::Result<()> {
    let mut bus = OpenOptions::new().read(true).write(true).open(PMIC_BUS)?;
    // SAFETY: a plain ioctl on a file descriptor we own, with an integer
    // argument.
    if unsafe { libc::ioctl(bus.as_raw_fd(), I2C_SLAVE_FORCE as _, PMIC_ADDRESS) } < 0 {
        return Err(io::Error::last_os_error());
    }
    bus.write_all(&[MUXCTRL, MUX_IN])?;

    // Read it back to make sure it took.
    bus.write_all(&[MUXCTRL])?;
    let mut value = [0u8];
    bus.read_exact(&mut value)?;
    if value[0] & 0x07 != MUX_IN {
        return Err(io::Error::other(format!(
            "PMIC MUXCTRL reads 0x{:02x} after writing 0x{MUX_IN:02x}",
            value[0]
        )));
    }
    Ok(())
}
