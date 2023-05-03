use anyhow::anyhow;
use axum::{
    body::StreamBody,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode},
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use futures_util::{stream, Stream, StreamExt};
use gstreamer::{
    glib::clone::Downgrade, prelude::*, promise::Promise, Bin, Caps, Element, ElementFactory,
    PadDirection, Pipeline,
};
use gstreamer_sdp::SDPMessage;
use gstreamer_webrtc::{
    WebRTCBundlePolicy, WebRTCFECType, WebRTCICEConnectionState, WebRTCICEGatheringState,
    WebRTCRTPTransceiver, WebRTCRTPTransceiverDirection, WebRTCSDPType, WebRTCSessionDescription,
    WebRTCSignalingState,
};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize, Serializer};
use std::{
    fmt::Display,
    net::Ipv6Addr,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, trace};

mod macos_workaround;

enum HttpError {
    InternalServerError(anyhow::Error),
}

impl IntoResponse for HttpError {
    fn into_response(self) -> axum::response::Response {
        match self {
            HttpError::InternalServerError(e) => {
                (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response()
            }
        }
    }
}

impl<T> From<T> for HttpError
where
    T: Into<anyhow::Error>,
{
    fn from(value: T) -> Self {
        Self::InternalServerError(value.into())
    }
}

struct ServerState {
    pipeline: Pipeline,
}

const FRONTEND: &[u8] = include_bytes!("../frontend/index.html");
const ALLOWED_CODECS: &[&str] = &["H264", "H265", "VP8", "VP9", "AV1"];

static WEBRTCBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
static DECODEBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
static VIDEOOUTBIN: OnceCell<Bin> = OnceCell::new();

fn main() -> Result<(), anyhow::Error> {
    macos_workaround::run(|| {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(run())
    })
}

fn find_element(name: &str) -> ElementFactory {
    ElementFactory::find(name).unwrap_or_else(|| panic!("Could not find {}", name))
}

async fn run() -> Result<(), anyhow::Error> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "merdia=debug");
    }
    tracing_subscriber::fmt::init();
    gstreamer::init()?;

    WEBRTCBIN_FACTORY.set(find_element("webrtcbin")).unwrap();
    DECODEBIN_FACTORY.set(find_element("decodebin")).unwrap();
    let videoout_bin = gstreamer::parse_bin_from_description(
        "queue ! videoconvert ! videoscale ! autovideosink",
        true,
    )
    .expect("failed to create videoout bin");
    VIDEOOUTBIN.set(videoout_bin).unwrap();

    let pipeline = Pipeline::new(None);
    let bus = pipeline.bus().unwrap();
    let mut msg_stream = bus.stream().fuse();

    pipeline
        .set_state(gstreamer::State::Playing)
        .expect("Couldn't set pipeline to Playing");

    let state = ServerState { pipeline };

    let app = Router::new()
        .route(
            "/",
            get(|| async {
                let mut headers = HeaderMap::new();
                headers.append(
                    CONTENT_TYPE,
                    HeaderValue::from_static("text/html; charset=utf-8"),
                );
                (headers, FRONTEND)
            }),
        )
        .route("/screen_share", post(screen_share))
        .with_state(Arc::new(state));

    let mut server_loop =
        axum::Server::bind(&(Ipv6Addr::LOCALHOST, 3000).into()).serve(app.into_make_service());
    loop {
        tokio::select! {
            res = &mut server_loop => return Ok(res?),
            Some(msg) = msg_stream.next() => {
                let v = msg.view();
                use gstreamer::MessageView::*;
                match v {
                    StateChanged(s) => {
                        trace!(current = ?s.current(), prev = ?s.old(), src = ?msg.src(), "gstreamer state changed")
                    }
                    v => {
                        debug!(?v, "gstreamer message");
                    }
                }
            }
        }
    }
}

#[derive(Deserialize)]
struct OfferParams {
    offer: String,
}

#[derive(Default, Serialize)]
struct WebRTCCandidate {
    candidate: String,
    #[serde(rename = "sdpMid")]
    #[serde(skip_serializing_if = "Option::is_none")]
    sdp_mid: Option<String>,
    #[serde(rename = "sdpMLineIndex")]
    #[serde(skip_serializing_if = "Option::is_none")]
    sdp_mline_index: Option<u32>,
    #[serde(rename = "usernameFragment")]
    #[serde(skip_serializing_if = "Option::is_none")]
    username_fragment: Option<String>,
}

fn use_display<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: Display,
    S: Serializer,
{
    serializer.collect_str(value)
}

#[derive(Debug, Serialize)]
struct SessionDescription {
    #[serde(serialize_with = "use_display")]
    r#type: WebRTCSDPType,
    #[serde(serialize_with = "use_display")]
    sdp: SDPMessage,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum WebRTCResponse {
    Answer(SessionDescription),
    Candidate(WebRTCCandidate),
}

async fn screen_share(
    State(state): State<Arc<ServerState>>,
    Json(params): Json<OfferParams>,
) -> Result<StreamBody<impl Stream<Item = serde_json::Result<String>>>, HttpError> {
    // create a webrtcbin for handling this screen share
    let peer_conn = WEBRTCBIN_FACTORY
        .get()
        .unwrap()
        .create()
        .property("bundle-policy", WebRTCBundlePolicy::MaxBundle)
        .build()?;

    let (candidate_tx, candidate_rx) = mpsc::channel(20);
    let candidate_tx = Arc::new(Mutex::new(Some(candidate_tx)));
    let candidate_tx2 = Arc::clone(&candidate_tx);
    peer_conn.connect("on-ice-candidate", false, move |values| {
        let _webrtc = values[0].get::<Element>().expect("Invalid argument");
        let mlineindex = values[1].get::<u32>().expect("Invalid argument");
        let candidate = values[2].get::<String>().expect("Invalid argument");
        debug!(%candidate, "got ICE candidate");

        if let Some(candidate_tx) = candidate_tx.lock().unwrap().as_ref() {
            let _ = candidate_tx.try_send(WebRTCCandidate {
                candidate,
                sdp_mline_index: Some(mlineindex),
                ..Default::default()
            });
        }

        None
    });

    peer_conn.connect("on-negotiation-needed", false, move |_values| {
        info!("negotiation needed!");
        None
    });

    peer_conn.connect_notify(None, move |conn, param| {
        if param.value_type() == WebRTCSignalingState::static_type() {
            match conn.property(param.name()) {
                WebRTCSignalingState::Stable => info!("connection established!"),
                WebRTCSignalingState::Closed => info!("connection closed!"),
                state => info!(?state, "signaling state changed"),
            }
        } else if param.value_type() == WebRTCICEGatheringState::static_type() {
            match conn.property(param.name()) {
                WebRTCICEGatheringState::Complete => {
                    // close the channel by dropping to end the HTTP response
                    candidate_tx2.lock().unwrap().take();
                    info!("ICE gathering complete")
                }
                state => info!(?state, "ICE gathering state changed"),
            }
        } else if param.value_type() == WebRTCICEConnectionState::static_type() {
            let state: WebRTCICEConnectionState = conn.property(param.name());
            info!(?state, "ICE connection state changed");
        } else {
            debug!(
                name = %param.name(),
                value_type = ?param.value_type(),
                "property changed",
            );
        }
    });

    let state_ref = state.downgrade();
    peer_conn.connect_pad_added(move |_conn, pad| {
        if pad.direction() != PadDirection::Src {
            debug!(?pad, "ignoring non-src pad");
            return;
        }

        let Some(state) = state_ref.upgrade() else { return };

        info!(?pad, "added webrtc pad");
        let decoder = DECODEBIN_FACTORY.get().unwrap().create().build().unwrap();
        let state_ref = state.downgrade();
        decoder.connect_pad_added(move |_dec, pad| {
            let caps = pad.current_caps().unwrap();
            let name = caps.structure(0).unwrap().name();

            if !name.starts_with("video/") {
                debug!(?caps, "ignoring pad with non-video cap");
                return;
            }

            let Some(state) = state_ref.upgrade() else { return };

            let sink = VIDEOOUTBIN.get().unwrap().clone();
            state.pipeline.add(&sink).unwrap();
            sink.sync_state_with_parent().unwrap();

            let sinkpad = sink.static_pad("sink").unwrap();
            pad.link(&sinkpad).unwrap();
        });

        state.pipeline.add(&decoder).unwrap();
        decoder.sync_state_with_parent().unwrap();

        let sinkpad = decoder.static_pad("sink").unwrap();
        pad.link(&sinkpad).unwrap();
    });

    // read peer offer
    let sdp = SDPMessage::parse_buffer(params.offer.as_bytes())?;
    let offer = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);
    info!("got SDP offer");

    // Allow us to receive the video track
    let caps = caps_for_offer(&offer)?;
    let transceiver: WebRTCRTPTransceiver = peer_conn.emit_by_name(
        "add-transceiver",
        &[&WebRTCRTPTransceiverDirection::Recvonly, &caps],
    );
    info!("added transceiver");
    transceiver.set_property("do_nack", true);
    transceiver.set_property("fec-type", WebRTCFECType::UlpRed);

    state.pipeline.add(&peer_conn)?;
    peer_conn.sync_state_with_parent()?;
    debug!("added to pipeline");

    // Set the remote SessionDescription
    let (prom, set_remote_fut) = Promise::new_future();
    peer_conn.emit_by_name::<()>("set-remote-description", &[&offer, &Some(prom)]);
    let _ = set_remote_fut.await;
    info!("set remote description");

    // Create an answer
    let (prom, create_answer_fut) = Promise::new_future();
    peer_conn.emit_by_name::<()>("create-answer", &[&None::<gstreamer::Structure>, &prom]);
    let reply = create_answer_fut
        .await
        .map_err(|e| anyhow!("failed to create answer: {:?}", e))?
        .ok_or_else(|| anyhow!("answer creation got no response"))?;
    if reply.has_field("error") {
        let err: gstreamer::glib::Error = reply.get("error")?;
        return Err(err.into());
    }
    let answer: WebRTCSessionDescription = reply.get("answer")?;

    // Set the LocalDescription
    let (prom, set_local_fut) = Promise::new_future();
    peer_conn.emit_by_name::<()>("set-local-description", &[&answer, &prom]);
    let _ = set_local_fut.await; // todo: error handling

    let answer = SessionDescription {
        r#type: answer.type_(),
        sdp: answer.sdp(),
    };
    let stream = stream::once(async { WebRTCResponse::Answer(answer) })
        .chain(ReceiverStream::new(candidate_rx).map(WebRTCResponse::Candidate))
        .map(|r| serde_json::to_string(&r));
    Ok(StreamBody::new(stream))
}

fn caps_for_offer(offer: &WebRTCSessionDescription) -> Result<Caps, anyhow::Error> {
    let sdp = offer.sdp();
    let mut caps = Caps::new_empty();
    for media in sdp.medias() {
        for format in media.formats() {
            let pt = format.parse::<i32>()?;
            let mut tmpcaps = media
                .caps_from_media(pt)
                .ok_or_else(|| anyhow!("failed to get caps for {}", pt))?;
            {
                let tmpcaps = tmpcaps.get_mut().unwrap();

                // tmpcaps
                //     .structure_mut(0)
                //     .unwrap()
                //     .set_name("application/x-rtp");

                media.attributes_to_caps(tmpcaps)?;
            }

            let encoding_name = tmpcaps
                .structure(0)
                .unwrap()
                .get::<&str>("encoding-name")
                .unwrap();

            if ALLOWED_CODECS.contains(&encoding_name) {
                caps.get_mut().unwrap().append(tmpcaps);
            }
        }
    }

    Ok(caps)
}
