use std::sync::{Arc, Mutex};

use gstreamer::{
    prelude::ObjectExt,
    traits::{ElementExt, GstBinExt},
    BusSyncReply, Element, ElementFactory, Message, Pipeline,
};
use once_cell::sync::OnceCell;
use tokio::sync::mpsc;
use tracing::{debug, error, trace, warn};

pub fn find_element(name: &str) -> ElementFactory {
    ElementFactory::find(name).unwrap_or_else(|| panic!("Could not find {}", name))
}

pub static WEBRTCBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
pub static DECODEBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();

pub struct VideoProcessor {
    pipeline: Arc<Pipeline>,
    removal_tx: mpsc::Sender<Element>,
    removal_rx: Mutex<Option<mpsc::Receiver<Element>>>,
}

impl VideoProcessor {
    pub fn init() -> Result<Self, anyhow::Error> {
        gstreamer::init()?;

        WEBRTCBIN_FACTORY.set(find_element("webrtcbin")).unwrap();
        DECODEBIN_FACTORY.set(find_element("decodebin")).unwrap();

        let pipeline = Pipeline::new(None);
        let (removal_tx, removal_rx) = mpsc::channel(100);
        Ok(VideoProcessor {
            pipeline: Arc::new(pipeline),
            removal_tx,
            removal_rx: Mutex::new(Some(removal_rx)),
        })
    }

    pub async fn main_loop(
        &self,
        sync_handler: impl Fn(&Message) + Send + Sync + 'static,
    ) -> Result<(), anyhow::Error> {
        let (msg_tx, mut msg_rx) = mpsc::unbounded_channel();
        self.pipeline
            .bus()
            .unwrap()
            .set_sync_handler(move |_bus, msg| {
                sync_handler(msg);
                let _ = msg_tx.send(msg.to_owned());
                BusSyncReply::Drop
            });

        self.pipeline.set_state(gstreamer::State::Playing)?;

        let mut removal_rx = self.removal_rx.lock().unwrap().take().unwrap();

        loop {
            tokio::select! {
                Some(msg) = msg_rx.recv() => {
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
                Some(removed_elem) = removal_rx.recv() => {
                    removed_elem.set_state(gstreamer::State::Null).unwrap();
                    self.pipeline.remove(&removed_elem).unwrap();
                }
            }
        }
    }

    pub fn pipeline(&self) -> &Pipeline {
        &self.pipeline
    }

    pub fn queue_removal(&self, elem: Element) {
        self.removal_tx.blocking_send(elem).unwrap();
    }
}
