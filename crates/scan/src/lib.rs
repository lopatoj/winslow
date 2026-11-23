//! Turning raw pin and ADC samples into clean input events, for inputs sampled at a
//! fixed rate (the Pico firmware samples every 500 µs).
//!
//! Kept free of any hardware or OS code so it can be tested on the laptop.

#![no_std]

/// Debounces a button by requiring a new level to hold for several
/// consecutive samples before accepting it.
#[derive(Debug, Clone, Copy)]
pub struct Debouncer {
    pressed: bool,
    /// Consecutive samples that disagreed with `pressed`.
    count: u8,
}

impl Debouncer {
    /// Starts released, so a button held at power-up is reported as a press
    /// once it has been stable for long enough.
    pub const fn new() -> Self {
        Self {
            pressed: false,
            count: 0,
        }
    }

    pub fn pressed(&self) -> bool {
        self.pressed
    }

    /// Feed one sample (`true` = pressed). Returns the new state when it
    /// changes, after `samples` consecutive samples agree.
    pub fn update(&mut self, sample: bool, samples: u8) -> Option<bool> {
        if sample == self.pressed {
            self.count = 0;
            return None;
        }
        self.count += 1;
        if self.count < samples.max(1) {
            return None;
        }
        self.pressed = sample;
        self.count = 0;
        Some(sample)
    }
}

impl Default for Debouncer {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes a mechanical quadrature encoder into detents ("clicks").
///
/// Every valid transition between the four A/B states moves one step;
/// contact bounce shows up as steps back and forth that cancel out, so the
/// pins need no separate debouncing. Transitions where both pins changed at
/// once (a sample was missed) are ignored.
///
/// Common encoders (e.g. EC11) move 4 steps per detent and rest with both
/// contacts open. The state at power-up is taken as a rest position, and
/// partial steps are dropped whenever the encoder comes back to rest, so a
/// missed step can't leave it permanently off by a fraction of a detent.
#[derive(Debug, Clone, Copy)]
pub struct Quadrature {
    state: u8,
    rest: u8,
    steps: i8,
    steps_per_detent: u8,
}

/// Step for each (previous state << 2 | new state); states are A << 1 | B.
/// Clockwise is 00 → 10 → 11 → 01 → 00 (A leads B).
const STEP: [i8; 16] = [
    0, -1, 1, 0, // from 00
    1, 0, 0, -1, // from 01
    -1, 0, 0, 1, // from 10
    0, 1, -1, 0, // from 11
];

impl Quadrature {
    /// `a` and `b` are the pins' levels now, with the encoder at rest.
    /// `steps_per_detent` is 4 for most encoders, 2 or 1 for some.
    pub fn new(a: bool, b: bool, steps_per_detent: u8) -> Self {
        let state = (a as u8) << 1 | b as u8;
        Self {
            state,
            rest: state,
            steps: 0,
            steps_per_detent: steps_per_detent.clamp(1, 4),
        }
    }

    /// Feed one sample of both pins; returns the detents moved (usually 0 or
    /// ±1), positive for clockwise.
    pub fn update(&mut self, a: bool, b: bool) -> i8 {
        let new = (a as u8) << 1 | b as u8;
        let step = STEP[(self.state << 2 | new) as usize];
        self.state = new;
        if step == 0 {
            return 0;
        }

        let per = self.steps_per_detent as i8;
        self.steps += step;
        let mut detents = self.steps / per;
        self.steps -= detents * per;

        if self.at_rest() {
            // Round a mostly-complete detent up (a step was missed), and drop
            // any remainder so we stay aligned with the physical detents.
            if 2 * self.steps.abs() >= per && self.steps != 0 {
                detents += self.steps.signum();
            }
            self.steps = 0;
        }
        detents
    }

    fn at_rest(&self) -> bool {
        match self.steps_per_detent {
            4 => self.state == self.rest,
            // Detents at both the rest state and its opposite.
            2 => self.state == self.rest || self.state == self.rest ^ 0b11,
            _ => true,
        }
    }
}

/// Smooths a noisy ADC reading and only reports meaningful changes.
///
/// A moving average (each sample counts 1/8) takes out random noise, and a
/// deadband stops a pot resting between two codes from sending a stream of
/// alternating values. Values within the deadband of either end snap to that
/// end, so the ends stay reachable.
#[derive(Debug, Clone, Copy)]
pub struct Smoother {
    /// Running average, in 1/16ths of a code.
    average: Option<u32>,
    reported: Option<u16>,
    max: u16,
    deadband: u16,
}

impl Smoother {
    /// `max` is the highest raw reading (4095 for 12 bits); `deadband` is the
    /// smallest change (in raw codes) worth reporting.
    pub const fn new(max: u16, deadband: u16) -> Self {
        Self {
            average: None,
            reported: None,
            max,
            deadband,
        }
    }

    /// The last value reported, if any.
    pub fn value(&self) -> Option<u16> {
        self.reported
    }

    /// Feed one raw sample; returns a new value (0..=max) when it has moved
    /// by at least the deadband or reached an end.
    pub fn update(&mut self, raw: u16) -> Option<u16> {
        let sample = (raw.min(self.max) as u32) << 4;
        // Start at the first reading rather than ramping up from zero.
        let average = match self.average {
            None => sample,
            Some(avg) => avg - avg / 8 + sample / 8,
        };
        self.average = Some(average);

        let mut value = ((average + 8) >> 4) as u16;
        if value <= self.deadband {
            value = 0;
        } else if value >= self.max.saturating_sub(self.deadband) {
            value = self.max;
        }

        let changed = match self.reported {
            None => true,
            Some(last) => {
                last.abs_diff(value) >= self.deadband
                    || (value != last && (value == 0 || value == self.max))
            }
        };
        if changed {
            self.reported = Some(value);
            Some(value)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `raw` until the value settles; return the last value reported.
    fn settle(s: &mut Smoother, raw: u16) -> Option<u16> {
        (0..100).filter_map(|_| s.update(raw)).last()
    }

    #[test]
    fn first_reading_is_reported_immediately() {
        let mut s = Smoother::new(4095, 8);
        assert_eq!(s.update(2000), Some(2000));
        assert_eq!(s.value(), Some(2000));
    }

    #[test]
    fn noise_within_the_deadband_is_silent() {
        let mut s = Smoother::new(4095, 8);
        s.update(2000);
        for i in 0..1000u16 {
            // ±6 codes of jitter, plus an occasional big spike.
            let raw = if i % 97 == 0 { 2040 } else { 1994 + (i * 7) % 13 };
            assert_eq!(s.update(raw), None, "sample {i}");
        }
    }

    #[test]
    fn real_movement_is_tracked() {
        let mut s = Smoother::new(4095, 8);
        s.update(1000);
        // Settles within the deadband of the true position (exact match
        // isn't guaranteed: that's what the deadband is for).
        for target in [3000, 1500, 1508, 2222] {
            settle(&mut s, target);
            let v = s.value().unwrap();
            assert!(v.abs_diff(target) < 8, "{v} vs {target}");
        }
    }

    #[test]
    fn ends_are_reachable_and_snap() {
        let mut s = Smoother::new(4095, 8);
        s.update(2000);
        assert_eq!(settle(&mut s, 3), Some(0));
        assert_eq!(settle(&mut s, 4090), Some(4095));
        // Sitting at an end with a little noise stays quiet.
        for raw in [4088, 4095, 4091, 4093] {
            assert_eq!(s.update(raw), None);
        }
    }

    #[test]
    fn out_of_range_readings_are_clamped() {
        let mut s = Smoother::new(4095, 8);
        assert_eq!(s.update(u16::MAX), Some(4095));
    }

    /// Clockwise from rest (11): 11 → 01 → 00 → 10 → 11.
    const CW: [(bool, bool); 4] = [(false, true), (false, false), (true, false), (true, true)];

    fn turn(q: &mut Quadrature, samples: &[(bool, bool)]) -> i32 {
        samples.iter().map(|&(a, b)| q.update(a, b) as i32).sum()
    }

    fn ccw() -> [(bool, bool); 4] {
        // Reverse order, ending back at rest.
        [CW[2], CW[1], CW[0], (true, true)]
    }

    #[test]
    fn debounce_needs_consecutive_samples() {
        let mut d = Debouncer::new();
        assert_eq!(d.update(true, 3), None);
        assert_eq!(d.update(true, 3), None);
        assert_eq!(d.update(false, 3), None, "bounce resets the count");
        assert_eq!(d.update(true, 3), None);
        assert_eq!(d.update(true, 3), None);
        assert_eq!(d.update(true, 3), Some(true));
        assert!(d.pressed());
        assert_eq!(d.update(true, 3), None, "only reports changes");
        for _ in 0..2 {
            assert_eq!(d.update(false, 3), None);
        }
        assert_eq!(d.update(false, 3), Some(false));
    }

    #[test]
    fn one_detent_per_full_cycle() {
        let mut q = Quadrature::new(true, true, 4);
        assert_eq!(turn(&mut q, &CW), 1);
        assert_eq!(turn(&mut q, &CW), 1);
        assert_eq!(turn(&mut q, &ccw()), -1);
    }

    #[test]
    fn detent_is_reported_on_arrival_at_rest() {
        let mut q = Quadrature::new(true, true, 4);
        assert_eq!(turn(&mut q, &CW[..3]), 0);
        assert_eq!(turn(&mut q, &CW[3..]), 1);
    }

    #[test]
    fn contact_bounce_cancels_out() {
        let mut q = Quadrature::new(true, true, 4);
        let bouncy = [
            CW[0], (true, true), CW[0], // bounce on the first edge
            CW[1], CW[0], CW[1], // and the second
            CW[2], CW[3],
        ];
        assert_eq!(turn(&mut q, &bouncy), 1);
    }

    #[test]
    fn half_turn_and_back_is_nothing() {
        let mut q = Quadrature::new(true, true, 4);
        assert_eq!(turn(&mut q, &[CW[0], CW[1], CW[0], (true, true)]), 0);
    }

    #[test]
    fn missed_sample_still_counts_the_detent() {
        let mut q = Quadrature::new(true, true, 4);
        // 01 → 10 skips 00: both pins changed, so that transition is ignored.
        assert_eq!(turn(&mut q, &[CW[0], CW[2], CW[3]]), 1);
        // And the next detent starts cleanly.
        assert_eq!(turn(&mut q, &CW), 1);
    }

    #[test]
    fn rest_state_comes_from_power_up() {
        // An encoder that rests with both contacts closed (00).
        let mut q = Quadrature::new(false, false, 4);
        let cw_from_00 = [(true, false), (true, true), (false, true), (false, false)];
        assert_eq!(turn(&mut q, &cw_from_00), 1);
    }

    #[test]
    fn two_steps_per_detent() {
        let mut q = Quadrature::new(true, true, 2);
        assert_eq!(turn(&mut q, &CW[..2]), 1);
        assert_eq!(turn(&mut q, &CW[2..]), 1);
    }

    #[test]
    fn one_step_per_detent() {
        let mut q = Quadrature::new(true, true, 1);
        assert_eq!(turn(&mut q, &CW), 4);
    }
}
