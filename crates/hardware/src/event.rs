#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// A button was pressed.
    Rising,
    /// A button was released.
    Falling,
    /// An absolute position, 0.0..=1.0 (pots).
    Change(f32),
    /// A relative movement in steps, positive clockwise (encoders).
    Delta(i32),
}
