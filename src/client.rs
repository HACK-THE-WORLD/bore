//! Client implementation for the `bore` service.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};
use tokio_native_tls::TlsConnector;
use tracing::{error, info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    tune_tcp_stream, ClientMessage, Delimited, ServerMessage, TargetAddr, CONTROL_PORT,
    HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT, NETWORK_TIMEOUT,
};

trait AsyncStream: AsyncRead + AsyncWrite + Unpin + Send + Sync {}

impl<T: AsyncRead + AsyncWrite + Unpin + Send + Sync> AsyncStream for T {}

type RemoteStream = Pin<Box<dyn AsyncStream>>;

#[derive(Clone, Debug)]
enum ProxyScheme {
    Http,
    Https,
    /// SOCKS5 proxy. The boolean indicates whether DNS should be resolved by
    /// the proxy (`socks5h://`) or locally (`socks5://`).
    Socks5 {
        remote_dns: bool,
    },
}

/// Configuration for an HTTP(S) CONNECT or SOCKS5 proxy used by the client.
#[derive(Clone, Debug)]
pub struct ProxyConfig {
    scheme: ProxyScheme,
    host: String,
    port: u16,
    /// Optional credentials for the proxy. For SOCKS5 this enables RFC 1929
    /// username/password authentication. For HTTP proxies it is currently
    /// rejected during parsing (no Basic auth support yet).
    credentials: Option<(String, String)>,
}

impl ProxyConfig {
    /// Parse a proxy URL.
    ///
    /// Supported schemes:
    /// - `http://host:port` / `https://host:port`
    /// - `socks5://[user:pass@]host:port`
    /// - `socks5h://[user:pass@]host:port` (proxy resolves the destination DNS)
    pub fn parse(url: &str) -> Result<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("http://") {
            (ProxyScheme::Http, rest)
        } else if let Some(rest) = url.strip_prefix("https://") {
            (ProxyScheme::Https, rest)
        } else if let Some(rest) = url.strip_prefix("socks5://") {
            (ProxyScheme::Socks5 { remote_dns: false }, rest)
        } else if let Some(rest) = url.strip_prefix("socks5h://") {
            (ProxyScheme::Socks5 { remote_dns: true }, rest)
        } else {
            bail!("proxy URL must start with http://, https://, socks5:// or socks5h://");
        };

        let authority = rest
            .split_once('/')
            .map_or(rest, |(authority, _)| authority);
        if authority.is_empty() {
            bail!("proxy URL is missing a host");
        }

        let (credentials, authority) = match authority.rsplit_once('@') {
            Some((creds, host_port)) => {
                let (user, pass) = creds
                    .split_once(':')
                    .context("proxy credentials must be in user:password form")?;
                (
                    Some((percent_decode(user)?, percent_decode(pass)?)),
                    host_port,
                )
            }
            None => (None, authority),
        };

        if matches!(scheme, ProxyScheme::Http | ProxyScheme::Https) && credentials.is_some() {
            bail!("HTTP proxy authentication is not supported; use SOCKS5 instead");
        }

        let default_port = match scheme {
            ProxyScheme::Http => 80,
            ProxyScheme::Https => 443,
            ProxyScheme::Socks5 { .. } => 1080,
        };
        let (host, port) = parse_host_port(authority, default_port)?;
        Ok(Self {
            scheme,
            host,
            port,
            credentials,
        })
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

    /// Optional proxy used for remote server connections.
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
    ///
    /// This method runs the client until the underlying control connection
    /// terminates. Higher-level code is expected to wrap this in a reconnect
    /// loop (see [`run_with_reconnect`]).
    pub async fn listen(mut self) -> Result<()> {
        let mut conn = self.conn.take().unwrap();
        let this = Arc::new(self);
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        // Skip the first immediate tick.
        heartbeat.tick().await;
        loop {
            tokio::select! {
                msg = timeout(HEARTBEAT_TIMEOUT, conn.recv()) => {
                    let msg = msg.context("control connection idle timeout")??;
                    match msg {
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
                _ = heartbeat.tick() => {
                    if let Err(err) = conn.send(ClientMessage::Heartbeat).await {
                        warn!(%err, "failed to send heartbeat");
                        return Err(err);
                    }
                }
            }
        }
    }

    async fn handle_connection(&self, id: Uuid, target: TargetAddr) -> Result<()> {
        let mut remote_conn =
            Delimited::new(connect_remote(&self.to[..], CONTROL_PORT, self.proxy.as_ref()).await?);
        if let Some(auth) = &self.auth {
            auth.client_handshake(&mut remote_conn).await?;
        }
        let mut target_conn = match connect_tcp(&target.host, target.port).await {
            Ok(stream) => stream,
            Err(err) => {
                let _ = remote_conn.send(ClientMessage::Reject(id)).await;
                return Err(err);
            }
        };
        remote_conn.send(ClientMessage::Accept(id)).await?;

        let mut parts = remote_conn.into_parts();
        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
        target_conn.write_all(&parts.read_buf).await?;
        tokio::io::copy_bidirectional(&mut target_conn, &mut parts.io).await?;
        Ok(())
    }
}

/// Run the client and automatically reconnect with exponential backoff if the
/// control connection drops.
pub async fn run_with_reconnect(
    to: String,
    secret: Option<String>,
    proxy: Option<ProxyConfig>,
) -> Result<()> {
    let mut backoff = Duration::from_secs(1);
    let max_backoff = Duration::from_secs(30);
    loop {
        match Client::new(&to, secret.as_deref(), proxy.clone()).await {
            Ok(client) => {
                backoff = Duration::from_secs(1);
                match client.listen().await {
                    Ok(()) => {
                        warn!("control connection closed by server, reconnecting in 1s");
                        sleep(Duration::from_secs(1)).await;
                    }
                    Err(err) => {
                        warn!(%err, "control connection error, reconnecting in {:?}", backoff);
                        sleep(backoff).await;
                        backoff = (backoff * 2).min(max_backoff);
                    }
                }
            }
            Err(err) => {
                warn!(%err, "failed to connect to server, retrying in {:?}", backoff);
                sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        }
    }
}

async fn connect_tcp(to: &str, port: u16) -> Result<TcpStream> {
    let stream = match timeout(NETWORK_TIMEOUT, TcpStream::connect((to, port))).await {
        Ok(res) => res,
        Err(err) => Err(err.into()),
    }
    .with_context(|| format!("could not connect to {to}:{port}"))?;
    tune_tcp_stream(&stream);
    Ok(stream)
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
        tune_tcp_stream(&tcp);

        match &proxy.scheme {
            ProxyScheme::Http => {
                let mut stream: RemoteStream = Box::pin(tcp);
                http_connect(&mut stream, to, port).await?;
                Ok(stream)
            }
            ProxyScheme::Https => {
                let connector = native_tls::TlsConnector::new()?;
                let connector = TlsConnector::from(connector);
                let mut stream: RemoteStream = Box::pin(connector.connect(&proxy.host, tcp).await?);
                http_connect(&mut stream, to, port).await?;
                Ok(stream)
            }
            ProxyScheme::Socks5 { remote_dns } => {
                let mut stream: RemoteStream = Box::pin(tcp);
                socks5_connect(
                    &mut stream,
                    to,
                    port,
                    *remote_dns,
                    proxy.credentials.as_ref(),
                )
                .await?;
                Ok(stream)
            }
        }
    })
    .await
    .context("timed out connecting through proxy")?
}

async fn http_connect(stream: &mut RemoteStream, host: &str, port: u16) -> Result<()> {
    let target = format!("{}:{}", http_authority_host(host), port);
    let request = format!(
        "CONNECT {target} HTTP/1.1\r\nHost: {target}\r\nProxy-Connection: Keep-Alive\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;

    // Read the response status + headers up to "\r\n\r\n". We use a small
    // buffered loop with a hard cap to avoid slow-loris style abuse from a
    // malicious proxy.
    let mut buf = [0u8; 512];
    let mut response = Vec::with_capacity(512);
    loop {
        if response.len() >= 8192 {
            bail!("proxy CONNECT response exceeded 8192 bytes");
        }
        let n = stream.read(&mut buf).await?;
        if n == 0 {
            bail!("proxy closed connection during CONNECT");
        }
        response.extend_from_slice(&buf[..n]);
        if let Some(end) = find_double_crlf(&response) {
            // We must not consume bytes beyond the headers, but in practice a
            // well-behaved proxy will not send body bytes for CONNECT. If it
            // does, those bytes are part of the tunnel and we have a problem;
            // so far we treat that as an error.
            if response.len() > end + 4 {
                bail!("proxy returned data before tunnel was established");
            }
            let status_line = response
                .split(|b| *b == b'\n')
                .next()
                .context("proxy response is empty")?;
            let status_line = String::from_utf8_lossy(status_line);
            if status_line.starts_with("HTTP/1.1 200") || status_line.starts_with("HTTP/1.0 200") {
                return Ok(());
            }
            bail!("proxy CONNECT failed: {}", status_line.trim_end());
        }
    }
}

fn find_double_crlf(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

async fn socks5_connect(
    stream: &mut RemoteStream,
    host: &str,
    port: u16,
    remote_dns: bool,
    credentials: Option<&(String, String)>,
) -> Result<()> {
    // Greeting: VER=5, list of supported methods.
    if credentials.is_some() {
        stream.write_all(&[0x05, 0x02, 0x00, 0x02]).await?;
    } else {
        stream.write_all(&[0x05, 0x01, 0x00]).await?;
    }
    let mut reply = [0u8; 2];
    stream.read_exact(&mut reply).await?;
    if reply[0] != 0x05 {
        bail!("invalid SOCKS5 server reply");
    }
    match reply[1] {
        0x00 => {} // no auth required
        0x02 => {
            let (user, pass) = credentials
                .ok_or_else(|| anyhow::anyhow!("SOCKS5 proxy requires authentication"))?;
            if user.len() > 255 || pass.len() > 255 {
                bail!("SOCKS5 username/password too long");
            }
            let mut auth = Vec::with_capacity(3 + user.len() + pass.len());
            auth.push(0x01);
            auth.push(user.len() as u8);
            auth.extend_from_slice(user.as_bytes());
            auth.push(pass.len() as u8);
            auth.extend_from_slice(pass.as_bytes());
            stream.write_all(&auth).await?;
            let mut auth_reply = [0u8; 2];
            stream.read_exact(&mut auth_reply).await?;
            if auth_reply[0] != 0x01 || auth_reply[1] != 0x00 {
                bail!("SOCKS5 proxy authentication failed");
            }
        }
        0xff => bail!("SOCKS5 proxy rejected all offered auth methods"),
        other => bail!("SOCKS5 proxy selected unsupported method 0x{other:02x}"),
    }

    // CONNECT request.
    let mut req = vec![0x05, 0x01, 0x00];
    if let Ok(ip) = host.parse::<std::net::IpAddr>() {
        match ip {
            std::net::IpAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
        }
    } else if remote_dns {
        if host.len() > 255 {
            bail!("hostname too long for SOCKS5");
        }
        req.push(0x03);
        req.push(host.len() as u8);
        req.extend_from_slice(host.as_bytes());
    } else {
        // Resolve locally and send as IP.
        let addr = tokio::net::lookup_host((host, port))
            .await
            .with_context(|| format!("could not resolve {host}"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("no addresses for {host}"))?;
        match addr.ip() {
            std::net::IpAddr::V4(v4) => {
                req.push(0x01);
                req.extend_from_slice(&v4.octets());
            }
            std::net::IpAddr::V6(v6) => {
                req.push(0x04);
                req.extend_from_slice(&v6.octets());
            }
        }
    }
    req.extend_from_slice(&port.to_be_bytes());
    stream.write_all(&req).await?;

    // Read reply: VER, REP, RSV, ATYP, then variable BND.ADDR + BND.PORT.
    let mut head = [0u8; 4];
    stream.read_exact(&mut head).await?;
    if head[0] != 0x05 {
        bail!("invalid SOCKS5 reply version");
    }
    if head[1] != 0x00 {
        bail!("SOCKS5 CONNECT failed with status 0x{:02x}", head[1]);
    }
    match head[3] {
        0x01 => {
            let mut rest = [0u8; 4 + 2];
            stream.read_exact(&mut rest).await?;
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut rest = vec![0u8; len[0] as usize + 2];
            stream.read_exact(&mut rest).await?;
        }
        0x04 => {
            let mut rest = [0u8; 16 + 2];
            stream.read_exact(&mut rest).await?;
        }
        other => bail!("SOCKS5 reply has unsupported address type 0x{other:02x}"),
    }
    Ok(())
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

fn percent_decode(input: &str) -> Result<String> {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char)
                .to_digit(16)
                .context("invalid percent-encoding in proxy credentials")?;
            let lo = (bytes[i + 2] as char)
                .to_digit(16)
                .context("invalid percent-encoding in proxy credentials")?;
            out.push((hi * 16 + lo) as u8);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).context("proxy credentials are not valid UTF-8")
}
