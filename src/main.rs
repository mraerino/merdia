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
use gstreamer::{prelude::*, promise::Promise, Caps, Element, ElementFactory, Pipeline};
use gstreamer_sdp::SDPMessage;
use gstreamer_webrtc::{
    WebRTCBundlePolicy, WebRTCFECType, WebRTCICEGatheringState, WebRTCRTPTransceiver,
    WebRTCRTPTransceiverDirection, WebRTCSDPType, WebRTCSessionDescription, WebRTCSignalingState,
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
static WEBRTCBIN_FACTORY: OnceCell<ElementFactory> = OnceCell::new();
const ALLOWED_CODECS: &[&str] = &["H264", "H265", "VP8", "VP9", "AV1"];

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    gstreamer::init()?;

    let webrtcbin_factory =
        gstreamer::ElementFactory::find("webrtcbin").expect("Could not find webrtcbin");
    WEBRTCBIN_FACTORY.set(webrtcbin_factory).unwrap();

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
            Some(_msg) = msg_stream.next() => {
                //eprintln!("gstreamer message: {:?}", msg.view());
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

        if let Some(candidate_tx) = candidate_tx.lock().unwrap().as_ref() {
            let _ = candidate_tx.try_send(WebRTCCandidate {
                candidate,
                sdp_mline_index: Some(mlineindex),
                ..Default::default()
            });
        }

        None
    });

    peer_conn.connect_notify(None, move |conn, param| {
        if param.value_type() == WebRTCSignalingState::static_type() {
            match conn.property(param.name()) {
                WebRTCSignalingState::Stable => eprintln!("connection established!"),
                WebRTCSignalingState::Closed => eprintln!("connection closed!"),
                s => eprintln!("signaling state changed to: {:?}", s),
            }
        } else if param.value_type() == WebRTCICEGatheringState::static_type() {
            match conn.property(param.name()) {
                WebRTCICEGatheringState::Complete => {
                    // close the channel by dropping to end the HTTP response
                    candidate_tx2.lock().unwrap().take();
                    eprintln!("ICE gathering complete")
                }
                s => eprintln!("ICE gathering state changed to: {:?}", s),
            }
        } else {
            eprintln!(
                "Property changed: {}: {:?}",
                param.name(),
                param.value_type()
            );
        }
    });

    peer_conn.connect_pad_added(|_conn, pad| {
        eprintln!("added pad: {:?}", pad);

        // todo: handle new stream
    });

    // read peer offer
    let sdp = SDPMessage::parse_buffer(params.offer.as_bytes())?;
    //eprintln!("offer sdp: {:#?}", sdp);
    let offer = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);

    // Allow us to receive the video track
    let caps = caps_for_offer(&offer)?;
    eprintln!("using caps: {:#?}", caps);
    let transceiver: WebRTCRTPTransceiver = peer_conn.emit_by_name(
        "add-transceiver",
        &[&WebRTCRTPTransceiverDirection::Recvonly, &caps],
    );
    transceiver.set_property("do_nack", true);
    transceiver.set_property("fec-type", WebRTCFECType::UlpRed);

    state.pipeline.add(&peer_conn)?;
    peer_conn.sync_state_with_parent()?;

    // Set the remote SessionDescription
    let (prom, set_remote_fut) = Promise::new_future();
    peer_conn.emit_by_name::<()>("set-remote-description", &[&offer, &Some(prom)]);
    let _ = set_remote_fut.await;

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
    // eprintln!("answer: {:#?}", answer);
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
