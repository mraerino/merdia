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
    event,
    glib::{clone::Downgrade, GString},
    prelude::*,
    promise::Promise,
    Bin, Caps, DebugGraphDetails, Element, ElementFactory, EventView, Object as GstObject, Pad,
    PadDirection, PadProbeData, PadProbeReturn, PadProbeType, Pipeline, Structure,
};
use gstreamer_sdp::SDPMessage;
use gstreamer_webrtc::{
    WebRTCBundlePolicy, WebRTCDTLSTransportState, WebRTCICEConnectionState,
    WebRTCICEGatheringState, WebRTCPeerConnectionState, WebRTCRTPTransceiver, WebRTCSDPType,
    WebRTCSessionDescription, WebRTCSignalingState,
};
use once_cell::sync::OnceCell;
use serde::{Deserialize, Serialize, Serializer};
use std::{
    fmt::Display,
    net::Ipv6Addr,
    process::Stdio,
    sync::{Arc, Mutex, Weak},
};
use tokio::{
    io::AsyncWriteExt,
    process::Command,
    sync::{mpsc, watch},
};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info, trace, warn};
use webrtc_ice::candidate::{candidate_base::unmarshal_candidate, Candidate};

mod macos_workaround;
mod stun;

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
    mixer: Element,
}

const FRONTEND: &[u8] = include_bytes!("../frontend/index.html");

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

type PipelineParts = (Pipeline, Element, watch::Receiver<(i32, i32)>);

fn create_pipeline() -> Result<PipelineParts, anyhow::Error> {
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

    Ok((pipeline, mixer, sizing_rx))
}

async fn run() -> Result<(), anyhow::Error> {
    if std::env::var("RUST_LOG").is_err() {
        std::env::set_var("RUST_LOG", "merdia=debug");
    }
    tracing_subscriber::fmt::init();
    gstreamer::init()?;

    WEBRTCBIN_FACTORY.set(find_element("webrtcbin")).unwrap();
    DECODEBIN_FACTORY.set(find_element("decodebin")).unwrap();
    let videoout_bin =
        gstreamer::parse_bin_from_description("queue leaky=downstream ! videoconvert", true)
            .expect("failed to create videoout bin");
    VIDEOOUTBIN.set(videoout_bin).unwrap();

    let (pipeline, mixer, mut sizing_changed) = create_pipeline()?;
    let bus = pipeline.bus().unwrap();
    let mut msg_stream = bus.stream().fuse();

    pipeline
        .set_state(gstreamer::State::Playing)
        .expect("Couldn't set pipeline to Playing");

    let state = Arc::new(ServerState { pipeline, mixer });

    let mut init_sizing_rx = sizing_changed.clone();
    let init_state = Arc::clone(&state);
    tokio::spawn(async move {
        if init_sizing_rx.changed().await.is_ok() {
            let src = find_element("videotestsrc")
                .create()
                .property_from_str("pattern", "smpte")
                .build()
                .unwrap();
            init_state.pipeline.add(&src).unwrap();

            let (w, h) = &*init_sizing_rx.borrow();
            let filter_props = Structure::builder("video/x-raw")
                .field("width", w)
                .field("height", h)
                .build();
            let filter = Caps::builder_full_with_any_features()
                .structure(filter_props)
                .build();
            src.link_filtered(&init_state.mixer, &filter).unwrap();

            src.sync_state_with_parent().unwrap();
        }
    });

    tokio::spawn(async move {
        while sizing_changed.changed().await.is_ok() {
            debug!("canvas size changed: {:?}", &*sizing_changed.borrow());
        }
    });

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
        .route("/debug/pipeline", get(show_pipeline))
        .with_state(state);

    let mut server_loop =
        axum::Server::bind(&(Ipv6Addr::UNSPECIFIED, 3000).into()).serve(app.into_make_service());

    let stun_server = stun::Server::new().start((Ipv6Addr::UNSPECIFIED, 3478).into());
    tokio::pin!(stun_server);

    loop {
        tokio::select! {
            res = &mut server_loop => return Ok(res?),
            res = &mut stun_server => res?,
            Some(msg) = msg_stream.next() => {
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
    Error(#[serde(serialize_with = "use_display")] anyhow::Error),
}

async fn show_pipeline(
    State(state): State<Arc<ServerState>>,
) -> Result<(HeaderMap, Vec<u8>), HttpError> {
    let data = state
        .pipeline
        .debug_to_dot_data(DebugGraphDetails::MEDIA_TYPE.union(DebugGraphDetails::CAPS_DETAILS));

    let mut child = Command::new("dot")
        .arg("-Tsvg")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;

    let stdin = child.stdin.as_mut().unwrap();
    stdin.write_all(data.as_bytes()).await?;

    let out = child.wait_with_output().await?;
    if !out.status.success() {
        return Err(anyhow!(
            "failed to execute process: exit code {:?}",
            out.status.code()
        )
        .into());
    };

    let mut headers = HeaderMap::new();
    headers.append(CONTENT_TYPE, "image/svg+xml".try_into().unwrap());

    Ok((headers, out.stdout))
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

    let (events_tx, events_rx) = mpsc::channel(20);
    let events_tx = Arc::new(events_tx);
    let error_events_tx = Arc::clone(&events_tx);

    let candidate_events_tx = Arc::downgrade(&events_tx);
    peer_conn.connect("on-ice-candidate", false, move |values| {
        let _webrtc = values[0].get::<Element>().expect("Invalid argument");
        let mlineindex = values[1].get::<u32>().expect("Invalid argument");
        let candidate = values[2].get::<String>().expect("Invalid argument");
        debug!(%candidate, "got ICE candidate");

        if let Some(events_tx) = candidate_events_tx.upgrade() {
            let _ = events_tx.try_send(WebRTCResponse::Candidate(WebRTCCandidate {
                candidate,
                sdp_mline_index: Some(mlineindex),
                ..Default::default()
            }));
        }

        None
    });

    peer_conn.connect("on-negotiation-needed", false, move |_values| {
        debug!("negotiation needed!");
        None
    });

    // workaround because `connect_notify` takes `Fn` which cannot hold mutable state
    let events_dropper = Mutex::new(Some(events_tx));
    peer_conn.connect_notify(None, move |conn, param| {
        if param.value_type() == WebRTCSignalingState::static_type() {
            let state: WebRTCSignalingState = conn.property(param.name());
            info!(?state, "signaling state changed");
        } else if param.value_type() == WebRTCICEGatheringState::static_type() {
            match conn.property(param.name()) {
                WebRTCICEGatheringState::Complete => {
                    // close the channel by dropping to end the HTTP response
                    events_dropper.lock().unwrap().take();
                    info!("ICE gathering complete");
                }
                state => info!(?state, "ICE gathering state changed"),
            }
        } else if param.value_type() == WebRTCICEConnectionState::static_type() {
            let state: WebRTCICEConnectionState = conn.property(param.name());
            info!(?state, "ICE connection state changed");
        } else if param.value_type() == WebRTCPeerConnectionState::static_type() {
            let state: WebRTCPeerConnectionState = conn.property(param.name());
            info!(?state, "Peer connection state changed");
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

        let pad_name: GString = pad.name();
        info!(?pad, ?pad_name, "added webrtc pad");

        let transceiver: WebRTCRTPTransceiver = pad.property("transceiver");
        let transport = transceiver.receiver().unwrap().transport().unwrap();
        transport.connect_state_notify(move |tr| {
            use WebRTCDTLSTransportState::*;
            let state = tr.state();
            debug!(?state, "transport state changed");
            match tr.state() {
                Closed | Failed => {
                    let parent = transceiver
                        .dynamic_cast_ref::<GstObject>()
                        .unwrap()
                        .parent()
                        .unwrap();
                    let parent: &Element = parent.downcast_ref().unwrap();
                    if let Some(pad) = parent.src_pads().iter().find(|p| p.name() == pad_name) {
                        debug!(?pad, "found src pad");
                        pad.add_probe(PadProbeType::BLOCK, |pad, _info| {
                            let peer = pad.peer().unwrap();
                            pad.unlink(&peer).unwrap();
                            peer.send_event(event::Eos::new());
                            PadProbeReturn::Drop
                        });
                    }
                }
                _ => {}
            }
        });

        let decoder = DECODEBIN_FACTORY.get().unwrap().create().build().unwrap();
        let state_ref = state.downgrade();
        decoder.connect_pad_added(move |_dec, pad| {
            let caps = pad.current_caps().unwrap();
            let name = caps.structure(0).unwrap().name();

            if !name.starts_with("video/") {
                debug!(?caps, "ignoring pad with non-video cap");
                return;
            }

            let decoder_state_ref = Weak::clone(&state_ref);
            probe_eos(pad, move |pad, _| {
                debug!("EOS event on decoder pad!");
                unlink_from_peer_and_maybe_remove(pad, &decoder_state_ref);
            });

            let Some(state) = state_ref.upgrade() else { return };

            let sink = VIDEOOUTBIN.get().unwrap().clone();
            let videoout_state_ref = Weak::clone(&state_ref);
            probe_eos(&sink.static_pad("src").unwrap(), move |pad, _| {
                debug!("EOS event on video out bin pad!");
                unlink_from_peer_and_maybe_remove(pad, &videoout_state_ref);
            });

            let queue = sink.by_name("queue0").unwrap();
            queue.connect("overrun", false, |_| {
                debug!("queue experienced overrun, dropping old buffers");
                // todo: turn into metric
                None
            });

            state.pipeline.add(&sink).unwrap();
            sink.sync_state_with_parent().unwrap();

            sink.link(&state.mixer).unwrap();

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
    let sdp = cleanup_invalid_candidates(sdp);
    let offer = WebRTCSessionDescription::new(WebRTCSDPType::Offer, sdp);
    info!("got SDP offer");

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
    info!("created answer");

    let webrtc_answer = SessionDescription {
        r#type: answer.type_(),
        sdp: answer.sdp(),
    };

    tokio::spawn(async move {
        // Set the LocalDescription
        let (prom, set_local_fut) = Promise::new_future();
        peer_conn.emit_by_name::<()>("set-local-description", &[&answer, &prom]);
        if let Err(err) = set_local_fut.await {
            let _ = error_events_tx
                .send(WebRTCResponse::Error(anyhow!(
                    "failed to set local description: {:?}",
                    err
                )))
                .await;
            warn!(?err, "failed to set local description");
        } else {
            debug!("set local description");
        }
    });

    let stream = stream::once(async { WebRTCResponse::Answer(webrtc_answer) })
        .chain(ReceiverStream::new(events_rx))
        .map(|r| serde_json::to_string(&r));
    debug!("returning response stream");
    Ok(StreamBody::new(stream))
}

// we're going to remove any `candidate` attributes from
// the SDP media if they don't contain a valid IP address.
//
// we might receive ICE candidates with an mDNS address
// those cause a delay (10s i think) before timing out
// the mDNS lookup which halts the entire transaction.
//
// for why browsers do this, see:
// https://bloggeek.me/psa-mdns-and-local-ice-candidates-are-coming/
fn cleanup_invalid_candidates(mut sdp: SDPMessage) -> SDPMessage {
    for media in sdp.medias_mut() {
        let mut removal = Vec::new();
        for (i, attr) in media.attributes().enumerate() {
            if attr.key() == "candidate" {
                if let Some(val) = attr.value() {
                    if let Ok(candidate) = unmarshal_candidate(val) {
                        if candidate.addr().ip().is_unspecified() {
                            debug!(addr = %candidate.address(), "dropping invalid candidate");
                            removal.push(i);
                        }
                    }
                }
            }
        }
        // remove in reverse so the relevant indices don't change when removing
        for idx in removal.into_iter().rev() {
            let _ = media.remove_attribute(idx as u32);
        }
    }
    sdp
}

fn probe_eos<F: Fn(&Pad, &event::Eos) + Send + Sync + 'static>(pad: &Pad, f: F) {
    pad.add_probe(PadProbeType::EVENT_BOTH, move |pad, info| {
        if let Some(PadProbeData::Event(ref ev)) = info.data {
            if let EventView::Eos(eos) = ev.view() {
                f(pad, eos);
            }
        }
        PadProbeReturn::Ok
    });
}

fn unlink_from_peer_and_maybe_remove(pad: &Pad, state: &Weak<ServerState>) {
    let state = Weak::clone(state);
    pad.add_probe(PadProbeType::BLOCK, move |pad, _info| {
        let peer = pad.peer().unwrap();
        pad.unlink(&peer).unwrap();
        let elem = pad.parent_element().unwrap();
        if !elem.pads().iter().any(|p| p.is_linked()) {
            debug!(src = ?elem.type_(), "element has no more linked pads, removing from pipeline");
            elem.set_state(gstreamer::State::Null).unwrap();
            if let Some(state) = state.upgrade() {
                state.pipeline.remove(&elem).unwrap();
            }
        }
        PadProbeReturn::Drop
    });
}
