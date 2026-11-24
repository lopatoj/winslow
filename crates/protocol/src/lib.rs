//! Wire format between the Pico input board and the PocketBeagle, over UART
//! (115200 baud, 8N1).
//!
//! Framing borrows MIDI's trick: the first byte of a message (the status
//! byte) has its top bit set, and data bytes never do. A receiver that joins
//! mid-stream or loses a byte resynchronizes at the next status byte, with
//! no checksums or escaping needed.
//!
//! Pico → PocketBeagle:
//!
//! | Bytes          | Message                                              |
//! |----------------|------------------------------------------------------|
//! | `8n`           | button `n` released                                  |
//! | `9n`           | button `n` pressed                                   |
//! | `An dd`        | encoder `n` turned `dd` detents (7-bit signed)       |
//! | `Bn mm ll`     | pot `n` = `mm << 7 \| ll` (14-bit)                    |
//! | `F0 vv`        | hello, protocol version `vv` (first reply to a sync) |
//! | `F1`           | heartbeat, every 500 ms                              |
//!
//! PocketBeagle → Pico:
//!
//! | Bytes | Message                                                        |
//! |-------|----------------------------------------------------------------|
//! | `F1`  | heartbeat, every 500 ms                                        |
//! | `F2`  | sync request: the Pico answers with hello, a `9n` for every button currently held, and a `Bn` for every pot |
//!
//! The Pico only sends while the PocketBeagle is talking to it: it starts
//! silent, and goes silent again 2 s after the last message it received. The
//! PocketBeagle sends `F2` every 500 ms until it gets an answer, then `F1`.
//! This keeps the Pico from disturbing U-Boot, which listens on the
//! PocketBeagle's UART0 at power-on, when it is wired there.
//!
//! `n` is 0-15, so a board has at most 16 buttons, 16 encoders and 16
//! analog inputs.

#![no_std]

/// Sent in [`Message::Hello`]; bump on incompatible changes.
pub const VERSION: u8 = 1;
/// Highest index a button, encoder or analog input can have.
pub const MAX_INDEX: u8 = 15;
/// Longest encoded message.
pub const MAX_LEN: usize = 3;

const BUTTON_UP: u8 = 0x80;
const BUTTON_DOWN: u8 = 0x90;
const ENCODER: u8 = 0xA0;
const ANALOG: u8 = 0xB0;
const HELLO: u8 = 0xF0;
const HEARTBEAT: u8 = 0xF1;
const SYNC: u8 = 0xF2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Message {
    ButtonUp(u8),
    ButtonDown(u8),
    /// Positive is clockwise (with A and B wired as documented).
    Encoder { index: u8, delta: i8 },
    Analog { index: u8, value: u16 },
    Hello { version: u8 },
    Heartbeat,
    /// Request from the PocketBeagle for the Pico's current state.
    Sync,
}

impl Message {
    /// Encode into `buf`, returning the bytes to send.
    ///
    /// Out-of-range fields are clamped/masked so the result is always a
    /// well-formed message: indices to 0-15, encoder deltas to -64..=63,
    /// analog values to 14 bits.
    pub fn encode(self, buf: &mut [u8; MAX_LEN]) -> &[u8] {
        let index = |i: u8| i & MAX_INDEX;
        let len = match self {
            Message::ButtonUp(i) => {
                buf[0] = BUTTON_UP | index(i);
                1
            }
            Message::ButtonDown(i) => {
                buf[0] = BUTTON_DOWN | index(i);
                1
            }
            Message::Encoder { index: i, delta } => {
                buf[0] = ENCODER | index(i);
                buf[1] = (delta.clamp(-64, 63) as u8) & 0x7F;
                2
            }
            Message::Analog { index: i, value } => {
                let value = value.min(0x3FFF);
                buf[0] = ANALOG | index(i);
                buf[1] = (value >> 7) as u8;
                buf[2] = (value & 0x7F) as u8;
                3
            }
            Message::Hello { version } => {
                buf[0] = HELLO;
                buf[1] = version & 0x7F;
                2
            }
            Message::Heartbeat => {
                buf[0] = HEARTBEAT;
                1
            }
            Message::Sync => {
                buf[0] = SYNC;
                1
            }
        };
        &buf[..len]
    }
}

/// Incremental decoder: feed it bytes as they arrive.
#[derive(Debug, Default)]
pub struct Decoder {
    status: Option<u8>,
    data: [u8; MAX_LEN - 1],
    len: usize,
}

impl Decoder {
    pub const fn new() -> Self {
        Self {
            status: None,
            data: [0; MAX_LEN - 1],
            len: 0,
        }
    }

    /// Feed one byte; returns a message when one is complete. Stray data
    /// bytes and unknown status bytes are skipped.
    pub fn push(&mut self, byte: u8) -> Option<Message> {
        if byte & 0x80 != 0 {
            // A status byte always starts a new message, abandoning any
            // incomplete one.
            self.status = Some(byte);
            self.len = 0;
        } else if self.status.is_some() && self.len < self.data.len() {
            self.data[self.len] = byte;
            self.len += 1;
        } else {
            return None;
        }

        let status = self.status?;
        let index = status & 0x0F;
        let message = match (status & 0xF0, status, self.len) {
            (BUTTON_UP, _, 0) => Message::ButtonUp(index),
            (BUTTON_DOWN, _, 0) => Message::ButtonDown(index),
            (ENCODER, _, 1) => Message::Encoder {
                index,
                // Sign-extend the 7-bit value.
                delta: ((self.data[0] << 1) as i8) >> 1,
            },
            (ANALOG, _, 2) => Message::Analog {
                index,
                value: (self.data[0] as u16) << 7 | self.data[1] as u16,
            },
            (_, HELLO, 1) => Message::Hello {
                version: self.data[0],
            },
            (_, HEARTBEAT, 0) => Message::Heartbeat,
            (_, SYNC, 0) => Message::Sync,
            // Needs more data, or an unknown status byte (whose data bytes
            // will then be ignored up to the next status byte).
            _ => {
                if !self.knows(status) {
                    self.status = None;
                }
                return None;
            }
        };
        self.status = None;
        Some(message)
    }

    fn knows(&self, status: u8) -> bool {
        matches!(status & 0xF0, BUTTON_UP | BUTTON_DOWN | ENCODER | ANALOG)
            || matches!(status, HELLO | HEARTBEAT | SYNC)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use std::vec::Vec;

    fn decode_all(bytes: &[u8]) -> Vec<Message> {
        let mut d = Decoder::new();
        bytes.iter().filter_map(|&b| d.push(b)).collect()
    }

    fn roundtrip(m: Message) -> Message {
        let mut buf = [0; MAX_LEN];
        let bytes = m.encode(&mut buf);
        let out = decode_all(bytes);
        assert_eq!(out.len(), 1, "{m:?} -> {bytes:02X?}");
        out[0]
    }

    #[test]
    fn every_message_roundtrips() {
        let mut all = Vec::new();
        for i in 0..=MAX_INDEX {
            all.push(Message::ButtonUp(i));
            all.push(Message::ButtonDown(i));
            for delta in -64..=63 {
                all.push(Message::Encoder { index: i, delta });
            }
            for value in [0, 1, 127, 128, 8191, 0x3FFF] {
                all.push(Message::Analog { index: i, value });
            }
        }
        all.extend([
            Message::Hello { version: VERSION },
            Message::Heartbeat,
            Message::Sync,
        ]);
        for m in all {
            assert_eq!(roundtrip(m), m);
        }
    }

    #[test]
    fn out_of_range_values_are_clamped() {
        assert_eq!(
            roundtrip(Message::Encoder {
                index: 2,
                delta: 100
            }),
            Message::Encoder {
                index: 2,
                delta: 63
            }
        );
        assert_eq!(
            roundtrip(Message::Encoder {
                index: 2,
                delta: -128
            }),
            Message::Encoder {
                index: 2,
                delta: -64
            }
        );
        assert_eq!(
            roundtrip(Message::Analog {
                index: 0,
                value: u16::MAX
            }),
            Message::Analog {
                index: 0,
                value: 0x3FFF
            }
        );
    }

    #[test]
    fn data_bytes_never_have_the_top_bit_set() {
        let mut buf = [0; MAX_LEN];
        for delta in i8::MIN..=i8::MAX {
            let bytes = Message::Encoder { index: 0, delta }.encode(&mut buf);
            assert!(bytes[1..].iter().all(|b| b & 0x80 == 0));
        }
    }

    #[test]
    fn resynchronizes_after_garbage_and_truncation() {
        let stream = [
            0x05, 0x7F, // stray data bytes before any status
            0x91, // button 1 down
            0xA3, // encoder 3... truncated by the next status byte
            0x81, // button 1 up
            0xC0, 0x12, 0x34, // unknown status and its data
            0xA3, 0x7F, // encoder 3, -1
            0x40, // stray data byte after a complete message
            0xF1,
        ];
        assert_eq!(
            decode_all(&stream),
            [
                Message::ButtonDown(1),
                Message::ButtonUp(1),
                Message::Encoder {
                    index: 3,
                    delta: -1
                },
                Message::Heartbeat,
            ]
        );
    }
}
