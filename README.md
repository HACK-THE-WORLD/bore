# bore

A reverse SOCKS5 proxy built from the original `bore` tunnel. It lets a remote server expose a SOCKS5 port while all outbound network connections are made from the client machine's network environment.

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

If the client itself must reach the server through an HTTP or HTTPS proxy, pass `--proxy <URL>` or set `BORE_PROXY`:

```shell
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy http://127.0.0.1:8080
bore local --to <SERVER_ADDRESS> --secret my_secret_string --proxy https://proxy.example.com:443
```

`--proxy` only affects the client's connection to the bore server control port. Users of the remote SOCKS5 proxy still connect to `<SERVER_ADDRESS>:1080`.

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
  -h, --help                     Print help
```

The control server listens on TCP port `7835`. The SOCKS5 proxy listens on `--socks-port`.

## Protocol

The client keeps a control connection to the server on port `7835`. The server exposes a SOCKS5 listener. For each SOCKS5 `CONNECT` request, the server sends the requested target host and port to the client over the control connection. The client opens a new connection back to the control server, accepts that request ID, dials the target from the client's network, and the two streams are copied bidirectionally.

## Authentication

On a custom deployment, use `--secret` to require clients to authenticate. The protocol verifies possession of the secret on every control connection by answering random HMAC challenges.

If the public SOCKS5 endpoint should not be anonymous, set `--socks-username` and `--socks-password`. This enables the SOCKS5 username/password method defined by RFC 1929.

The proxied TCP traffic is not encrypted by this tool unless the application protocol itself uses encryption, such as HTTPS or SSH.

## Notes

This implements a TCP SOCKS5 proxy, not a full layer-3 VPN. Applications must support SOCKS5 directly or be wrapped with a tool that can route TCP traffic through a SOCKS5 proxy. UDP ASSOCIATE and BIND are not supported.

## License

Licensed under the [MIT license](LICENSE).
