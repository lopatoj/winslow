use hardware::event::Event::{self, Change, Falling, Rising};
use wmidi::{
    Channel, ControlFunction,
    MidiMessage::{self},
    Note, U7,
};

pub trait AsMidi {
    fn midi(&self, channel: Channel, position: u8) -> Vec<MidiMessage<'static>>;
}

impl AsMidi for Event {
    fn midi(&self, channel: Channel, position: u8) -> Vec<MidiMessage<'static>> {
        match self {
            Rising => vec![MidiMessage::NoteOn(
                channel,
                Note::from_u8_lossy(position),
                U7::from_u8_lossy(127),
            )],
            Falling => vec![MidiMessage::NoteOff(
                channel,
                Note::from_u8_lossy(position),
                U7::from_u8_lossy(0),
            )],
            Change(value) => {
                let uvalue = (value.clamp(0.0, 1.0) * 16383.0).round() as u16;
                let msb = (uvalue >> 7) as u8;
                let lsb = (uvalue & 0x7F) as u8;
                let controller = position & 0x1F;

                vec![
                    MidiMessage::ControlChange(
                        channel,
                        ControlFunction(U7::from_u8_lossy(controller)),
                        U7::new(msb).unwrap(),
                    ),
                    MidiMessage::ControlChange(
                        channel,
                        ControlFunction(U7::from_u8_lossy(controller + 32)),
                        U7::new(lsb).unwrap(),
                    ),
                ]
            }
        }
    }
}

#[cfg(test)]
const CH: Channel = Channel::Ch3;

/// Unpack a ControlChange into (channel, controller, value), panicking on anything else.
#[cfg(test)]
fn cc(msg: &MidiMessage) -> (Channel, u8, u8) {
    match msg {
        MidiMessage::ControlChange(ch, ControlFunction(num), val) => {
            (*ch, u8::from(*num), u8::from(*val))
        }
        other => panic!("expected ControlChange, got {other:?}"),
    }
}

/// Decode a Change event's two CC messages back into (controller, 14-bit value).
#[cfg(test)]
fn decode_14bit(msgs: &[MidiMessage]) -> (u8, u16) {
    assert_eq!(msgs.len(), 2, "14-bit CC must be exactly two messages");
    let (ch_msb, num_msb, msb) = cc(&msgs[0]);
    let (ch_lsb, num_lsb, lsb) = cc(&msgs[1]);
    assert_eq!(ch_msb, CH);
    assert_eq!(ch_lsb, CH);
    assert_eq!(num_lsb, num_msb + 32, "LSB must go to controller MSB + 32");
    (num_msb, (u16::from(msb) << 7) | u16::from(lsb))
}

#[test]
fn rising_is_note_on_full_velocity() {
    assert_eq!(
        Rising.midi(CH, 60),
        vec![MidiMessage::NoteOn(CH, Note::C4, U7::from_u8_lossy(127))]
    );
}

#[test]
fn falling_is_note_off_zero_velocity() {
    assert_eq!(
        Falling.midi(CH, 60),
        vec![MidiMessage::NoteOff(CH, Note::C4, U7::from_u8_lossy(0))]
    );
}

#[test]
fn note_messages_use_given_channel_and_position() {
    for ch in [Channel::Ch1, Channel::Ch16] {
        for pos in [0u8, 64, 127] {
            match Rising.midi(ch, pos).as_slice() {
                [MidiMessage::NoteOn(c, n, _)] => {
                    assert_eq!(*c, ch);
                    assert_eq!(u8::from(*n), pos);
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }
}

#[test]
fn change_sends_msb_then_lsb_on_paired_controllers() {
    let msgs = Change(0.5).midi(CH, 7);
    let (_, num_msb, _) = cc(&msgs[0]);
    let (_, num_lsb, _) = cc(&msgs[1]);
    assert_eq!((num_msb, num_lsb), (7, 39));
}

#[test]
fn change_endpoints() {
    assert_eq!(decode_14bit(&Change(0.0).midi(CH, 1)), (1, 0));
    assert_eq!(decode_14bit(&Change(1.0).midi(CH, 1)), (1, 16383));
}

#[test]
fn change_midpoint_splits_into_7bit_halves() {
    // 0.5 * 16383 = 8191.5 → rounds to 8192 = 64 << 7 | 0
    let msgs = Change(0.5).midi(CH, 1);
    assert_eq!(cc(&msgs[0]).2, 64);
    assert_eq!(cc(&msgs[1]).2, 0);
}

#[test]
fn change_roundtrips_every_14bit_value() {
    // Also guards against the old 8/8 split, which produced LSBs > 127.
    for expected in 0..=16383u16 {
        let value = f32::from(expected) / 16383.0;
        let (_, got) = decode_14bit(&Change(value).midi(CH, 1));
        assert_eq!(got, expected, "value {value}");
    }
}

#[test]
fn change_is_monotonic() {
    let mut prev = 0;
    for i in 0..=1000 {
        let (_, v) = decode_14bit(&Change(i as f32 / 1000.0).midi(CH, 1));
        assert!(v >= prev, "dropped from {prev} to {v} at step {i}");
        prev = v;
    }
}

#[test]
fn change_clamps_out_of_range_values() {
    assert_eq!(decode_14bit(&Change(-0.5).midi(CH, 1)).1, 0);
    assert_eq!(decode_14bit(&Change(2.0).midi(CH, 1)).1, 16383);
    assert_eq!(decode_14bit(&Change(f32::INFINITY).midi(CH, 1)).1, 16383);
    assert_eq!(decode_14bit(&Change(f32::NEG_INFINITY).midi(CH, 1)).1, 0);
}

#[test]
fn change_nan_does_not_panic_and_maps_to_zero() {
    assert_eq!(decode_14bit(&Change(f32::NAN).midi(CH, 1)).1, 0);
}

#[test]
fn change_masks_position_into_14bit_controller_range() {
    // Documents current behavior: controllers 32+ wrap into 0..=31.
    for pos in [0u8, 31, 32, 40, 127, 255] {
        let (num, _) = decode_14bit(&Change(0.25).midi(CH, pos));
        assert!(num <= 31, "pos {pos} → controller {num}");
        assert_eq!(num, pos & 0x1F);
    }
}

#[test]
fn messages_are_static_and_serialize_to_3_bytes() {
    let msgs: Vec<MidiMessage<'static>> = Change(0.75).midi(CH, 10);
    for m in &msgs {
        let mut buf = [0u8; 3];
        assert_eq!(m.copy_to_slice(&mut buf).unwrap(), 3);
        assert_eq!(buf[0], 0xB0 | CH.index());
        assert!(
            buf[1] < 0x80 && buf[2] < 0x80,
            "data bytes must be 7-bit: {buf:?}"
        );
    }
}
