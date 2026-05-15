use std::net::IpAddr;

use anyhow::{bail, Result};
use bore_cli::{
    client::{Client, ProxyConfig},
    server::Server,
    shared::DEFAULT_SOCKS_PORT,
};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Registers this machine as the network egress for the remote SOCKS5 proxy.
    Local {
        /// Address of the remote server that hosts the SOCKS5 proxy.
        #[clap(short, long, env = "BORE_SERVER")]
        to: String,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Optional HTTP(S) proxy URL for connecting to the remote server.
        #[clap(long, env = "BORE_PROXY")]
        proxy: Option<String>,
    },

    /// Runs the remote SOCKS5 proxy server.
    Server {
        /// TCP port where the SOCKS5 proxy listens.
        #[clap(long, default_value_t = DEFAULT_SOCKS_PORT, env = "BORE_SOCKS_PORT")]
        socks_port: u16,

        /// Optional secret for authentication.
        #[clap(short, long, env = "BORE_SECRET", hide_env_values = true)]
        secret: Option<String>,

        /// Optional username required by the public SOCKS5 listener.
        #[clap(long, env = "BORE_SOCKS_USERNAME")]
        socks_username: Option<String>,

        /// Optional password required by the public SOCKS5 listener.
        #[clap(long, env = "BORE_SOCKS_PASSWORD", hide_env_values = true)]
        socks_password: Option<String>,

        /// IP address to bind to, clients must reach this.
        #[clap(long, default_value = "0.0.0.0")]
        bind_addr: IpAddr,

        /// IP address where the SOCKS5 proxy will listen on, defaults to --bind-addr.
        #[clap(long)]
        bind_socks: Option<IpAddr>,
    },
}

#[tokio::main]
async fn run(command: Command) -> Result<()> {
    match command {
        Command::Local { to, secret, proxy } => {
            let proxy = proxy.as_deref().map(ProxyConfig::parse).transpose()?;
            let client = Client::new(&to, secret.as_deref(), proxy).await?;
            client.listen().await?;
        }
        Command::Server {
            socks_port,
            secret,
            socks_username,
            socks_password,
            bind_addr,
            bind_socks,
        } => {
            let mut server = Server::new(socks_port, secret.as_deref());
            match (socks_username, socks_password) {
                (Some(username), Some(password)) => server.set_socks_auth(username, password),
                (None, None) => (),
                _ => bail!("--socks-username and --socks-password must be provided together"),
            }
            server.set_bind_addr(bind_addr);
            server.set_bind_socks(bind_socks.unwrap_or(bind_addr));
            server.listen().await?;
        }
    }

    Ok(())
}

fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    run(Args::parse().command)
}
