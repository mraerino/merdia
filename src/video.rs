use std::sync::Arc;

use futures_util::StreamExt;
use gstreamer::{prelude::ObjectExt, traits::ElementExt, Bin, ElementFactory, Pipeline};
use once_cell::sync::OnceCell;
use tracing::{debug, error, trace, warn};

pub fn find_element(name: &str) -> ElementFactory {
    ElementFactory::find(name).unwrap_or_else(|| panic!("Could not find {}", name))
}

pub static WEBRTCBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
pub static DECODEBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
pub static VIDEOOUTBIN: OnceCell<Bin> = OnceCell::new();

pub struct VideoProcessor {
    pipeline: Arc<Pipeline>,
}

impl VideoProcessor {
    pub fn init() -> Result<Self, anyhow::Error> {
        gstreamer::init()?;

        WEBRTCBIN_FACTORY.set(find_element("webrtcbin")).unwrap();
        DECODEBIN_FACTORY.set(find_element("decodebin")).unwrap();
        let videoout_bin =
            gstreamer::parse_bin_from_description("queue leaky=downstream ! videoconvert", true)
                .expect("failed to create videoout bin");
        VIDEOOUTBIN.set(videoout_bin).unwrap();

        let pipeline = Pipeline::new(None);
        Ok(VideoProcessor {
            pipeline: Arc::new(pipeline),
        })
    }

    pub async fn main_loop(&self) -> Result<(), anyhow::Error> {
        let bus = self.pipeline.bus().unwrap();
        let mut msg_stream = bus.stream().fuse();

        self.pipeline.set_state(gstreamer::State::Playing)?;

        while let Some(msg) = msg_stream.next().await {
            let v = msg.view();
            use gstreamer::MessageView::*;
            let src = msg.src().map(|s| s.type_());
            match v {
                StateChanged(s) => {
                    trace!(current = ?s.current(), prev = ?s.old(), ?src, "gstreamer state changed")
                }
                StreamStatus(s) => {
                    let (t, _) = s.get();
                    trace!(r#type = ?t, src = ?msg.src(), "stream status notification");
                }
                StreamStart(m) => {
                    debug!(?m, ?src, "stream started");
                }
                Qos(_) => {
                    trace!("qos notification");
                }
                Warning(w) => {
                    warn!(err = ?w.error().message(), debug = ?w.debug(), ?src, "gstreamer warning");
                }
                Error(e) => {
                    error!(err = ?e.error().message(), debug = ?e.debug(), ?src, "gstreamer error");
                }
                v => {
                    debug!(msg = ?v, ?src, "gstreamer message");
                }
            }
        }

        Ok(())
    }

    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }
}
