use async_stream::stream;
use futures_core::stream::BoxStream;
use hardware::source::EventSource;
use tokio_stream::{StreamExt, StreamMap};
use wmidi::{Channel, MidiMessage};

use crate::adapter::AsMidi;

/// Range handed out by [`MidiType::NextControl`]. CC 16-19 are general purpose
/// and 20-31 undefined, so they won't clash with standard controllers.
const FIRST_CONTROL: u8 = 16;
/// Highest controller that can carry a 14-bit value: its LSB goes on
/// controller + 32, and 32-63 are reserved for exactly those LSBs.
const LAST_CONTROL: u8 = 31;
/// Range handed out by [`MidiType::NextRelative`]: CC 102-119 are undefined.
const FIRST_RELATIVE: u8 = 102;
const LAST_RELATIVE: u8 = 119;
/// CC 120-127 are channel mode messages (all notes off, etc.).
const LAST_CC: u8 = 119;
/// Range handed out by [`MidiType::NextNote`] (36 = C1, the usual first drum pad).
const FIRST_NOTE: u8 = 36;
const LAST_NOTE: u8 = 127;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidiType {
    /// The lowest free note from 36 up.
    NextNote,
    /// The lowest free 14-bit controller from 16 to 31.
    NextControl,
    /// The lowest free single controller from 102 to 119 (for encoders).
    NextRelative,
    /// A specific note.
    Note(wmidi::Note),
    /// A specific 14-bit controller (0-31); its LSB is sent on controller + 32.
    Control(wmidi::ControlFunction),
    /// A specific single controller (0-119), e.g. for relative values.
    Relative(wmidi::ControlFunction),
}

pub struct MidiCollector<E: AsMidi + Send + 'static> {
    channel: Channel,
    sources: Vec<(Box<dyn EventSource<E> + Send>, u8)>,
    /// Bit n set = note n is taken.
    used_notes: u128,
    /// Bit n set = controller n is taken. A 14-bit controller takes both n
    /// and its LSB partner n + 32.
    used_ccs: u128,
}

impl<E: AsMidi + Send + 'static> MidiCollector<E> {
    pub fn new(channel: Channel) -> Self {
        Self {
            channel,
            sources: vec![],
            used_notes: 0,
            used_ccs: 0,
        }
    }

    /// Register a source at a MIDI position.
    ///
    /// Notes and controllers are tracked separately (note 20 and CC 20 are
    /// different messages), and a position can only be registered once.
    /// `Next*` picks the lowest free position, skipping any taken explicitly.
    /// On error nothing is registered and no position is used up.
    pub fn register(
        &mut self,
        source: Box<dyn EventSource<E> + Send>,
        position: MidiType,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let note_free = |n: u8| self.used_notes & (1u128 << n) == 0;
        let cc_free = |c: u8| self.used_ccs & (1u128 << c) == 0;
        // A 14-bit controller needs its LSB partner free too.
        let control_free = |c: u8| cc_free(c) && cc_free(c + 32);

        let p = match position {
            MidiType::NextNote => {
                let n = (FIRST_NOTE..=LAST_NOTE)
                    .find(|&n| note_free(n))
                    .ok_or("no free notes left (36-127)")?;
                self.used_notes |= 1 << n;
                n
            }
            MidiType::NextControl => {
                let c = (FIRST_CONTROL..=LAST_CONTROL)
                    .find(|&c| control_free(c))
                    .ok_or("no free 14-bit controllers left (16-31)")?;
                self.used_ccs |= 1 << c | 1 << (c + 32);
                c
            }
            MidiType::NextRelative => {
                let c = (FIRST_RELATIVE..=LAST_RELATIVE)
                    .find(|&c| cc_free(c))
                    .ok_or("no free controllers left for encoders (102-119)")?;
                self.used_ccs |= 1 << c;
                c
            }
            MidiType::Note(note) => {
                let n = u8::from(note);
                if !note_free(n) {
                    return Err(format!("note {n} is already registered").into());
                }
                self.used_notes |= 1 << n;
                n
            }
            MidiType::Control(control) => {
                let c = u8::from(control);
                if c > LAST_CONTROL {
                    return Err(format!(
                        "controller {c} can't carry 14-bit CC: use 0-{LAST_CONTROL} \
                         (the LSB is sent on controller + 32)"
                    )
                    .into());
                }
                if !control_free(c) {
                    return Err(format!(
                        "controller {c} (or its LSB partner {}) is already registered",
                        c + 32
                    )
                    .into());
                }
                self.used_ccs |= 1 << c | 1 << (c + 32);
                c
            }
            MidiType::Relative(control) => {
                let c = u8::from(control);
                if c > LAST_CC {
                    return Err(format!(
                        "controller {c} is a channel mode message: use 0-{LAST_CC}"
                    )
                    .into());
                }
                if !cc_free(c) {
                    return Err(format!("controller {c} is already registered").into());
                }
                self.used_ccs |= 1 << c;
                c
            }
        };
        self.sources.push((source, p));
        Ok(())
    }
}

impl<E: AsMidi + Send + 'static> EventSource<MidiMessage<'static>> for MidiCollector<E> {
    fn events(&mut self) -> BoxStream<'static, hardware::error::Result<MidiMessage<'static>>> {
        let channel = self.channel;
        let mut streams = StreamMap::new();
        for (i, (source, position)) in self.sources.iter_mut().enumerate() {
            let position = *position;
            streams.insert(
                i,
                source
                    .events()
                    .map(move |r| r.map(|e| e.midi(channel, position))),
            );
        }

        Box::pin(stream! {
            while let Some((_, item)) = streams.next().await {
                match item {
                    Ok(messages) => for m in messages { yield Ok(m) },
                    Err(e) => yield Err(e),
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hardware::event::Event;
    use wmidi::{ControlFunction, Note, U7};

    /// A source that never produces anything; registration doesn't read it.
    struct Silent;

    impl EventSource<Event> for Silent {
        fn events(&mut self) -> BoxStream<'static, hardware::error::Result<Event>> {
            Box::pin(tokio_stream::empty())
        }
    }

    fn collector() -> MidiCollector<Event> {
        MidiCollector::new(Channel::Ch1)
    }

    fn register(c: &mut MidiCollector<Event>, position: MidiType) -> Result<u8, String> {
        c.register(Box::new(Silent), position)
            .map(|()| c.sources.last().unwrap().1)
            .map_err(|e| e.to_string())
    }

    fn note(n: u8) -> MidiType {
        MidiType::Note(Note::try_from(n).unwrap())
    }

    fn control(c: u8) -> MidiType {
        MidiType::Control(ControlFunction(U7::new(c).unwrap()))
    }

    #[test]
    fn next_note_uses_every_note_up_to_127() {
        let mut c = collector();
        let assigned: Vec<u8> = (FIRST_NOTE..=LAST_NOTE)
            .map(|_| register(&mut c, MidiType::NextNote).unwrap())
            .collect();
        assert_eq!(assigned, (36..=127).collect::<Vec<u8>>());
        assert!(register(&mut c, MidiType::NextNote).is_err());
        assert_eq!(c.sources.len(), 92, "the failed call must not register");
    }

    #[test]
    fn next_control_uses_16_to_31_then_fails() {
        let mut c = collector();
        let assigned: Vec<u8> = (0..16)
            .map(|_| register(&mut c, MidiType::NextControl).unwrap())
            .collect();
        assert_eq!(assigned, (16..=31).collect::<Vec<u8>>());
        assert!(register(&mut c, MidiType::NextControl).is_err());
    }

    #[test]
    fn explicit_control_above_31_is_rejected() {
        let mut c = collector();
        for cc in [32, 63, 74, 127] {
            let err = register(&mut c, control(cc)).unwrap_err();
            assert!(err.contains("14-bit"), "{err}");
        }
        assert!(c.sources.is_empty());
        // Low controllers are allowed explicitly, even below the auto range.
        assert_eq!(register(&mut c, control(0)), Ok(0));
        assert_eq!(register(&mut c, control(31)), Ok(31));
    }

    #[test]
    fn duplicates_are_rejected() {
        let mut c = collector();
        assert_eq!(register(&mut c, note(60)), Ok(60));
        assert!(register(&mut c, note(60)).is_err());
        assert_eq!(register(&mut c, control(7)), Ok(7));
        assert!(register(&mut c, control(7)).is_err());
        assert_eq!(c.sources.len(), 2);
    }

    #[test]
    fn auto_assignment_skips_explicit_positions() {
        let mut c = collector();
        register(&mut c, note(36)).unwrap();
        register(&mut c, control(16)).unwrap();
        assert_eq!(register(&mut c, MidiType::NextNote), Ok(37));
        assert_eq!(register(&mut c, MidiType::NextControl), Ok(17));
    }

    #[test]
    fn explicit_position_after_auto_assignment_collides() {
        let mut c = collector();
        assert_eq!(register(&mut c, MidiType::NextControl), Ok(16));
        assert!(register(&mut c, control(16)).is_err());
    }

    #[test]
    fn notes_and_controls_are_separate() {
        let mut c = collector();
        assert_eq!(register(&mut c, note(20)), Ok(20));
        assert_eq!(register(&mut c, control(20)), Ok(20));
    }

    fn relative(c: u8) -> MidiType {
        MidiType::Relative(ControlFunction(U7::new(c).unwrap()))
    }

    #[test]
    fn next_relative_uses_102_to_119_then_fails() {
        let mut c = collector();
        let assigned: Vec<u8> = (0..18)
            .map(|_| register(&mut c, MidiType::NextRelative).unwrap())
            .collect();
        assert_eq!(assigned, (102..=119).collect::<Vec<u8>>());
        assert!(register(&mut c, MidiType::NextRelative).is_err());
    }

    #[test]
    fn relative_and_14_bit_controllers_dont_overlap() {
        let mut c = collector();
        assert_eq!(register(&mut c, control(16)), Ok(16));
        // 16's LSB lives on 48.
        let err = register(&mut c, relative(48)).unwrap_err();
        assert!(err.contains("already registered"), "{err}");
        assert!(register(&mut c, relative(16)).is_err());
        // And the other way round: a relative CC on 49 blocks 14-bit 17.
        assert_eq!(register(&mut c, relative(49)), Ok(49));
        assert!(register(&mut c, control(17)).is_err());
        assert_eq!(register(&mut c, MidiType::NextControl), Ok(18), "skips 17");
    }

    #[test]
    fn channel_mode_controllers_are_rejected() {
        let mut c = collector();
        for cc in 120..=127 {
            assert!(register(&mut c, relative(cc)).is_err());
        }
        assert_eq!(register(&mut c, relative(119)), Ok(119));
    }

    #[test]
    fn failed_registration_uses_up_nothing() {
        let mut c = collector();
        assert!(register(&mut c, control(40)).is_err());
        assert_eq!(register(&mut c, MidiType::NextControl), Ok(16));
    }
}
