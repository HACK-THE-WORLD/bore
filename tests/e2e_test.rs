use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bore_cli::{client::Client, server::Server, shared::CONTROL_PORT};
use lazy_static::lazy_static;
use rstest::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    /// Guard to make sure that tests are run serially, not concurrently.
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

async fn spawn_server(secret: Option<&str>, socks_auth: Option<(&str, &str)>) {
    let mut server = Server::new(1080, secret);
    if let Some((username, password)) = socks_auth {
        server.set_socks_auth(username.to_string(), password.to_string());
    }
    tokio::spawn(server.listen());
    time::sleep(Duration::from_millis(50)).await;
}

async fn spawn_client(secret: Option<&str>) -> Result<()> {
    let client = Client::new("localhost", secret, None).await?;
    tokio::spawn(client.listen());
    Ok(())
}

async fn socks5_connect(
    proxy_addr: SocketAddr,
    target: SocketAddr,
    credentials: Option<(&str, &str)>,
) -> Result<TcpStream> {
    let mut stream = TcpStream::connect(proxy_addr).await?;
    match credentials {
        Some(_) => stream.write_all(&[0x05, 0x01, 0x02]).await?,
        None => stream.write_all(&[0x05, 0x01, 0x00]).await?,
    }
    let mut greeting = [0u8; 2];
    stream.read_exact(&mut greeting).await?;
    let expected_method = if credentials.is_some() { 0x02 } else { 0x00 };
    assert_eq!(greeting, [0x05, expected_method]);

    if let Some((username, password)) = credentials {
        let username = username.as_bytes();
        let password = password.as_bytes();
        stream.write_all(&[0x01, username.len() as u8]).await?;
        stream.write_all(username).await?;
        stream.write_all(&[password.len() as u8]).await?;
        stream.write_all(password).await?;

        let mut auth_status = [0u8; 2];
        stream.read_exact(&mut auth_status).await?;
        assert_eq!(auth_status, [0x01, 0x00]);
    }

    let mut request = vec![0x05, 0x01, 0x00, 0x01];
    match target.ip() {
        std::net::IpAddr::V4(addr) => request.extend_from_slice(&addr.octets()),
        std::net::IpAddr::V6(_) => return Err(anyhow!("test helper only supports IPv4")),
    }
    request.extend_from_slice(&target.port().to_be_bytes());
    stream.write_all(&request).await?;

    let mut response = [0u8; 10];
    stream.read_exact(&mut response).await?;
    assert_eq!(response[0], 0x05);
    assert_eq!(response[1], 0x00);
    Ok(stream)
}

#[rstest]
#[tokio::test]
async fn basic_proxy(#[values(None, Some(""), Some("abc"))] secret: Option<&str>) -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(secret, None).await;
    spawn_client(secret).await?;

    let listener = TcpListener::bind("localhost:0").await?;
    let target_addr = listener.local_addr()?;
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = [0u8; 11];
        stream.read_exact(&mut buf).await?;
        assert_eq!(&buf, b"hello world");

        stream.write_all(b"I can send a message too!").await?;
        anyhow::Ok(())
    });

    let proxy_addr = ([127, 0, 0, 1], 1080).into();
    let mut stream = socks5_connect(proxy_addr, target_addr, None).await?;
    stream.write_all(b"hello world").await?;

    let mut buf = [0u8; 25];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"I can send a message too!");

    Ok(())
}

#[rstest]
#[case(None, Some("my secret"))]
#[case(Some("my secret"), None)]
#[tokio::test]
async fn mismatched_secret(
    #[case] server_secret: Option<&str>,
    #[case] client_secret: Option<&str>,
) {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(server_secret, None).await;
    assert!(spawn_client(client_secret).await.is_err());
}

#[tokio::test]
async fn socks_authentication() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None, Some(("user", "pass"))).await;
    spawn_client(None).await?;

    let listener = TcpListener::bind("localhost:0").await?;
    let target_addr = listener.local_addr()?;
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await?;
        stream.write_all(b"ok").await?;
        anyhow::Ok(())
    });

    let mut stream = socks5_connect(
        ([127, 0, 0, 1], 1080).into(),
        target_addr,
        Some(("user", "pass")),
    )
    .await?;
    let mut buf = [0u8; 2];
    stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, b"ok");

    Ok(())
}

#[tokio::test]
async fn socks_authentication_rejects_missing_or_invalid_credentials() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None, Some(("user", "pass"))).await;
    spawn_client(None).await?;

    let mut no_auth = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, 1080)).await?;
    no_auth.write_all(&[0x05, 0x01, 0x00]).await?;
    let mut response = [0u8; 2];
    no_auth.read_exact(&mut response).await?;
    assert_eq!(response, [0x05, 0xff]);

    let mut wrong_auth = TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, 1080)).await?;
    wrong_auth.write_all(&[0x05, 0x01, 0x02]).await?;
    wrong_auth.read_exact(&mut response).await?;
    assert_eq!(response, [0x05, 0x02]);
    wrong_auth.write_all(&[0x01, 0x04]).await?;
    wrong_auth.write_all(b"user").await?;
    wrong_auth.write_all(&[0x05]).await?;
    wrong_auth.write_all(b"wrong").await?;
    wrong_auth.read_exact(&mut response).await?;
    assert_eq!(response, [0x01, 0x01]);

    Ok(())
}

#[tokio::test]
async fn invalid_address() -> Result<()> {
    async fn check_address(to: &str, use_secret: bool) -> Result<()> {
        match Client::new(to, use_secret.then_some("a secret"), None).await {
            Ok(_) => Err(anyhow!("expected error for {to}, use_secret={use_secret}")),
            Err(_) => Ok(()),
        }
    }
    tokio::try_join!(
        check_address("google.com", false),
        check_address("google.com", true),
        check_address("nonexistent.domain.for.demonstration", false),
        check_address("nonexistent.domain.for.demonstration", true),
        check_address("malformed !$uri$%", false),
        check_address("malformed !$uri$%", true),
    )?;
    Ok(())
}

#[tokio::test]
async fn very_long_frame() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None, None).await;
    let mut attacker = TcpStream::connect(("localhost", CONTROL_PORT)).await?;

    for _ in 0..10 {
        let result = attacker.write_all(&[42u8; 100000]).await;
        if result.is_err() {
            return Ok(());
        }
        time::sleep(Duration::from_millis(10)).await;
    }
    panic!("did not exit after a 1 MB frame");
}

#[tokio::test]
async fn half_closed_tcp_stream() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;

    spawn_server(None, None).await;
    spawn_client(None).await?;

    let listener = TcpListener::bind("localhost:0").await?;
    let target_addr = listener.local_addr()?;
    let (mut cli, (mut srv, _)) = tokio::try_join!(
        socks5_connect(([127, 0, 0, 1], 1080).into(), target_addr, None),
        async { Ok::<_, anyhow::Error>(listener.accept().await?) }
    )?;

    let mut buf = b"message before shutdown".to_vec();
    cli.write_all(&buf).await?;
    cli.shutdown().await?;

    srv.read_exact(&mut buf).await?;
    assert_eq!(buf, b"message before shutdown");
    assert_eq!(srv.read(&mut buf).await?, 0);

    let mut buf = b"hello from the other side!".to_vec();
    srv.write_all(&buf).await?;
    cli.read_exact(&mut buf).await?;
    assert_eq!(buf, b"hello from the other side!");

    Ok(())
}
