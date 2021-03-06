#![feature(async_await)]

use futures::prelude::*;
use hyper::{http::header::AUTHORIZATION, Body, Request, Response, StatusCode};
use hyper_tls::HttpsConnector;
use snafu::ResultExt;

pub mod category;
pub mod game;
pub mod race;
pub mod run;
pub mod runner;
mod schema;
mod wrapper;
pub use schema::*;

pub use uuid;

pub struct Client {
    client: hyper::Client<HttpsConnector<hyper::client::HttpConnector>>,
    access_token: Option<String>,
}

impl Client {
    pub fn new() -> Self {
        let https = HttpsConnector::new(4).unwrap();
        let client = hyper::Client::builder().build::<_, hyper::Body>(https);
        Client {
            client,
            access_token: None,
        }
    }

    pub fn set_access_token(&mut self, access_token: &str) {
        let buf = self.access_token.get_or_insert_with(String::new);
        buf.clear();
        buf.push_str("Bearer ");
        buf.push_str(access_token);
    }
}

#[derive(Debug, snafu::Snafu)]
pub enum Error {
    #[snafu(display("HTTP Status Code: {}", status.canonical_reason().unwrap_or_else(|| status.as_str())))]
    Status { status: StatusCode },
    #[snafu(display("{}", message))]
    Api {
        status: StatusCode,
        message: Box<str>,
    },
    /// Failed downloading the response.
    Download { source: hyper::Error },
    /// Failed to parse the response.
    Json { source: serde_json::Error },
}

async fn get_response(
    client: &Client,
    mut request: Request<Body>,
) -> Result<Response<Body>, Error> {
    // TODO: Only for requests that need it.
    if let Some(access_token) = &client.access_token {
        if let Ok(access_token) = access_token.parse() {
            // TODO: Don't ignore error.
            request.headers_mut().insert(AUTHORIZATION, access_token);
        }
    }

    let response = client.client.request(request).await.context(Download)?;
    let status = response.status();
    if !status.is_success() {
        if let Ok(buf) = response.into_body().try_concat().await {
            if let Ok(error) = serde_json::from_reader::<_, ApiError>(&*buf) {
                return Err(Error::Api {
                    status,
                    message: error.error,
                });
            }
        }
        return Err(Error::Status { status });
    }
    Ok(response)
}

async fn get_json<T: serde::de::DeserializeOwned>(
    client: &Client,
    request: Request<Body>,
) -> Result<T, Error> {
    let response = get_response(client, request).await?;
    let buf = response.into_body().try_concat().await.context(Download)?;
    println!("{}", String::from_utf8_lossy(&*buf));
    serde_json::from_reader(&*buf).context(Json)
}

#[derive(serde::Deserialize)]
struct ApiError {
    #[serde(alias = "message")]
    error: Box<str>,
}
