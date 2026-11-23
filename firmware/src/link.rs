//! The UART link to the PocketBeagle, polled.
//!
//! Messages are at most 3 bytes and the RP2040's UART has a 32-byte FIFO,
//! so writes almost never wait; at 115200 baud a full FIFO drains in ~3 ms.

use winslow_protocol::{Decoder, MAX_LEN, Message};
use zephyr::raw::{self, device};

pub struct Link {
    uart: *const device,
    decoder: Decoder,
}

impl Link {
    pub fn new(uart: *const device) -> Result<Self, &'static str> {
        // SAFETY: `uart` is a static devicetree device.
        if !unsafe { raw::device_is_ready(uart) } {
            return Err("link UART not ready");
        }
        Ok(Self {
            uart,
            decoder: Decoder::new(),
        })
    }

    pub fn send(&mut self, message: Message) {
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
                return Some(message);
            }
        }
        None
    }
}
