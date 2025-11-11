use crate::config::{
    self, CONNECT_BUFFER_SIZE, CONNECT_REGEX, COPY_BUFFER_SIZE, INITIAL_CONNECT_TIMEOUT_SEC,
    INITIAL_READ_TIMEOUT_SEC, MAX_CONNECT_LINE_LEN, PROXY_READ_TIMEOUT_SEC,
    PROXY_WRITE_TIMEOUT_SEC,
};
use bytes::BytesMut;
use std::{net::SocketAddr, str::Utf8Error};
use tokio::{
    io::{self, AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{Duration, sleep, timeout},
};

const GATEWAY_TIMEOUT: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\n\r\n";
const URI_TOO_LONG: &[u8] = b"HTTP/1.1 414 URI Too Long\r\n\r\n";
const BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\n\r\n";
const METHOD_NOT_ALLOWED: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nAllow: CONNECT\r\n\r\n";
const BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\n\r\n";
const OK: &[u8] = b"HTTP/1.1 200 OK\r\n\r\n";

#[derive(Debug)]
enum ParseError {
    Incomplete,
    InvalidUtf8(Utf8Error),
    InvalidMethod,
    MissingHost,
}

struct ConnectError {
    response: &'static [u8],
    log_err_msg: String,
}

pub async fn handle_connection(mut connection: TcpStream, client_addr: SocketAddr) {
    let mut buf = BytesMut::with_capacity(CONNECT_BUFFER_SIZE);

    let target_uri = match read_connect_request(&mut connection, &mut buf, client_addr).await {
        Ok(uri) => uri,
        Err(err) => {
            log::warn!("client: {}, error: {}", client_addr, err.log_err_msg);
            let _ = connection.write_all(err.response).await;
            return;
        }
    };

    let upstream = match connect_with_retry(&target_uri, client_addr).await {
        Ok(stream) => stream,
        Err(err) => {
            log::warn!("client: {}, error: {}", client_addr, err.log_err_msg);
            let _ = connection.write_all(err.response).await;
            return;
        }
    };

    let _ = connection.write_all(OK).await;

    let (mut upstream_read, mut upstream_write) = upstream.into_split();
    let (mut client_read, mut client_write) = connection.into_split();

    let server_to_client = copy_with_timeout(
        &mut upstream_read,
        &mut client_write,
        Duration::from_secs(PROXY_READ_TIMEOUT_SEC),
        Duration::from_secs(PROXY_WRITE_TIMEOUT_SEC),
    );
    let client_to_server = copy_with_timeout(
        &mut client_read,
        &mut upstream_write,
        Duration::from_secs(PROXY_READ_TIMEOUT_SEC),
        Duration::from_secs(PROXY_WRITE_TIMEOUT_SEC),
    );

    let _ = tokio::join!(server_to_client, client_to_server);
}

async fn copy_with_timeout<R, W>(
    reader: &mut R,
    writer: &mut W,
    read_timeout: Duration,
    write_timeout: Duration,
) -> io::Result<()>
where
    R: io::AsyncRead + Unpin,
    W: io::AsyncWrite + Unpin,
{
    let mut buf = [0u8; COPY_BUFFER_SIZE];

    loop {
        let n = match timeout(read_timeout, reader.read(&mut buf)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => n,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "read timeout")),
        };

        match timeout(write_timeout, writer.write_all(&buf[..n])).await {
            Ok(Ok(())) => continue,
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err(io::Error::new(io::ErrorKind::TimedOut, "write timeout")),
        }
    }

    Ok(())
}

async fn read_connect_request(
    connection: &mut TcpStream,
    buffer: &mut BytesMut,
    client_addr: SocketAddr,
) -> Result<String, ConnectError> {
    loop {
        timeout(
            Duration::from_secs(INITIAL_READ_TIMEOUT_SEC),
            connection.read_buf(buffer),
        )
        .await
        .map_err(|err| ConnectError {
            response: GATEWAY_TIMEOUT,
            log_err_msg: format!("read timeout during handshake: {}", err),
        })?
        .map_err(|err| ConnectError {
            response: GATEWAY_TIMEOUT,
            log_err_msg: format!("client disconnected before handshake: {}", err),
        })?;

        if buffer.len() >= MAX_CONNECT_LINE_LEN {
            return Err(ConnectError {
                response: URI_TOO_LONG,
                log_err_msg: "connection failed (URI too long)".to_owned(),
            });
        }

        match parse_connect_request(buffer, client_addr) {
            Ok(uri) => return Ok(uri),
            Err(ParseError::Incomplete) => continue,
            Err(ParseError::InvalidUtf8(e)) => {
                return Err(ConnectError {
                    response: BAD_REQUEST,
                    log_err_msg: format!("invalid UTF-8 in HTTP header: {}", e),
                });
            }
            Err(ParseError::InvalidMethod) => {
                return Err(ConnectError {
                    response: METHOD_NOT_ALLOWED,
                    log_err_msg: "client sent non-CONNECT request".to_owned(),
                });
            }
            Err(ParseError::MissingHost) => {
                return Err(ConnectError {
                    response: BAD_REQUEST,
                    log_err_msg: "client missing host in request".to_owned(),
                });
            }
        }
    }
}

fn parse_connect_request(buffer: &[u8], client_addr: SocketAddr) -> Result<String, ParseError> {
    let request_str = std::str::from_utf8(buffer).map_err(ParseError::InvalidUtf8)?;

    if !request_str.contains("HTTP/1.1\r\n") {
        return Err(ParseError::Incomplete);
    }

    let captures = CONNECT_REGEX
        .captures(request_str)
        .ok_or(ParseError::InvalidMethod)?;

    let uri = captures
        .get(1)
        .ok_or(ParseError::MissingHost)?
        .as_str()
        .to_string();

    log::debug!("parsed CONNECT request from {}: {}", client_addr, uri);
    Ok(uri)
}

async fn connect_with_retry(uri: &str, client_addr: SocketAddr) -> Result<TcpStream, ConnectError> {
    for attempt in 0..config::MAX_RETRY_ATTEMPTS {
        let err_msg = match timeout(
            Duration::from_secs(INITIAL_CONNECT_TIMEOUT_SEC),
            TcpStream::connect(uri),
        )
        .await
        {
            Ok(Ok(stream)) => {
                log::info!("connected {} -> {}", client_addr, uri);
                return Ok(stream);
            }
            Ok(Err(err)) => err.to_string(),
            Err(_) => "timeout".to_owned(),
        };

        log::warn!(
            "connection attempt {} failed for {} -> {}: {}",
            attempt + 1,
            client_addr,
            uri,
            err_msg,
        );

        sleep(Duration::from_millis(config::RETRY_DELAY_MS)).await;
    }

    Err(ConnectError {
        response: BAD_GATEWAY,
        log_err_msg: format!("all connection attempts failed for {}", uri),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    #[tokio::test]
    async fn test_handle_invalid_utf8_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((connection, client_addr)) = listener.accept().await {
                handle_connection(connection, client_addr).await;
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT \xFF\xFE HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut response = vec![0u8; 128];
        let n = client.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("400 Bad Request"));
    }

    #[tokio::test]
    async fn test_handle_invalid_method() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((connection, client_addr)) = listener.accept().await {
                handle_connection(connection, client_addr).await;
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"GET / HTTP/1.1\r\nHost: example.com\r\n\r\n")
            .await
            .unwrap();

        let mut response = vec![0u8; 128];
        let n = client.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("405 Method Not Allowed"));
    }

    #[tokio::test]
    async fn test_handle_uri_too_long() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((connection, client_addr)) = listener.accept().await {
                handle_connection(connection, client_addr).await;
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        let long_host = "a".repeat(2000);
        let request = format!("CONNECT {} HTTP/1.1\r\n\r\n", long_host);
        client.write_all(request.as_bytes()).await.unwrap();

        let mut response = vec![0u8; 128];
        let n = client.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("414 URI Too Long"));
    }

    #[tokio::test]
    async fn test_handle_connection_refused() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((connection, client_addr)) = listener.accept().await {
                handle_connection(connection, client_addr).await;
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(b"CONNECT 127.0.0.1:1 HTTP/1.1\r\n\r\n")
            .await
            .unwrap();

        let mut response = vec![0u8; 128];
        let n = client.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);

        assert!(response_str.contains("502 Bad Gateway"));
    }

    #[tokio::test]
    async fn test_happy_case() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = upstream.local_addr().unwrap();

        tokio::spawn(async move {
            if let Ok((mut connection, _)) = upstream.accept().await {
                println!("upstream connected");
                let _ = connection.write_all("hello from upstream".as_bytes()).await;
            }
        });

        tokio::spawn(async move {
            if let Ok((connection, client_addr)) = listener.accept().await {
                handle_connection(connection, client_addr).await;
            }
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client
            .write_all(format!("CONNECT {} HTTP/1.1\r\n\r\n", upstream_addr).as_bytes())
            .await
            .unwrap();

        let mut connect_response = vec![0u8; 19];
        client.read_exact(&mut connect_response).await.unwrap();

        let mut response = vec![0u8; 1024];
        let n = client.read(&mut response).await.unwrap();
        let response_str = String::from_utf8_lossy(&response[..n]);
        assert!(response_str.contains("hello from upstream"));
    }
}
