//! The UART link to the PocketBeagle, polled.
//!
//! Messages are at most 3 bytes and the RP2040's UART has a 32-byte FIFO,
//! so writes almost never wait; at 115200 baud a full FIFO drains in ~3 ms.
//!
//! The Pico only talks while the PocketBeagle is talking to it (its daemon
//! sends a heartbeat every 500 ms). This matters when the link is on the
//! PocketBeagle's UART0: U-Boot listens there at power-on, and a stray byte
//! could stop it from booting. U-Boot never sends heartbeats, so we stay
//! quiet until Linux and the daemon are up.

use winslow_protocol::{Decoder, MAX_LEN, Message};
use zephyr::raw::{self, device};

pub struct Link {
    uart: *const device,
    decoder: Decoder,
    /// Ticks since the PocketBeagle last sent anything.
    silent_ticks: u32,
    /// Stop sending after this many ticks of silence.
    timeout_ticks: u32,
}

impl Link {
    pub fn new(uart: *const device, timeout_ticks: u32) -> Result<Self, &'static str> {
        // SAFETY: `uart` is a static devicetree device.
        if !unsafe { raw::device_is_ready(uart) } {
            return Err("link UART not ready");
        }
        Ok(Self {
            uart,
            decoder: Decoder::new(),
            // Start silent: wait to be spoken to.
            silent_ticks: timeout_ticks,
            timeout_ticks,
        })
    }

    /// Call once per scan.
    pub fn tick(&mut self) {
        self.silent_ticks = self.silent_ticks.saturating_add(1);
    }

    /// Whether the PocketBeagle has been heard from recently.
    pub fn connected(&self) -> bool {
        self.silent_ticks < self.timeout_ticks
    }

    /// Send a message, or drop it while nobody is listening.
    pub fn send(&mut self, message: Message) {
        if !self.connected() {
            return;
        }
        let mut buf = [0; MAX_LEN];
        for &byte in message.encode(&mut buf) {
            // SAFETY: the device is ready; poll_out blocks until there's room.
            unsafe { raw::uart_poll_out(self.uart, byte) };
        }
    }

    /// The next complete message from the PocketBeagle, if any. Never blocks.
    pub fn receive(&mut self) -> Option<Message> {
        let mut byte = 0u8;
        // SAFETY: the device is ready; poll_in returns 0 when it read a byte,
        // -1 when there was nothing to read.
        while unsafe { raw::uart_poll_in(self.uart, &mut byte) } == 0 {
            if let Some(message) = self.decoder.push(byte) {
                // Only well-formed messages count as the PocketBeagle
                // talking, not boot-loader or console text.
                self.silent_ticks = 0;
                return Some(message);
            }
        }
        None
    }
}
