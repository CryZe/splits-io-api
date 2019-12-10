use bytes::buf::BufExt;
use http::{header, Request, Response, StatusCode};
use hyper_tls::HttpsConnector;
use snafu::ResultExt;
use std::{io::Read, ops::Deref};
use tokio_tungstenite::{tungstenite::protocol::Role, WebSocketStream};

pub use hyper::{Body, Error};

pub type WsStream = WebSocketStream<hyper::upgrade::Upgraded>;
pub use tokio_tungstenite::tungstenite::{Error as WsError, Message};

pub async fn recv_bytes(body: Body) -> Result<impl Deref<Target = [u8]>, Error> {
    hyper::body::to_bytes(body).await
}

pub async fn recv_reader(body: Body) -> Result<impl Read, Error> {
    Ok(hyper::body::aggregate(body).await?.reader())
}

pub struct Client {
    client: hyper::Client<HttpsConnector<hyper::client::HttpConnector>>,
}

impl Client {
    pub fn new() -> Self {
        let https = HttpsConnector::new();
        let client = hyper::Client::builder().build::<_, hyper::Body>(https);
        Self { client }
    }

    pub async fn request(&self, request: Request<Body>) -> Result<Response<Body>, Error> {
        self.client.request(request).await
    }

    pub async fn connect_ws(&self, uri: &str) -> Result<WsStream, crate::Error> {
        let request = Request::get(uri)
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "TotallyRandomBytesHere==")
            .body(Body::empty())
            .unwrap();

        let response = self.request(request).await.context(crate::Download)?;

        let status = response.status();
        if status != StatusCode::SWITCHING_PROTOCOLS {
            return Err(crate::Error::Status { status });
        }

        let upgraded = response
            .into_body()
            .on_upgrade()
            .await
            .context(crate::Download)?;

        Ok(WebSocketStream::from_raw_socket(upgraded, Role::Client, None).await)
    }
}
