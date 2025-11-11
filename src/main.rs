mod config;
mod handler;

use tokio::net::TcpListener;

#[tokio::main]
async fn main() {
    env_logger::init();

    let listener = TcpListener::bind(config::LISTEN_ADDR)
        .await
        .expect("failed to bind to address");

    log::info!("proxy server listening on {}", config::LISTEN_ADDR);

    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!("Received SIGINT/SIGTERM, shutting down...");
                return;
            },
            res = listener.accept() => {
                match res {
                    Ok((connection, client_addr)) => {
                        tokio::spawn(async move {
                            handler::handle_connection(connection, client_addr).await;
                        });
                    }
                    Err(err) => {
                        log::error!("failed to accept connection: {}", err);
                    }
                }
            }
        }
    }
}
