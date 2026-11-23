//! The board's inputs and link UART, as described in the devicetree
//! (`boards/rpi_pico.overlay`) and collected by `board.c`.

use core::ffi::c_int;

use zephyr::raw::{self, device, gpio_dt_spec};

/// Mirrors `struct winslow_encoder` in board.c.
#[repr(C)]
pub struct Encoder {
    pub a: gpio_dt_spec,
    pub b: gpio_dt_spec,
    pub steps_per_detent: u32,
}

unsafe extern "C" {
    fn winslow_buttons(count: *mut usize) -> *const gpio_dt_spec;
    fn winslow_encoders(count: *mut usize) -> *const Encoder;
    fn winslow_link_uart() -> *const device;
    fn winslow_pot_count() -> usize;
    fn winslow_pots_init() -> c_int;
    fn winslow_pot_read(index: usize) -> c_int;
}

pub fn buttons() -> &'static [gpio_dt_spec] {
    let mut count = 0;
    // SAFETY: board.c returns a pointer to a static array of `count` items.
    unsafe { core::slice::from_raw_parts(winslow_buttons(&mut count), count) }
}

pub fn encoders() -> &'static [Encoder] {
    let mut count = 0;
    // SAFETY: as above.
    unsafe { core::slice::from_raw_parts(winslow_encoders(&mut count), count) }
}

pub fn link_uart() -> *const device {
    // SAFETY: returns a static device; no preconditions.
    unsafe { winslow_link_uart() }
}

/// The pots, i.e. the ADC channels listed in the devicetree.
pub struct Pots(usize);

impl Pots {
    /// Highest raw reading (12-bit ADC).
    pub const MAX: u16 = 4095;

    pub fn new() -> Result<Self, &'static str> {
        // SAFETY: no preconditions; board.c checks the ADC is ready.
        unsafe {
            if winslow_pots_init() != 0 {
                return Err("cannot set up ADC channels");
            }
            Ok(Self(winslow_pot_count()))
        }
    }

    pub fn len(&self) -> usize {
        self.0
    }

    /// One conversion of pot `index` (0..=MAX), or None if it failed.
    pub fn read(&self, index: usize) -> Option<u16> {
        // SAFETY: board.c bounds-checks `index`; the channels were set up in `new`.
        let value = unsafe { winslow_pot_read(index) };
        u16::try_from(value).ok()
    }
}

/// An input pin. Reads are logical: "active" per the devicetree flags, so a
/// pressed active-low button reads `true`.
pub struct Input(&'static gpio_dt_spec);

impl Input {
    pub fn new(spec: &'static gpio_dt_spec) -> Result<Self, &'static str> {
        // SAFETY: `spec` comes from the devicetree, so its port is a valid
        // GPIO device.
        unsafe {
            if !raw::gpio_is_ready_dt(spec) {
                return Err("GPIO controller not ready");
            }
            if raw::gpio_pin_configure_dt(spec, raw::ZR_GPIO_INPUT) != 0 {
                return Err("cannot configure pin as input");
            }
        }
        Ok(Self(spec))
    }

    pub fn pin(&self) -> u8 {
        self.0.pin
    }

    pub fn read(&self) -> bool {
        // SAFETY: configured as an input in `new`.
        let value: c_int = unsafe { raw::gpio_pin_get_dt(self.0) };
        // Errors read as inactive; on the RP2040 a read can't actually fail.
        value > 0
    }
}
