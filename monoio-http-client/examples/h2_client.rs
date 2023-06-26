use bytes::Bytes;
use http::{request::Builder, Method, Version};
use monoio_http::{common::{body::{HttpBody, Body, FixedBody}, request::Request}, h2::client};
use tracing_subscriber::FmtSubscriber;

#[monoio::main(enable_timer = true)]
async fn main() {
    let subscriber = FmtSubscriber::builder()
    .with_max_level(tracing::Level::DEBUG) 
        .finish();
    // Initialize the tracing subscriber
    tracing::subscriber::set_global_default(subscriber)
        .expect("Failed to set up the tracing subscriber");


    let h2_client = monoio_http_client::Builder::new().build_http2(); 

    for _ in 0..2 {
        let body = HttpBody::fixed_body(None) ;

        let request = Builder::new()
            .method(Method::GET)
            .uri("http://127.0.0.1:8080/")
            // .uri("https://http2.akamai.com/demo")
            .version(Version::HTTP_2)
            .body(body)
            .unwrap();

        tracing::debug!("starting request");

            let resp = h2_client.send_request(request).await.expect("Sending request");
            let (parts, mut body) = resp.into_parts();
            println!("{:?}", parts);
            while let Some(Ok(data)) = body.next_data().await{
                println!("{:?}", data);
            }
    }

    tracing::debug!("Its done");
}
