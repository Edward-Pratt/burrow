//! TLS through rustls for tokio-postgres's in-band negotiation: the
//! connector built from a node's TLS settings, and the stream it gives.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use rustls::ClientConfig;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_postgres::tls::{ChannelBinding, TlsConnect};

use crate::net::tls::{self};

use super::Result;
use super::spec::TlsSpec;

/// TLS through rustls, for tokio-postgres's in-band negotiation.
#[derive(Clone)]
pub(super) struct Rustls {
    config: Arc<ClientConfig>,
    name: ServerName<'static>,
}

impl Rustls {
    pub(super) fn new(spec: &TlsSpec) -> Result<Rustls> {
        Ok(Rustls {
            config: tls::client_config(spec.verify, spec.root_cert.as_deref())?,
            name: ServerName::try_from(spec.name.clone())
                .map_err(|_| format!("{:?} can't be checked against a certificate", spec.name))?,
        })
    }
}

impl<S> TlsConnect<S> for Rustls
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    type Stream = TlsStream<S>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = std::io::Result<TlsStream<S>>> + Send>>;

    fn connect(self, stream: S) -> Self::Future {
        Box::pin(async move {
            let connector = tokio_rustls::TlsConnector::from(self.config);
            Ok(TlsStream(connector.connect(self.name, stream).await?))
        })
    }
}

pub(super) struct TlsStream<S>(tokio_rustls::client::TlsStream<S>);

impl<S: AsyncRead + AsyncWrite + Unpin> tokio_postgres::tls::TlsStream for TlsStream<S> {
    fn channel_binding(&self) -> ChannelBinding {
        ChannelBinding::none()
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncRead for TlsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_read(cx, buf)
    }
}

impl<S: AsyncRead + AsyncWrite + Unpin> AsyncWrite for TlsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.0).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.0).poll_shutdown(cx)
    }
}
