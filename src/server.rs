//! Server implementation for the `bore` service.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::{sleep, timeout};
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{ClientMessage, Delimited, ServerMessage, TargetAddr, CONTROL_PORT};

struct PendingConnection {
    stream: TcpStream,
}

#[derive(Clone)]
struct SocksAuth {
    username: String,
    password: String,
}

/// State structure for the server.
pub struct Server {
    /// TCP port where the SOCKS5 proxy will listen.
    socks_port: u16,

    /// Optional secret used to authenticate clients.
    auth: Option<Authenticator>,

    /// Optional username/password required by the public SOCKS5 listener.
    socks_auth: Option<SocksAuth>,

    /// Concurrent map of IDs to incoming SOCKS5 connections.
    conns: Arc<DashMap<Uuid, PendingConnection>>,

    /// Currently registered network egress client.
    client_tx: Arc<Mutex<Option<mpsc::UnboundedSender<(Uuid, TargetAddr)>>>>,

    /// Queue of SOCKS5 requests waiting for a registered client.
    pending: Arc<Mutex<VecDeque<(Uuid, TargetAddr)>>>,

    /// IP address where the control server will bind to.
    bind_addr: IpAddr,

    /// IP address where the SOCKS5 proxy will bind to.
    bind_socks: IpAddr,
}

impl Server {
    /// Create a new server with a SOCKS5 listener port.
    pub fn new(socks_port: u16, secret: Option<&str>) -> Self {
        Server {
            socks_port,
            conns: Arc::new(DashMap::new()),
            client_tx: Arc::new(Mutex::new(None)),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            auth: secret.map(Authenticator::new),
            socks_auth: None,
            bind_addr: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            bind_socks: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        }
    }

    /// Set the IP address where the control server will bind to.
    pub fn set_bind_addr(&mut self, bind_addr: IpAddr) {
        self.bind_addr = bind_addr;
    }

    /// Set the IP address where the SOCKS5 proxy will listen on.
    pub fn set_bind_socks(&mut self, bind_socks: IpAddr) {
        self.bind_socks = bind_socks;
    }

    /// Require username/password authentication on the public SOCKS5 listener.
    pub fn set_socks_auth(&mut self, username: String, password: String) {
        self.socks_auth = Some(SocksAuth { username, password });
    }

    /// Start the server, listening for clients and SOCKS5 users.
    pub async fn listen(self) -> Result<()> {
        let this = Arc::new(self);

        let control_listener = TcpListener::bind((this.bind_addr, CONTROL_PORT)).await?;
        let socks_listener = TcpListener::bind((this.bind_socks, this.socks_port)).await?;
        let socks_port = socks_listener.local_addr()?.port();
        info!(addr = ?this.bind_addr, port = CONTROL_PORT, "control server listening");
        info!(addr = ?this.bind_socks, port = socks_port, "SOCKS5 proxy listening");

        let socks_server = Arc::clone(&this);
        tokio::spawn(async move {
            if let Err(err) = socks_server.listen_socks(socks_listener).await {
                warn!(%err, "SOCKS5 listener exited");
            }
        });

        this.listen_control(control_listener, socks_port).await
    }

    async fn listen_control(self: Arc<Self>, listener: TcpListener, socks_port: u16) -> Result<()> {
        loop {
            let (stream, addr) = listener.accept().await?;
            let this = Arc::clone(&self);
            tokio::spawn(
                async move {
                    if let Err(err) = this.handle_control_connection(stream, socks_port).await {
                        warn!(%err, "control connection exited with error");
                    }
                }
                .instrument(info_span!("control", ?addr)),
            );
        }
    }

    async fn listen_socks(self: Arc<Self>, listener: TcpListener) -> Result<()> {
        loop {
            let (stream, addr) = listener.accept().await?;
            let this = Arc::clone(&self);
            tokio::spawn(
                async move {
                    if let Err(err) = this.handle_socks_connection(stream).await {
                        warn!(%err, "SOCKS5 connection exited with error");
                    }
                }
                .instrument(info_span!("socks", ?addr)),
            );
        }
    }

    async fn handle_socks_connection(&self, mut stream: TcpStream) -> Result<()> {
        let target = match socks5_handshake(&mut stream, self.socks_auth.as_ref()).await {
            Ok(target) => target,
            Err(err) => {
                warn!(%err, "SOCKS5 handshake failed");
                return Ok(());
            }
        };
        info!(%target.host, target.port, "new SOCKS5 request");

        let id = Uuid::new_v4();
        self.conns.insert(id, PendingConnection { stream });
        self.dispatch_request(id, target).await?;

        let conns = Arc::clone(&self.conns);
        tokio::spawn(async move {
            sleep(Duration::from_secs(10)).await;
            if conns.remove(&id).is_some() {
                warn!(%id, "removed stale SOCKS5 request");
            }
        });

        Ok(())
    }

    async fn dispatch_request(&self, id: Uuid, target: TargetAddr) -> Result<()> {
        let mut client_tx = self.client_tx.lock().await;
        if let Some(tx) = client_tx.as_ref() {
            if tx.send((id, target.clone())).is_ok() {
                return Ok(());
            }
            *client_tx = None;
        }
        self.pending.lock().await.push_back((id, target));
        Ok(())
    }

    async fn handle_control_connection(&self, stream: TcpStream, socks_port: u16) -> Result<()> {
        let mut stream = Delimited::new(stream);
        if let Some(auth) = &self.auth {
            if let Err(err) = auth.server_handshake(&mut stream).await {
                warn!(%err, "server handshake failed");
                stream.send(ServerMessage::Error(err.to_string())).await?;
                return Ok(());
            }
        }

        match stream.recv_timeout().await? {
            Some(ClientMessage::Authenticate(_)) => warn!("unexpected authenticate"),
            Some(ClientMessage::Hello) => {
                info!(socks_port, "new network egress client");
                stream.send(ServerMessage::Hello(socks_port)).await?;
                let (tx, rx) = mpsc::unbounded_channel();
                {
                    let mut client_tx = self.client_tx.lock().await;
                    *client_tx = Some(tx.clone());
                }
                {
                    let mut pending = self.pending.lock().await;
                    while let Some(request) = pending.pop_front() {
                        if tx.send(request).is_err() {
                            break;
                        }
                    }
                }
                self.run_registered_client(stream, rx).await?;
            }
            Some(ClientMessage::Accept(id)) => {
                info!(%id, "forwarding SOCKS5 connection");
                match self.conns.remove(&id) {
                    Some((_, mut pending)) => {
                        let mut parts = stream.into_parts();
                        debug_assert!(parts.write_buf.is_empty(), "framed write buffer not empty");
                        pending.stream.write_all(&parts.read_buf).await?;
                        tokio::io::copy_bidirectional(&mut parts.io, &mut pending.stream).await?;
                    }
                    None => warn!(%id, "missing SOCKS5 connection"),
                }
            }
            None => (),
        }
        Ok(())
    }

    async fn run_registered_client(
        &self,
        mut stream: Delimited<TcpStream>,
        mut pending_rx: mpsc::UnboundedReceiver<(Uuid, TargetAddr)>,
    ) -> Result<()> {
        loop {
            tokio::select! {
                Some((id, target)) = pending_rx.recv() => {
                    if self.conns.contains_key(&id) {
                        stream.send(ServerMessage::Connection { id, target }).await?;
                    }
                }
                _ = sleep(Duration::from_millis(500)) => {
                    if stream.send(ServerMessage::Heartbeat).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

async fn socks5_handshake(stream: &mut TcpStream, auth: Option<&SocksAuth>) -> Result<TargetAddr> {
    let mut header = [0u8; 2];
    timeout(Duration::from_secs(5), stream.read_exact(&mut header))
        .await
        .context("timed out reading SOCKS5 greeting")??;
    if header[0] != 0x05 || header[1] == 0 {
        bail!("invalid SOCKS5 greeting");
    }

    let mut methods = vec![0u8; header[1] as usize];
    stream.read_exact(&mut methods).await?;

    let selected_method = match auth {
        Some(_) if methods.contains(&0x02) => 0x02,
        Some(_) => {
            stream.write_all(&[0x05, 0xff]).await?;
            bail!("SOCKS5 client does not allow username/password auth");
        }
        None if methods.contains(&0x00) => 0x00,
        None => {
            stream.write_all(&[0x05, 0xff]).await?;
            bail!("SOCKS5 client does not allow no-auth mode");
        }
    };
    stream.write_all(&[0x05, selected_method]).await?;

    if let Some(auth) = auth {
        socks5_password_auth(stream, auth).await?;
    }

    let mut request = [0u8; 4];
    stream.read_exact(&mut request).await?;
    if request[0] != 0x05 || request[1] != 0x01 || request[2] != 0x00 {
        write_socks_reply(stream, 0x07).await?;
        bail!("unsupported SOCKS5 request");
    }

    let host = match request[3] {
        0x01 => {
            let mut addr = [0u8; 4];
            stream.read_exact(&mut addr).await?;
            IpAddr::V4(addr.into()).to_string()
        }
        0x03 => {
            let mut len = [0u8; 1];
            stream.read_exact(&mut len).await?;
            let mut domain = vec![0u8; len[0] as usize];
            stream.read_exact(&mut domain).await?;
            String::from_utf8(domain).context("SOCKS5 domain is not valid UTF-8")?
        }
        0x04 => {
            let mut addr = [0u8; 16];
            stream.read_exact(&mut addr).await?;
            IpAddr::V6(Ipv6Addr::from(addr)).to_string()
        }
        _ => {
            write_socks_reply(stream, 0x08).await?;
            bail!("unsupported SOCKS5 address type");
        }
    };

    let mut port = [0u8; 2];
    stream.read_exact(&mut port).await?;
    let port = u16::from_be_bytes(port);

    write_socks_reply(stream, 0x00).await?;
    Ok(TargetAddr { host, port })
}

async fn socks5_password_auth(stream: &mut TcpStream, auth: &SocksAuth) -> Result<()> {
    let mut version = [0u8; 1];
    stream.read_exact(&mut version).await?;
    if version[0] != 0x01 {
        stream.write_all(&[0x01, 0x01]).await?;
        bail!("invalid SOCKS5 username/password auth version");
    }

    let username = read_socks5_string(stream).await?;
    let password = read_socks5_string(stream).await?;
    if username == auth.username && password == auth.password {
        stream.write_all(&[0x01, 0x00]).await?;
        Ok(())
    } else {
        stream.write_all(&[0x01, 0x01]).await?;
        bail!("invalid SOCKS5 username/password");
    }
}

async fn read_socks5_string(stream: &mut TcpStream) -> Result<String> {
    let mut len = [0u8; 1];
    stream.read_exact(&mut len).await?;
    let mut bytes = vec![0u8; len[0] as usize];
    stream.read_exact(&mut bytes).await?;
    String::from_utf8(bytes).context("SOCKS5 auth field is not valid UTF-8")
}

async fn write_socks_reply(stream: &mut TcpStream, status: u8) -> Result<()> {
    stream
        .write_all(&[0x05, status, 0x00, 0x01, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}
