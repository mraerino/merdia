use std::{
    net::{SocketAddr, SocketAddrV4},
    sync::Arc,
};

use byte_pool::BytePool;
use bytecodec::{DecodeExt, EncodeExt};
use futures_util::{future::Either, TryFutureExt};
use once_cell::sync::OnceCell;
use stun_codec::{
    define_attribute_enums,
    rfc5389::{attributes::*, errors::BadRequest, methods::BINDING},
    rfc5780::attributes::*,
    Message, MessageClass, MessageDecoder, MessageEncoder,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
};
use tracing::{debug, warn};

// merge attributes between RFCs
define_attribute_enums!(
    Attribute,
    AttributeDecoder,
    AttributeEncoder,
    [
        // RFC 5389
        ErrorCode,
        XorMappedAddress,
        Software,
        // RFC 5780
        ResponseOrigin
    ]
);

pub struct Server {}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] tokio::io::Error),

    #[error("failed to decode STUN message: {0}")]
    MessageDecode(#[from] bytecodec::Error),

    #[error("failed to read STUN message, invalid message")]
    MessageDecodeBroken,
}

static SOFTWARE: Software = Software::new_static("merdia");
static UDP_POOL: OnceCell<BytePool> = OnceCell::new();

impl Server {
    pub fn new() -> Self {
        Self {}
    }

    pub async fn start(self, addr: SocketAddr) -> Result<(), Error> {
        let tcp = TcpListener::bind(addr).await?;

        let udp_pool = UDP_POOL.get_or_init(BytePool::new);
        let udp = Arc::new(UdpSocket::bind(addr).await?);

        loop {
            let mut udp_buf = udp_pool.alloc(4096);
            let fut = tokio::select! {
                conn = tcp.accept() => {
                    let (mut stream, peer_addr) = conn?;
                    Either::Left(async move {
                        let mut buf = Vec::new();
                        stream.read_to_end(&mut buf).await?;

                        let resp = Self::handle_message(&buf, peer_addr, stream.local_addr().ok())?;
                        stream.write_all(&resp).await?;

                        Ok::<(), Error>(())
                    })
                }
                res = udp.recv_from(&mut udp_buf) => {
                    let (n, peer_addr) = res?; // this is error handling
                    let sock = Arc::clone(&udp);
                    Either::Right(async move {
                        let buf = &udp_buf[..n];
                        let resp = Self::handle_message(buf, peer_addr, None)?;
                        sock.send_to(&resp, peer_addr).await?;

                        Ok::<(), Error>(())
                    })
                }
            };
            // spawn the message handler so we can immediately process the next message
            tokio::spawn(fut.inspect_err(|err| {
                warn!(?err, "failed to handle STUN request");
            }));
        }
    }

    fn handle_message(
        buf: &[u8],
        peer_addr: SocketAddr,
        local_addr: Option<SocketAddr>,
    ) -> Result<Vec<u8>, Error> {
        let peer_addr = unmap_legacy_addr(peer_addr);

        let mut decoder = MessageDecoder::new();
        let req = decoder
            .decode_from_bytes(buf)?
            .map_err(|_| Error::MessageDecodeBroken)?;

        let mut encoder = MessageEncoder::new();

        if req.method() != BINDING {
            let b = encoder.encode_into_bytes(Self::bad_request(&req))?;
            return Ok(b);
        }

        debug!(?peer_addr, ?local_addr, "sending stun response");

        let mut resp = Message::new(
            MessageClass::SuccessResponse,
            req.method(),
            req.transaction_id(),
        );
        resp.add_attribute(Attribute::XorMappedAddress(XorMappedAddress::new(
            peer_addr,
        )));
        if let Some(local_addr) = local_addr {
            resp.add_attribute(Attribute::ResponseOrigin(ResponseOrigin::new(local_addr)));
        }
        resp.add_attribute(Attribute::Software(SOFTWARE.clone()));
        encoder.encode_into_bytes(resp).map_err(Into::into)
    }

    fn bad_request(req: &Message<Attribute>) -> Message<Attribute> {
        let mut message = Message::new(
            MessageClass::ErrorResponse,
            req.method(),
            req.transaction_id(),
        );
        message.add_attribute(Attribute::ErrorCode(BadRequest.into()));
        message.add_attribute(Attribute::Software(SOFTWARE.clone()));
        message
    }
}

fn unmap_legacy_addr(addr: SocketAddr) -> SocketAddr {
    let SocketAddr::V6(addr_v6) = addr else {return addr};
    addr_v6
        .ip()
        .to_ipv4_mapped()
        .map(|v4| SocketAddr::V4(SocketAddrV4::new(v4, addr.port())))
        .unwrap_or(addr)
}
