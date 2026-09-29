//! Pool transport: plain TCP or TLS behind one `Stream` type.
//!
//! TLS (feature `tls`, on by default) uses rustls with the Mozilla root set and
//! strict hostname verification; extra CAs can be added with `--cert` for pools
//! with a private CA. There is deliberately no "insecure" mode and no silent
//! fallback to plain TCP.
use crate::stratum::Endpoint;
#[cfg(feature = "tls")]
use crate::stratum::remaining;
#[cfg(feature = "tls")]
use anyhow::Context;
use anyhow::Result;
#[cfg(not(feature = "tls"))]
use anyhow::bail;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::path::Path;
use std::time::{Duration, Instant};

/// Socket timeouts on whatever carries the bytes.
pub trait Timeouts {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()>;
}

impl Timeouts for TcpStream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_read_timeout(self, timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        TcpStream::set_write_timeout(self, timeout)
    }
}

pub enum Stream {
    Plain(TcpStream),
    #[cfg(feature = "tls")]
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl Stream {
    fn socket(&self) -> &TcpStream {
        match self {
            Self::Plain(s) => s,
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.get_ref(),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.socket().shutdown(Shutdown::Both);
    }
}

impl Timeouts for Stream {
    fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket().set_read_timeout(timeout)
    }
    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket().set_write_timeout(timeout)
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.read(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Plain(s) => s.write(buf),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Plain(s) => s.flush(),
            #[cfg(feature = "tls")]
            Self::Tls(s) => s.flush(),
        }
    }
}

/// TLS trust settings: Mozilla roots plus any `--cert` PEM files.
#[derive(Clone, Default)]
pub struct TlsSettings {
    #[cfg(feature = "tls")]
    extra_roots: Vec<rustls_pki_types::CertificateDer<'static>>,
}

impl TlsSettings {
    /// Trust the certificates in a PEM file in addition to the built-in roots.
    pub fn add_pem_file(&mut self, path: &Path) -> Result<usize> {
        #[cfg(feature = "tls")]
        {
            use rustls_pki_types::{CertificateDer, pem::PemObject};
            let certs = CertificateDer::pem_file_iter(path)
                .with_context(|| format!("read certificates from {}", path.display()))?
                .collect::<Result<Vec<_>, _>>()
                .with_context(|| format!("parse certificates in {}", path.display()))?;
            anyhow::ensure!(!certs.is_empty(), "no certificates in {}", path.display());
            let count = certs.len();
            self.extra_roots.extend(certs);
            Ok(count)
        }
        #[cfg(not(feature = "tls"))]
        {
            let _ = path;
            bail!("this build has no TLS support (feature `tls`)")
        }
    }

    #[cfg(feature = "tls")]
    fn client_config(&self) -> Result<std::sync::Arc<rustls::ClientConfig>> {
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        for cert in &self.extra_roots {
            roots
                .add(cert.clone())
                .context("add --cert certificate to the trust store")?;
        }
        let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("TLS protocol configuration")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        Ok(std::sync::Arc::new(config))
    }
}

/// Connect (and for TLS, complete the handshake) before `deadline`. Returns the
/// stream and, for TLS, a short description such as "TLSv1_3 TLS13_AES_256_GCM_SHA384".
pub fn open(
    endpoint: &Endpoint,
    deadline: Instant,
    tls: &TlsSettings,
) -> Result<(Stream, Option<String>)> {
    let socket = endpoint.connect(deadline)?;
    if !endpoint.tls {
        let _ = tls;
        return Ok((Stream::Plain(socket), None));
    }
    #[cfg(feature = "tls")]
    {
        let name = rustls_pki_types::ServerName::try_from(endpoint.host.clone())
            .context("invalid TLS server name")?;
        let mut connection = rustls::ClientConnection::new(tls.client_config()?, name)
            .context("start TLS session")?;
        let mut socket = socket;
        socket.set_read_timeout(Some(remaining(deadline)?))?;
        socket.set_write_timeout(Some(remaining(deadline)?))?;
        while connection.is_handshaking() {
            connection
                .complete_io(&mut socket)
                .with_context(|| format!("TLS handshake with {}", endpoint.host))?;
        }
        let description = format!(
            "{:?} {:?}",
            connection.protocol_version().context("TLS version")?,
            connection
                .negotiated_cipher_suite()
                .context("TLS cipher")?
                .suite()
        );
        let stream = rustls::StreamOwned::new(connection, socket);
        Ok((Stream::Tls(Box::new(stream)), Some(description)))
    }
    #[cfg(not(feature = "tls"))]
    {
        let _ = (socket, deadline);
        bail!("this build has no TLS support; rebuild with the `tls` feature")
    }
}
