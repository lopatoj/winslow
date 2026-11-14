use crate::error::Result;

use futures_core::stream::BoxStream;

pub trait EventSource<E> {
    fn events(&mut self) -> BoxStream<'static, Result<E>>;
}
