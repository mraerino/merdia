use std::sync::Arc;

use futures_util::StreamExt;
use gstreamer::{
    prelude::{ElementExtManual, GstBinExtManual, ObjectExt},
    traits::{ElementExt, GstBinExt},
    Bin, Caps, Element, ElementFactory, Pipeline, Structure,
};
use once_cell::sync::OnceCell;
use tokio::sync::watch;
use tracing::{debug, error, trace, warn};

pub fn find_element(name: &str) -> ElementFactory {
    ElementFactory::find(name).unwrap_or_else(|| panic!("Could not find {}", name))
}

pub static WEBRTCBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
pub static DECODEBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
pub static VIDEOOUTBIN: OnceCell<Bin> = OnceCell::new();

pub struct VideoProcessor {
    pipeline: Arc<Pipeline>,
    mixer: Element,
    sizing_changed: watch::Receiver<(i32, i32)>,
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

        Self::create_pipeline()
    }

    fn create_pipeline() -> Result<VideoProcessor, anyhow::Error> {
        let pipeline = Pipeline::new(None);

        let kmssink = find_element("kmssink")
            .create()
            .property("driver-name", "virtio_gpu") // todo: auto-detect
            .property("can-scale", false)
            .property("force-modesetting", true)
            .build()?;
        let mixer = find_element("glvideomixer").create().build()?;

        let (sizing_tx, sizing_rx) = watch::channel((0, 0));
        kmssink.connect_notify(Some("display-width"), move |sink, _param| {
            let width = sink.property::<i32>("display-width");
            let height = sink.property::<i32>("display-height");
            let _ = sizing_tx.send((width, height));
        });

        pipeline.add_many(&[&kmssink, &mixer])?;
        mixer.link(&kmssink)?;

        Ok(VideoProcessor {
            pipeline: Arc::new(pipeline),
            mixer,
            sizing_changed: sizing_rx,
        })
    }

    pub async fn main_loop(&self) -> Result<(), anyhow::Error> {
        let bus = self.pipeline.bus().unwrap();
        let mut msg_stream = bus.stream().fuse();

        self.pipeline.set_state(gstreamer::State::Playing)?;

        let mut init_sizing_rx = self.sizing_changed.clone();
        let init_pipeline = Arc::clone(&self.pipeline);
        let init_mixer = self.mixer.clone();
        tokio::spawn(async move {
            if init_sizing_rx.changed().await.is_ok() {
                let src = find_element("videotestsrc")
                    .create()
                    .property_from_str("pattern", "smpte")
                    .build()
                    .unwrap();
                init_pipeline.add(&src).unwrap();

                let (w, h) = &*init_sizing_rx.borrow();
                let filter_props = Structure::builder("video/x-raw")
                    .field("width", w)
                    .field("height", h)
                    .build();
                let filter = Caps::builder_full_with_any_features()
                    .structure(filter_props)
                    .build();
                src.link_filtered(&init_mixer, &filter).unwrap();

                src.sync_state_with_parent().unwrap();
            }
        });

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

    pub fn mixer(&self) -> &Element {
        &self.mixer
    }
}
