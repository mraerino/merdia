use std::{net::Ipv6Addr, sync::Arc};

use gstreamer::{prelude::ElementExtManual, traits::GstBinExt, Element, ElementFactory};
use video::VideoProcessor;

mod http;
mod macos_workaround;
mod stun;
mod video;

pub struct SharedState {
    video_proc: Arc<VideoProcessor>,
    mixer: Element,
}

fn main() -> Result<(), anyhow::Error> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "merdia=debug");
    }
    tracing_subscriber::fmt::init();

    macos_workaround::run(|| {
        // restructure:
        // - setup opengl/egl drawing
        // - setup gstreamer pipeline
        let video_proc = Arc::new(video::VideoProcessor::init()?);

        // connect mixer to output
        let mixer = ElementFactory::make("glvideomixer").build()?;
        video_proc.pipeline().add(&mixer)?;

        let shared_state = Arc::new(SharedState {
            video_proc: Arc::clone(&video_proc),
            mixer,
        });

        // create async runtime
        let rt = tokio::runtime::Runtime::new().unwrap();

        // - start servers
        // todo: handle errors
        rt.spawn(async move { http::create_server(Arc::clone(&shared_state)).await });
        rt.spawn(async move { stun::Server::start((Ipv6Addr::UNSPECIFIED, 3478).into()).await });

        // - run video loop
        rt.block_on(video_proc.main_loop())
    })
}
