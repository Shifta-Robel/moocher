//! TCP + TLS listener for remote nodes. The TLS handshake happens here (not in
//! tonic) so each connection can carry who it is into the gRPC handlers: the
//! peer address and the SHA-256 fingerprint of its client certificate, if any.
//! Handlers read it with `req.extensions().get::<LinkPeer>()`; connections on
//! the hub's unix socket have none and are trusted by file permissions.

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context as TaskCx, Poll};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::server::TlsStream;
use tokio_rustls::TlsAcceptor;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Debug, Clone)]
pub struct LinkPeer {
    pub addr: SocketAddr,
    /// Fingerprint of the presented client certificate (verified against the CA).
    pub cert: Option<String>,
}

impl LinkPeer {
    pub fn ip(&self) -> std::net::IpAddr {
        self.addr.ip().to_canonical()
    }
}

pub struct TlsConn {
    inner: TlsStream<TcpStream>,
    peer: LinkPeer,
}

impl tonic::transport::server::Connected for TlsConn {
    type ConnectInfo = LinkPeer;
    fn connect_info(&self) -> LinkPeer {
        self.peer.clone()
    }
}

impl AsyncRead for TlsConn {
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &mut ReadBuf<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for TlsConn {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>, buf: &[u8]) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut TaskCx<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Accept connections and complete TLS handshakes concurrently (a slow or
/// hostile client cannot stall the accept loop).
pub async fn incoming(listen: &str, cfg: rustls::ServerConfig) -> Result<ReceiverStream<Result<TlsConn, std::io::Error>>> {
    let listener = TcpListener::bind(listen).await.with_context(|| format!("binding link listener {listen}"))?;
    let acceptor = TlsAcceptor::from(Arc::new(cfg));
    let (tx, rx) = mpsc::channel(64);
    tokio::spawn(async move {
        loop {
            let (tcp, addr) = match listener.accept().await {
                Ok(x) => x,
                Err(e) => {
                    tracing::warn!(error = %e, "link accept failed");
                    tokio::time::sleep(Duration::from_millis(200)).await;
                    continue;
                }
            };
            let _ = tcp.set_nodelay(true);
            let acceptor = acceptor.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                match tokio::time::timeout(Duration::from_secs(10), acceptor.accept(tcp)).await {
                    Ok(Ok(tls)) => {
                        let cert = tls.get_ref().1.peer_certificates().and_then(|c| c.first()).map(|c| ocl_protocol::tls::fingerprint(c));
                        let _ = tx.send(Ok(TlsConn { inner: tls, peer: LinkPeer { addr, cert } })).await;
                    }
                    Ok(Err(e)) => tracing::info!(peer = %addr, error = %e, "link TLS handshake failed"),
                    Err(_) => tracing::info!(peer = %addr, "link TLS handshake timed out"),
                }
            });
        }
    });
    Ok(ReceiverStream::new(rx))
}
