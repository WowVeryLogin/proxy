# Simple proxy server in Rust

Just a simple proxy which connects with CONNECT HTTP method and then works as a tunnel.

```bash
RUST_LOG=debug cargo run
curl -v -x http://127.0.0.1:8080 https://google.com
```
