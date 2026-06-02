//! Server implementation for the `bore` service.

use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use dashmap::DashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, Mutex};
use tokio::time::timeout;
use tracing::{info, info_span, warn, Instrument};
use uuid::Uuid;

use crate::auth::Authenticator;
use crate::shared::{
    tune_tcp_stream, ClientMessage, Delimited, ServerMessage, TargetAddr, CONTROL_PORT,
    HEARTBEAT_INTERVAL, HEARTBEAT_TIMEOUT,
};

/// Maximum number of SOCKS5 requests that can be queued while no client is
/// registered. Beyond this, new requests are rejected immediately.
const MAX_PENDING_REQUESTS: usize = 1024;

/// How long an unaccepted SOCKS5 request stays in `conns` before it is dropped.
const DEFAULT_PENDING_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

struct PendingConnection {
    stream: TcpStream,
    /// Time the request was accepted. Used to expire stale entries.
    queued_at: Instant,
}

#[derive(Clone)]
struct SocksAuth {
    username: String,
    password: String,
}

type ClientRequestTx = mpsc::UnboundedSender<(Uuid, TargetAddr)>;

/// Information about a currently registered egress client.
struct RegisteredClient {
    tx: ClientRequestTx,
    /// Human-readable address for logging.
    addr: String,
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

    /// All currently registered network egress clients, keyed by client id.
    clients: Arc<DashMap<Uuid, RegisteredClient>>,

    /// Round-robin counter used to dispatch requests across clients.
    rr_counter: Arc<AtomicUsize>,

    /// Queue of SOCKS5 requests waiting for a registered client.
    pending: Arc<Mutex<VecDeque<(Uuid, TargetAddr)>>>,

    /// Maximum time a SOCKS5 request may wait for an egress client.
    pending_request_timeout: Duration,

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
            clients: Arc::new(DashMap::new()),
            rr_counter: Arc::new(AtomicUsize::new(0)),
            pending: Arc::new(Mutex::new(VecDeque::new())),
            pending_request_timeout: DEFAULT_PENDING_REQUEST_TIMEOUT,
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

    /// Set how long a SOCKS5 request may wait for an egress client.
    pub fn set_pending_request_timeout(&mut self, pending_request_timeout: Duration) {
        self.pending_request_timeout = pending_request_timeout;
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

        // Periodic janitor: drop stale `conns` entries whose client never
        // returned an Accept in time.
        let janitor = Arc::clone(&this);
        tokio::spawn(async move {
            let mut tick =
                tokio::time::interval(janitor.pending_request_timeout.min(Duration::from_secs(2)));
            loop {
                tick.tick().await;
                janitor.expire_pending_requests().await;
            }
        });

        this.listen_control(control_listener, socks_port).await
    }

    async fn listen_control(self: Arc<Self>, listener: TcpListener, socks_port: u16) -> Result<()> {
        loop {
            let (stream, addr) = listener.accept().await?;
            tune_tcp_stream(&stream);
            let this = Arc::clone(&self);
            tokio::spawn(
                async move {
                    if let Err(err) = this
                        .handle_control_connection(stream, addr.to_string(), socks_port)
                        .await
                    {
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
            tune_tcp_stream(&stream);
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
        let queued_at = Instant::now();

        // Try to dispatch first; only insert into `conns` if a client accepts
        // ownership. This avoids leaking entries when no client is registered
        // and the queue is full.
        match self.dispatch_request(id, target.clone()).await {
            DispatchOutcome::Sent | DispatchOutcome::Queued => {
                self.conns
                    .insert(id, PendingConnection { stream, queued_at });
            }
            DispatchOutcome::Rejected => {
                // Best-effort: tell the SOCKS5 user that the request failed.
                let _ = write_socks_failure_late(&mut stream, 0x04).await;
                warn!("rejected SOCKS5 request: queue full and no available client");
            }
        }

        Ok(())
    }

    async fn dispatch_request(&self, id: Uuid, target: TargetAddr) -> DispatchOutcome {
        // Snapshot client ids (cheap; small map). Iterate in round-robin order.
        let ids: Vec<Uuid> = self.clients.iter().map(|e| *e.key()).collect();
        if !ids.is_empty() {
            let start = self.rr_counter.fetch_add(1, Ordering::Relaxed);
            for i in 0..ids.len() {
                let cid = ids[(start + i) % ids.len()];
                if let Some(client) = self.clients.get(&cid) {
                    if client.tx.send((id, target.clone())).is_ok() {
                        return DispatchOutcome::Sent;
                    }
                }
                // Stale entry: drop it and continue.
                self.clients.remove(&cid);
            }
        }

        // No live clients. Queue if there is room.
        let mut pending = self.pending.lock().await;
        if pending.len() >= MAX_PENDING_REQUESTS {
            return DispatchOutcome::Rejected;
        }
        pending.push_back((id, target));
        DispatchOutcome::Queued
    }

    async fn expire_pending_requests(&self) {
        let now = Instant::now();
        let expired_ids: Vec<Uuid> = self
            .conns
            .iter()
            .filter_map(|entry| {
                (now.duration_since(entry.value().queued_at) >= self.pending_request_timeout)
                    .then_some(*entry.key())
            })
            .collect();

        if expired_ids.is_empty() {
            return;
        }

        {
            let mut pending = self.pending.lock().await;
            pending.retain(|(id, _)| !expired_ids.contains(id));
        }

        for id in expired_ids {
            match self.conns.remove(&id) {
                Some((_, mut pending)) => {
                    warn!(%id, "timed out waiting for an egress client");
                    let _ = write_socks_failure_late(&mut pending.stream, 0x01).await;
                }
                None => warn!(%id, "stale SOCKS5 request already removed"),
            }
        }
    }

    async fn handle_control_connection(
        &self,
        stream: TcpStream,
        addr: String,
        socks_port: u16,
    ) -> Result<()> {
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
            Some(ClientMessage::Heartbeat) => warn!("unexpected heartbeat before hello"),
            Some(ClientMessage::Hello) => {
                let client_id = Uuid::new_v4();
                info!(%client_id, socks_port, "new network egress client");
                stream.send(ServerMessage::Hello(socks_port)).await?;
                let (tx, rx) = mpsc::unbounded_channel();
                self.clients.insert(
                    client_id,
                    RegisteredClient {
                        tx: tx.clone(),
                        addr: addr.clone(),
                    },
                );
                info!(%client_id, %addr, total = self.clients.len(), "client registered");

                // Drain any pending requests that arrived before any client
                // was connected.
                {
                    let mut pending = self.pending.lock().await;
                    while let Some(request) = pending.pop_front() {
                        if tx.send(request).is_err() {
                            break;
                        }
                    }
                }

                let result = self.run_registered_client(stream, rx).await;
                let removed = self.clients.remove(&client_id);
                let removed_addr = removed.map(|(_, c)| c.addr).unwrap_or_default();
                info!(%client_id, addr = %removed_addr, remaining = self.clients.len(), "client deregistered");
                result?;
            }
            Some(ClientMessage::Reject(id)) => {
                info!(%id, "rejecting SOCKS5 connection");
                match self.conns.remove(&id) {
                    Some((_, mut pending)) => {
                        let _ = write_socks_failure_late(&mut pending.stream, 0x01).await;
                    }
                    None => warn!(%id, "missing SOCKS5 connection"),
                }
            }
            Some(ClientMessage::Accept(id)) => {
                info!(%id, "forwarding SOCKS5 connection");
                match self.conns.remove(&id) {
                    Some((_, mut pending)) => {
                        write_socks_reply(&mut pending.stream, 0x00).await?;
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
        let mut heartbeat = tokio::time::interval(HEARTBEAT_INTERVAL);
        // Skip the initial immediate tick.
        heartbeat.tick().await;

        loop {
            tokio::select! {
                Some((id, target)) = pending_rx.recv() => {
                    if self.conns.contains_key(&id) {
                        if let Err(err) = stream.send(ServerMessage::Connection { id, target }).await {
                            warn!(%err, "failed to send connection request to client");
                            return Ok(());
                        }
                    }
                }
                msg = timeout(HEARTBEAT_TIMEOUT, stream.recv()) => {
                    match msg {
                        Ok(Ok(Some(ClientMessage::Heartbeat))) => {}
                        Ok(Ok(Some(_))) => {
                            warn!("unexpected message on registered client connection");
                        }
                        Ok(Ok(None)) => {
                            info!("client closed control connection");
                            return Ok(());
                        }
                        Ok(Err(err)) => {
                            warn!(%err, "control connection read error");
                            return Ok(());
                        }
                        Err(_) => {
                            warn!("client heartbeat timeout, dropping");
                            return Ok(());
                        }
                    }
                }
                _ = heartbeat.tick() => {
                    if stream.send(ServerMessage::Heartbeat).await.is_err() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

enum DispatchOutcome {
    /// Sent directly to a registered client.
    Sent,
    /// No client available; queued.
    Queued,
    /// Rejected because the queue is full.
    Rejected,
}

/// Write a SOCKS5 failure reply on a stream where the success reply was not
/// yet sent. This is best-effort: errors are ignored by the caller.
async fn write_socks_failure_late(stream: &mut TcpStream, status: u8) -> Result<()> {
    write_socks_reply(stream, status).await
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
    if constant_time_eq(username.as_bytes(), auth.username.as_bytes())
        && constant_time_eq(password.as_bytes(), auth.password.as_bytes())
    {
        stream.write_all(&[0x01, 0x00]).await?;
        Ok(())
    } else {
        stream.write_all(&[0x01, 0x01]).await?;
        bail!("invalid SOCKS5 username/password");
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
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
