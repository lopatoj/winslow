pub struct Error;

pub type Result<T> = std::result::Result<T, Error>;

pub type NewResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;
