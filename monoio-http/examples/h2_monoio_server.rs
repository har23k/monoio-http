use std::error::Error;

use bytes::Bytes;
use http::Request;
use monoio::net::{TcpListener, TcpStream};
use monoio_http::h2::{
    server::{self, SendResponse},
    RecvStream,
};
use tracing_subscriber::FmtSubscriber;

#[monoio::main]
async fn main() {
    let listener = TcpListener::bind("127.0.0.1:9081").unwrap();
    println!("listening on {:?}", listener.local_addr());
    let subscriber = FmtSubscriber::builder()
    .with_max_level(tracing::Level::DEBUG) 
        .finish();
    // Initialize the tracing subscriber
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set up the tracing subscriber");


    loop {
        if let Ok((socket, _peer_addr)) = listener.accept().await {
            println!("TCP connection accept");
            monoio::spawn(async move {
                if let Err(e) = serve(socket).await {
                    println!("  -> err={:?}", e);
                }
            });
        }
    }
}

async fn serve(socket: TcpStream) -> Result<(), Box<dyn Error + Send + Sync>> {
    let mut connection = server::handshake(socket).await.expect("Handshake failed");

    while let Some(result) = connection.accept().await {
        let (request, respond) = result?;
        monoio::spawn(async move {
            if let Err(e) = handle_request(request, respond).await {
                println!("error while handling request: {}", e);
            }
        });
    }

    println!("~~~~~~~~~~~ H2 connection CLOSE !!!!!! ~~~~~~~~~~~");
    Ok(())
}

async fn handle_request(
    mut request: Request<RecvStream>,
    mut respond: SendResponse<Bytes>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    println!("GOT request: {:?}", request);

    let body = request.body_mut();
    while let Some(data) = body.data().await {
        let data = data?;
        println!("<<<< recv {:?}", data);
        let _ = body.flow_control().release_capacity(data.len());
    }

    println!("GOT request: {:?}", request);
    let response = http::Response::new(());
    let mut send = respond.send_response(response, false)?;
    println!(">>>> send");
    send.send_data(Bytes::from_static(b"hello "), false)?;
    send.send_data(Bytes::from_static(b"world\n"), true)?;

    Ok(())
}
