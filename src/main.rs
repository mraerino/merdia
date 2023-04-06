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
use serde::{Deserialize, Serialize};
use std::{net::Ipv6Addr, sync::Arc};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use webrtc::{
    api::{
        interceptor_registry::register_default_interceptors,
        media_engine::{MediaEngine, MIME_TYPE_OPUS, MIME_TYPE_VP8},
        APIBuilder, API,
    },
    ice_transport::{ice_candidate::RTCIceCandidate, ice_connection_state::RTCIceConnectionState},
    interceptor::registry::Registry,
    peer_connection::{
        configuration::RTCConfiguration, sdp::session_description::RTCSessionDescription,
    },
    rtp_transceiver::rtp_codec::{RTCRtpCodecCapability, RTCRtpCodecParameters, RTPCodecType},
};

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
    api: API,
    config: RTCConfiguration,
}

const FRONTEND: &[u8] = include_bytes!("../frontend/index.html");

#[tokio::main]
async fn main() -> Result<(), anyhow::Error> {
    let api = {
        // Create a MediaEngine object to configure the supported codec
        let mut m = MediaEngine::default();

        // Setup the codecs you want to use.
        // We'll use VP8 and Opus but you can also define your own
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_VP8.to_owned(),
                    clock_rate: 90000,
                    channels: 0,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 96,
                ..Default::default()
            },
            RTPCodecType::Video,
        )?;
        m.register_codec(
            RTCRtpCodecParameters {
                capability: RTCRtpCodecCapability {
                    mime_type: MIME_TYPE_OPUS.to_owned(),
                    clock_rate: 48000,
                    channels: 2,
                    sdp_fmtp_line: "".to_owned(),
                    rtcp_feedback: vec![],
                },
                payload_type: 111,
                ..Default::default()
            },
            RTPCodecType::Audio,
        )?;

        // Create a InterceptorRegistry. This is the user configurable RTP/RTCP Pipeline.
        // This provides NACKs, RTCP Reports and other features. If you use `webrtc.NewPeerConnection`
        // this is enabled by default. If you are manually managing You MUST create a InterceptorRegistry
        // for each PeerConnection.
        let mut registry = Registry::new();
        // Use the default set of Interceptors
        registry = register_default_interceptors(registry, &mut m)?;

        APIBuilder::new()
            .with_media_engine(m)
            .with_interceptor_registry(registry)
            .build()
    };

    // Prepare the configuration
    let config = RTCConfiguration::default();

    let state = ServerState { api, config };

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

    axum::Server::bind(&(Ipv6Addr::LOCALHOST, 3000).into())
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

#[derive(Deserialize)]
struct OfferParams {
    offer: RTCSessionDescription,
}

#[allow(clippy::large_enum_variant)]
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum WebRTCResponse {
    Offer(RTCSessionDescription),
    Candidate(RTCIceCandidate),
}

#[axum::debug_handler]
async fn screen_share(
    State(state): State<Arc<ServerState>>,
    Json(params): Json<OfferParams>,
) -> Result<StreamBody<impl Stream<Item = serde_json::Result<String>>>, HttpError> {
    // Create a new RTCPeerConnection
    let peer_connection = state.api.new_peer_connection(state.config.clone()).await?;

    // Allow us to receive 1 audio track, and 1 video track
    peer_connection
        .add_transceiver_from_kind(RTPCodecType::Audio, None)
        .await?;
    peer_connection
        .add_transceiver_from_kind(RTPCodecType::Video, None)
        .await?;

    peer_connection.on_track(Box::new(move |track, _, _| Box::pin(async {})));

    peer_connection.on_ice_connection_state_change(Box::new(
        move |connection_state: RTCIceConnectionState| {
            println!("Connection State has changed {connection_state}");
            Box::pin(async {})
        },
    ));

    let (candidate_tx, candidate_rx) = mpsc::channel(20);
    let mut candidate_tx = Some(candidate_tx);
    peer_connection.on_ice_candidate(Box::new(move |candidate| {
        if let Some(candidate) = candidate {
            if let Some(candidate_tx) = &candidate_tx {
                let _ = candidate_tx.try_send(candidate);
            }
        } else {
            // close the channel by dropping to end the HTTP response
            drop(candidate_tx.take());
        };
        Box::pin(async {})
    }));

    // Set the remote SessionDescription
    peer_connection.set_remote_description(params.offer).await?;

    // Create an answer
    let answer = peer_connection.create_answer(None).await?;
    // Sets the LocalDescription, and starts our UDP listeners
    peer_connection.set_local_description(answer).await?;

    let local_desc = peer_connection
        .local_description()
        .await
        .ok_or(anyhow!("failed to generate local description"))?;

    let stream = stream::once(async { WebRTCResponse::Offer(local_desc) })
        .chain(ReceiverStream::new(candidate_rx).map(WebRTCResponse::Candidate))
        .map(|r| serde_json::to_string(&r));

    Ok(StreamBody::new(stream))
}
