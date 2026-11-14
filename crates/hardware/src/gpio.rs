use std::{path::PathBuf, sync::Arc, time::Duration};

use async_stream::try_stream;
use futures_core::stream::BoxStream;
use tokio_stream::StreamExt;

use gpiocdev::{
    Error as GpioCDevError, Request,
    line::{EdgeDetection, EdgeEvent, EdgeKind, Offset},
    tokio::AsyncRequest,
};

use crate::{
    error::{Error, NewResult, Result},
    event::Event,
    source::EventSource,
};

impl From<GpioCDevError> for Error {
    fn from(_error: GpioCDevError) -> Self {
        Self {}
    }
}

impl From<EdgeEvent> for Event {
    fn from(event: EdgeEvent) -> Self {
        match event.kind {
            EdgeKind::Rising => Event::Rising,
            EdgeKind::Falling => Event::Falling,
        }
    }
}

pub struct CDevGpio {
    request: Arc<AsyncRequest>,
    pub id: u64,
}

impl CDevGpio {
    pub fn new<P: Into<PathBuf>>(id: u64, chip: P, line: Offset) -> NewResult<Self> {
        let req = Request::builder()
            .on_chip(chip)
            .with_consumer("winslow")
            .with_line(line)
            .as_input()
            .with_edge_detection(EdgeDetection::BothEdges)
            .with_debounce_period(Duration::from_millis(5))
            .request()?;

        Ok(Self {
            request: Arc::new(AsyncRequest::new(req)),
            id,
        })
    }
}

impl EventSource<Event> for CDevGpio {
    fn events(&mut self) -> BoxStream<'static, Result<Event>> {
        let request = self.request.clone();
        Box::pin(try_stream! {
            let mut events = request.edge_events();
            while let Some(e) = events.next().await {
                yield e?.into();
            }
        })
    }
}
