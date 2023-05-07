use std::{net::Ipv6Addr, sync::Arc};

use video::VideoProcessor;

mod http;
mod macos_workaround;
mod stun;
mod video;

pub struct SharedState {
    video_proc: Arc<VideoProcessor>,
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

        let shared_state = Arc::new(SharedState {
            video_proc: Arc::clone(&video_proc),
        });

        // - setup app server
        let http_server = http::create_server(Arc::clone(&shared_state));
        // - setup stun server
        let stun_server = stun::Server::start((Ipv6Addr::UNSPECIFIED, 3478).into());

        // create async runtime
        let rt = tokio::runtime::Runtime::new().unwrap();

        // - spawn servers
        // todo: handle errors
        rt.spawn(http_server);
        rt.spawn(stun_server);

        // - run video loop
        rt.block_on(video_proc.main_loop())
    })
}
