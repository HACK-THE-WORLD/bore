//! Client implementation for the `bore` service.

use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};
use tokio_native_tls::TlsConnector;
use tracing::{error, info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    ClientMessage, Delimited, ServerMessage, TargetAddr, CONTROL_PORT, NETWORK_TIMEOUT,
};

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> AsyncStream for T {}

type RemoteStream = Pin<Box<dyn AsyncStream>>;

#[derive(Clone, Debug)]
enum ProxyScheme {
    Http,
    Https,
}

/// Configuration for an HTTP(S) CONNECT proxy used by the client.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    scheme: ProxyScheme,
    host: String,
    port: u16,
}

impl ProxyConfig {
    /// Parse a proxy URL such as `http://127.0.0.1:8080` or `https://proxy:443`.
    pub fn parse(url: &str) -> Result<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("http://") {
            (ProxyScheme::Http, rest)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (ProxyScheme::Https, rest)
        } else {
            bail!("proxy URL must start with http:// or https://");
        };

        let authority = rest
            .split_once('/')
            .map_or(rest, |(authority, _)| authority);
        if authority.is_empty() {
            bail!("proxy URL is missing a host");
        }
        if authority.contains('@') {
            bail!("proxy URLs with credentials are not supported");
        }

        let default_port = match scheme {
            ProxyScheme::Http => 80,
            ProxyScheme::Https => 443,
        };
        let (host, port) = parse_host_port(authority, default_port)?;
        Ok(Self { scheme, host, port })
    }
}

/// State structure for the client.
pub struct Client {
    /// Control connection to the server.
    conn: Option<Delimited<RemoteStream>>,

    /// Destination address of the server.
    to: String,

    /// Port that is publicly available on the remote.
    remote_port: u16,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,

    /// Optional HTTP(S) proxy used for remote server connections.
    proxy: Option<ProxyConfig>,
}

impl Client {
    /// Create a new client.
    pub async fn new(to: &str, secret: Option<&str>, proxy: Option<ProxyConfig>) -> Result<Self> {
        let mut stream = Delimited::new(connect_remote(to, CONTROL_PORT, proxy.as_ref()).await?);
        let auth = secret.map(Authenticator::new);
        if let Some(auth) = &auth {
            auth.client_handshake(&mut stream).await?;
        }

        stream.send(ClientMessage::Hello).await?;
        let remote_port = match stream.recv_timeout().await? {
            Some(ServerMessage::Hello(remote_port)) => remote_port,
            Some(ServerMessage::Error(message)) => bail!("server error: {message}"),
            Some(ServerMessage::Challenge(_)) => {
                bail!("server requires authentication, but no client secret was provided");
            }
            Some(_) => bail!("unexpected initial non-hello message"),
            None => bail!("unexpected EOF"),
        };
        info!(remote_port, "connected to server");
        info!("remote SOCKS5 proxy listening at {to}:{remote_port}");

        Ok(Client {
            conn: Some(stream),
            to: to.to_string(),
            remote_port,
            auth,
            proxy,
        })
    }

    /// Returns the SOCKS5 port publicly available on the remote.
    pub fn remote_port(&self) -> u16 {
        self.remote_port
    }

    /// Start the client, serving remote SOCKS5 requests.
    pub async fn listen(mut self) -> Result<()> {
        let mut conn = self.conn.take().unwrap();
        let this = Arc::new(self);
        loop {
            match conn.recv().await? {
                Some(ServerMessage::Hello(_)) => warn!("unexpected hello"),
                Some(ServerMessage::Challenge(_)) => warn!("unexpected challenge"),
                Some(ServerMessage::Heartbeat) => (),
                Some(ServerMessage::Connection { id, target }) => {
                    let this = Arc::clone(&this);
                    tokio::spawn(
                        async move {
                            info!(%target.host, target.port, "new network request");
                            match this.handle_connection(id, target).await {
                                Ok(_) => info!("connection exited"),
                                Err(err) => warn!(%err, "connection exited with error"),
                            }
                        }
                        .instrument(info_span!("proxy", %id)),
                    );
                }
                Some(ServerMessage::Error(err)) => error!(%err, "server error"),
                None => return Ok(()),
            }
        }
    }

    async fn handle_connection(&self, id: Uuid, target: TargetAddr) -> Result<()> {
        let mut remote_conn =
            Delimited::new(connect_remote(&self.to[..], CONTROL_PORT, self.proxy.as_ref()).await?);
        if let Some(auth) = &self.auth {
            auth.client_handshake(&mut remote_conn).await?;
        }
        remote_conn.send(ClientMessage::Accept(id)).await?;

        let mut target_conn = connect_tcp(&target.host, target.port).await?;
        let mut parts = remote_conn.into_parts();
        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
        target_conn.write_all(&parts.read_buf).await?;
        tokio::io::copy_bidirectional(&mut target_conn, &mut parts.io).await?;
        Ok(())
    }
}

async fn connect_tcp(to: &str, port: u16) -> Result<TcpStream> {
    match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))
}

async fn connect_remote(to: &str, port: u16, proxy: Option<&ProxyConfig>) -> Result<RemoteStream> {
    match proxy {
        Some(proxy) => connect_via_proxy(to, port, proxy).await,
        None => Ok(Box::pin(connect_tcp(to, port).await?)),
    }
}

async fn connect_via_proxy(to: &str, port: u16, proxy: &ProxyConfig) -> Result<RemoteStream> {
    timeout(NETWORK_TIMEOUT, async {
        let tcp = TcpStream::connect((proxy.host.as_str(), proxy.port))
            .await
            .with_context(|| format!("could not connect to proxy {}:{}", proxy.host, proxy.port))?;

        let mut stream: RemoteStream = match proxy.scheme {
            ProxyScheme::Http => Box::pin(tcp),
            ProxyScheme::Https => {
                let connector = native_tls::TlsConnector::new()?;
                let connector = TlsConnector::from(connector);
                Box::pin(connector.connect(&proxy.host, tcp).await?)
            }
        };

        let target = format!("{}:{}", http_authority_host(to), port);
        let request = format!(
            "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await?;

        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while response.len() < 8192 {
            stream.read_exact(&mut byte).await?;
            response.push(byte[0]);
            if response.ends_with(b"\r\n\r\n") {
                let status_line = response
                    .split(|b| *b == b'\n')
                    .next()
                    .context("proxy response is empty")?;
                let status_line = String::from_utf8_lossy(status_line);
                if status_line.starts_with("HTTP/1.1 200")
                    || status_line.starts_with("HTTP/1.0 200")
                {
                    return Ok(stream);
                }
                bail!("proxy CONNECT failed: {}", status_line.trim_end());
            }
        }

        bail!("proxy CONNECT response exceeded 8192 bytes")
    })
    .await
    .context("timed out connecting through proxy")?
}

fn parse_host_port(authority: &str, default_port: u16) -> Result<(String, u16)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, rest) = rest
            .split_once(']')
            .context("invalid bracketed IPv6 proxy host")?;
        let port = if let Some(port) = rest.strip_prefix(':') {
            port.parse().context("invalid proxy port")?
        } else if rest.is_empty() {
            default_port
        } else {
            bail!("invalid proxy URL authority");
        };
        return Ok((host.to_string(), port));
    }

    if authority.matches(':').count() > 1 {
        return Ok((authority.to_string(), default_port));
    }

    let (host, port) = if let Some((host, port)) = authority.rsplit_once(':') {
        (host, port.parse().context("invalid proxy port")?)
    } else {
        (authority, default_port)
    };
    if host.is_empty() {
        bail!("proxy URL is missing a host");
    }
    Ok((host.to_string(), port))
}

fn http_authority_host(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}
