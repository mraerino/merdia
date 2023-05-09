use std::{net::Ipv6Addr, sync::Arc};

use display::DisplaySetup;
use gstreamer::{
    prelude::ElementExtManual,
    traits::{ElementExt, GstBinExt},
    Element, ElementFactory,
};
use gstreamer_gl::gst_video::VideoCapsBuilder;
use video::VideoProcessor;

mod display;
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

    let res = macos_workaround::run(|| {
        // - setup gstreamer pipeline
        let video_proc = Arc::new(video::VideoProcessor::init()?);

        // - setup opengl/egl drawing
        let (display, gst_context_provider) = DisplaySetup::create(video_proc.pipeline())?;

        // connect mixer to output
        let mixer = ElementFactory::make("glvideomixer").build()?;
        video_proc.pipeline().add(&mixer)?;
        mixer.link(display.sink())?;

        // initial source for testing
        {
            let src = ElementFactory::make("gltestsrc")
                .property_from_str("pattern", "smpte")
                .build()?;
            video_proc.pipeline().add(&src)?;

            let (width, height) = display.size();
            let caps = VideoCapsBuilder::new()
                .any_features()
                .width(width as _)
                .height(height as _)
                .build();
            src.link_filtered(&mixer, &caps).unwrap();

            src.sync_state_with_parent().unwrap();
        }

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
        rt.spawn(async move {
            video_proc
                .main_loop(move |msg| gst_context_provider.bus_sync_handler(msg))
                .await
        });

        // run graphics loop
        display.main_loop()
    });
    if let Err(err) = &res {
        eprintln!("error backtrace:\n{}", err.backtrace());
    }
    res
}
