#[derive(Clone)]
pub enum Event {
    Rising,
    Falling,
    Change(f32),
}
