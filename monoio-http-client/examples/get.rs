use bytes::Bytes;
use monoio_http::common::{body::{HttpBody, Body}, request::Request};
use monoio_http_client::{ClientTest, ClientGlobalConfig, ConnectionConfig, Proto};
use tracing_subscriber::FmtSubscriber;
use http::{request::Builder, Method, Version};

use monoio_http::h1::{
    codec::{decoder::FillPayload, ClientCodec},
    payload::{FixedPayload, Payload, PayloadError},
};

#[monoio::main(enable_timer = true)]
async fn main() {
    let client = ClientTest::<HttpBody>::new(ClientGlobalConfig::default(), ConnectionConfig::default());
    // let request = Request::builder()
    //     .uri("127.0.0.1:8080")
    //     .body(HttpBody::default())
    //     .unwrap();

    let body :HttpBody =  Payload::<Bytes, PayloadError>::None.into();

    let request = Builder::new()
        .method(Method::GET)
        .uri("google.com:80")
        .version(Version::HTTP_11)
        .header(http::header::HOST, "captive.apple.com")
        .header(http::header::ACCEPT, "*/*")
        .header(http::header::USER_AGENT, "monoio-http")
        .body(body)
        .unwrap();


    let subscriber = FmtSubscriber::builder()
    .with_max_level(tracing::Level::DEBUG) 
        .finish();
    // Initialize the tracing subscriber
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set up the tracing subscriber");

    tracing::debug!("starting request");
    let resp = client.send_request(request).await;
    let (parts, mut body) = resp.into_parts();
    println!("{:?}", parts);
    while let Ok(Some(data)) = body.get_data().await{
        println!("{:?}", data);
    }
    tracing::debug!("Its done");

    // let subscriber = FmtSubscriber::builder()
    // .with_max_level(tracing::Level::DEBUG) 
    //     .finish();
    // // Initialize the tracing subscriber
    // tracing::subscriber::set_global_default(subscriber)
    //     .expect("Failed to set up the tracing subscriber");


    // let h2_client = ClientTest::<HttpBody>::new(ClientGlobalConfig::default(), ConnectionConfig {proto: Proto::Http2});


    // for i in 0..2 {
    //     let body :HttpBody =  Payload::<Bytes, PayloadError>::None.into();

    //     let request = Builder::new()
    //         .method(Method::GET)
    //         // .uri("https://127.0.0.1:8080")
    //         .uri("https://127.0.0.1:9081")
    //         .version(Version::HTTP_2)
    //         .header(http::header::HOST, "captive.apple.com")
    //         .header(http::header::ACCEPT, "*/*")
    //         .header(http::header::USER_AGENT, "monoio-http")
    //         .body(body)
    //         .unwrap();

    //     tracing::debug!("starting request");

    //         let resp = h2_client.send_request(request).await;
    //         let (parts, mut body) = resp.into_parts();
    //         println!("{:?}", parts);
    //         while let Ok(Some(data)) = body.get_data().await{
    //             println!("{:?}", data);
    //         }
    // }

    // tracing::debug!("Its done");

}
