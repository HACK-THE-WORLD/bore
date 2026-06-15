# bore

A reverse SOCKS5 proxy built from the original `bore` tunnel. It lets a remote server expose a SOCKS5 port while outbound network connections are normally made from the client machine's network environment.

This is useful when you want tools on the server side to browse through the client's network, instead of forwarding one fixed local port.

## Usage

Run the server on the public machine:

```shell
bore server --socks-port 1080 --secret my_secret_string
```

If you also want to protect the public SOCKS5 entrance itself, add username/password authentication:

```shell
bore server --socks-port 1080 --secret my_secret_string --socks-username user --socks-password pass
```

Run the client inside the network environment you want to reuse:

```shell
bore local --to <SERVER_ADDRESS> --secret my_secret_string
```

Then configure applications to use the server as a SOCKS5 proxy:

```shell
curl --socks5-hostname <SERVER_ADDRESS>:1080 https://example.com
```

With SOCKS5 username/password enabled:

```shell
curl --proxy socks5h://user:pass@<SERVER_ADDRESS>:1080 https://example.com
```

The destination connection is created by the client machine. DNS names are also sent through the SOCKS5 request when the application uses SOCKS5 hostname mode, such as `curl --socks5-hostname`.

If the client itself must reach the server through a proxy, pass `--proxy <URL>` or set `BORE_PROXY`:

```shell
# HTTP / HTTPS CONNECT proxy
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy http://127.0.0.1:8080
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy https://proxy.example.com:443

# SOCKS5 proxy (recommended for forwarding WebSocket / long-lived traffic)
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy socks5://proxy.example.com:1080
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy socks5h://user:pass@proxy.example.com:1080
```

Supported schemes:

- `http://host:port` and `https://host:port` use HTTP `CONNECT`. No proxy authentication.
- `socks5://[user:pass@]host:port` resolves the destination DNS locally.
- `socks5h://[user:pass@]host:port` defers DNS resolution to the SOCKS5 proxy. This is preferred when the client cannot resolve the bore server's hostname directly.

`--proxy` is used both for the long-lived control connection and for the per-request data connections, so the client only needs outbound access through the proxy. SOCKS5 is recommended if you forward WebSocket or other long-lived traffic, because HTTP CONNECT proxies often impose aggressive idle timeouts on tunnels.

`--proxy` only affects the client's connection to the bore server control port. Users of the remote SOCKS5 proxy still connect to `<SERVER_ADDRESS>:1080`.

If some targets should stay on the server side instead of using the client egress, set `--client-blacklist` on `bore server`.

Examples:

```shell
# Comma-separated wildcard patterns
bore server --socks-port 1080 --client-blacklist "*.corp.internal,10.0.*.*,localhost"

# Read patterns from a file
bore server --socks-port 1080 --client-blacklist @./client-blacklist.txt
```

The blacklist matches the requested SOCKS5 host before dispatching to a client:

- Exact IPs or hostnames such as `127.0.0.1` or `api.example.com`
- Wildcards such as `*.example.com`, `10.0.*.*`, or `db-??.corp`
- A comma-separated string, an existing file path, or `@path/to/file`

Blacklist files may use commas or newlines between entries. Empty lines and lines starting with `#` are ignored.

## Commands

### Client

```shell
bore local --to <SERVER_ADDRESS> [OPTIONS]
```

Options:

```shell
  -t, --to <TO>          Address of the remote server [env: BORE_SERVER=]
  -s, --secret <SECRET>  Optional secret for authentication [env: BORE_SECRET]
      --proxy <PROXY>    Optional HTTP(S) proxy URL for connecting to the remote server [env: BORE_PROXY=]
  -h, --help             Print help
```

### Server

```shell
bore server [OPTIONS]
```

Options:

```shell
      --socks-port <SOCKS_PORT>  TCP port where the SOCKS5 proxy listens [env: BORE_SOCKS_PORT=] [default: 1080]
  -s, --secret <SECRET>          Optional secret for authentication [env: BORE_SECRET]
        --socks-username <SOCKS_USERNAME>
                     Optional username required by the public SOCKS5 listener [env: BORE_SOCKS_USERNAME=]
        --socks-password <SOCKS_PASSWORD>
                     Optional password required by the public SOCKS5 listener [env: BORE_SOCKS_PASSWORD=]
      --bind-addr <BIND_ADDR>    IP address for the control server [default: 0.0.0.0]
      --bind-socks <BIND_SOCKS>  IP address where the SOCKS5 proxy listens, defaults to --bind-addr
      --client-blacklist <CLIENT_BLACKLIST>
                                 Targets that bypass client egress and are connected directly from the server [env: BORE_CLIENT_BLACKLIST=]
  -h, --help                     Print help
```

The control server listens on TCP port `7835`. The SOCKS5 proxy listens on `--socks-port`.

## Protocol

The client keeps a control connection to the server on port `7835`. The server exposes a SOCKS5 listener. For each SOCKS5 `CONNECT` request, the server either connects directly to the target itself when `--client-blacklist` matches, or sends the requested target host and port to the client over the control connection. In the client-egress case, the client opens a new connection back to the control server, accepts that request ID, dials the target from the client's network, and the two streams are copied bidirectionally.

### Multiple clients

Multiple bore clients can register against the same server. Incoming SOCKS5 requests are dispatched across registered clients in round-robin order; if a client disconnects mid-flight its slot is removed automatically. If no clients are connected, requests are queued (up to 1024) and dispatched as soon as a client comes online.

### Long-lived connections

The control connection has bidirectional 15-second heartbeats and a 45-second idle timeout, so half-open connections (silent NAT timeouts, network black holes) are detected within a minute. Data connections enable `TCP_NODELAY` and TCP keepalive (30s idle / 10s probe) to keep WebSocket and other interactive long-lived flows from being silently reset by intermediate gateways.

### Reconnect

The client automatically reconnects to the server with exponential backoff (1s, 2s, 4s, …, capped at 30s). In-flight data connections are independent and continue to function across control-connection reconnects.

## Authentication

On a custom deployment, use `--secret` to require clients to authenticate. The protocol verifies possession of the secret on every control connection by answering random HMAC challenges.

If the public SOCKS5 endpoint should not be anonymous, set `--socks-username` and `--socks-password`. This enables the SOCKS5 username/password method defined by RFC 1929.

The proxied TCP traffic is not encrypted by this tool unless the application protocol itself uses encryption, such as HTTPS or SSH.

## Notes

This implements a TCP SOCKS5 proxy, not a full layer-3 VPN. Applications must support SOCKS5 directly or be wrapped with a tool that can route TCP traffic through a SOCKS5 proxy. UDP ASSOCIATE and BIND are not supported.

## License

Licensed under the [MIT license](LICENSE).
