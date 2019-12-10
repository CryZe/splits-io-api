use futures_channel::{mpsc, oneshot};
use futures_util::{
    future::{select, Either, Future},
    sink::Sink,
    stream::Stream,
};
use http::{
    header::{HeaderName, HeaderValue},
    request::Parts,
    Request, Response, StatusCode,
};
use js_sys::{Array, Reflect, Uint8Array};
use snafu::OptionExt;
use std::{
    io::{Cursor, Read},
    ops::Deref,
    pin::Pin,
    task::{Context, Poll},
};
use wasm_bindgen::{prelude::*, JsCast};
use wasm_bindgen_futures::JsFuture;
use web_sys::{window, RequestInit, WebSocket};

#[derive(Debug, snafu::Snafu)]
pub enum WsError {
    /// Failed sending the message.
    #[snafu(display("Failed sending the message: {}", error.as_string().unwrap_or_default()))]
    Send { error: JsValue },
}

pub struct WsStream {
    ws: WebSocket,
    _on_close: Closure<dyn FnMut()>,
    _on_err: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut()>,
    rx_close: oneshot::Receiver<crate::Error>,
    rx_err: oneshot::Receiver<crate::Error>,
    rx_message: mpsc::UnboundedReceiver<Message>,
    connected: bool,
}

impl Drop for WsStream {
    fn drop(&mut self) {
        if self.connected {
            self.ws.set_onclose(None);
            self.ws.set_onmessage(None);
            self.ws.set_onerror(None);
            let _ = self.ws.close();
        }
    }
}

impl WsStream {
    // FIXME: Use ! for _msg
    pub async fn close(&mut self, _msg: Option<()>) -> Result<(), WsError> {
        if self.connected {
            self.ws.close();
            // TODO: Handle error
        }
        Ok(())
    }
}

impl Stream for WsStream {
    type Item = Result<Message, WsError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
        if !self.connected {
            return Poll::Ready(None);
        }

        if let Poll::Ready(Some(item)) = Pin::new(&mut self.rx_message).poll_next(cx) {
            return Poll::Ready(Some(Ok(item)));
        }

        if let Poll::Ready(item) = Pin::new(&mut self.rx_err).poll(cx) {
            self.connected = false;
            return Poll::Ready(Some(Err(item.unwrap())));
        }

        if let Poll::Ready(item) = Pin::new(&mut self.rx_close).poll(cx) {
            self.connected = false;
            return Poll::Ready(Some(item.unwrap()));
        }

        Poll::Pending
    }
}

impl Sink<Message> for WsStream {
    type Error = WsError;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(mut self: Pin<&mut Self>, item: Message) -> Result<(), Self::Error> {
        match item {
            Message::Text(text) => self
                .ws
                .send_with_str(&text)
                .map_err(|error| WsError::Send { error }),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
        // TODO: Do I really close the stream here?
        Poll::Ready(Ok(()))
    }
}

pub enum Message {
    Text(String),
}

pub struct Client;

#[derive(Debug, snafu::Snafu)]
pub enum Error {
    /// There is no global window object to be used.
    NoWindow,
    /// A forbidden header was used.
    #[snafu(display("A forbidden header was used: {}", error.as_string().unwrap_or_default()))]
    ForbiddenHeader { error: JsValue },
    /// Failed to receive the response.
    #[snafu(display("Failed to receive the response: {}", error.as_string().unwrap_or_default()))]
    ReceiveResponse { error: JsValue },
}

pub struct Body {
    data: Option<Vec<u8>>,
}

impl Body {
    pub fn empty() -> Self {
        Self { data: None }
    }
}

impl From<Vec<u8>> for Body {
    fn from(data: Vec<u8>) -> Body {
        Body { data: Some(data) }
    }
}

pub async fn recv_bytes(body: Body) -> Result<impl Deref<Target = [u8]>, Error> {
    Ok(body.data.unwrap_or_default())
}

pub async fn recv_reader(body: Body) -> Result<impl Read, Error> {
    Ok(Cursor::new(body.data.unwrap_or_default()))
}

impl Client {
    pub fn new() -> Self {
        Client
    }

    pub async fn request(&self, request: Request<Body>) -> Result<Response<Body>, Error> {
        let window = window().context(NoWindow)?;

        let (
            Parts {
                method,
                uri,
                version: _,
                headers,
                extensions: _,
                ..
            },
            body,
        ) = request.into_parts();

        let mut request_init = RequestInit::new();

        // FIXME: Use AbortSignal on drop to cancel the request when that is not experimental anymore.

        request_init.method(method.as_str());

        if let Some(body) = &body.data {
            let view = unsafe { Uint8Array::view(&body) };
            request_init.body(Some(view.unchecked_ref()));
        }

        let request_headers = web_sys::Headers::new().unwrap();

        for (name, value) in &headers {
            request_headers
                .append(name.as_str(), value.to_str().unwrap_or(""))
                .map_err(|error| Error::ForbiddenHeader { error })?;
        }

        request_init.headers(request_headers.unchecked_ref());

        let web_response: web_sys::Response =
            JsFuture::from(window.fetch_with_str_and_init(&uri.to_string(), &request_init))
                .await
                .map_err(|error| Error::ReceiveResponse { error })?
                .unchecked_into();

        // Don't drop this earlier, we unsafely borrow from it for the request.
        drop(body);

        let buf: js_sys::ArrayBuffer = JsFuture::from(
            web_response
                .array_buffer()
                .map_err(|error| Error::ReceiveResponse { error })?,
        )
        .await
        .map_err(|error| Error::ReceiveResponse { error })?
        .unchecked_into();

        let slice = Uint8Array::new(&buf);
        let mut body: Vec<u8> = vec![0; slice.length() as usize];
        slice.copy_to(&mut body);

        let mut response = Response::new(Body { data: Some(body) });

        *response.status_mut() = StatusCode::from_u16(web_response.status()).unwrap();

        let headers = response.headers_mut();

        let prop = "value".into();

        for pair in js_sys::try_iter(&web_response.headers()).unwrap().unwrap() {
            let array: Array = pair.unwrap().into();
            let vals = array.values();

            let key = Reflect::get(&vals.next().unwrap(), &prop).unwrap();
            let value = Reflect::get(&vals.next().unwrap(), &prop).unwrap();

            let key = key.as_string().unwrap();
            let value = value.as_string().unwrap();

            headers.append(
                HeaderName::from_bytes(key.as_bytes()).unwrap(),
                HeaderValue::from_str(&value).unwrap(),
            );
        }

        Ok(response)
    }

    pub async fn connect_ws(&self, uri: &str) -> Result<WsStream, crate::Error> {
        let ws = WebSocket::new(uri).unwrap(); // TODO: Error handling

        // TODO: We should actually select on open and close here, not error.
        // https://jsfiddle.net/lamarant/ry0ty52n/

        let (tx_open, rx_open) = oneshot::channel();
        let on_open = Closure::once(move || {
            let _ = tx_open.send(());
        });

        let (tx_close, rx_close) = oneshot::channel();
        let on_close = Closure::once(move || {
            let _ = tx_close.send(unimplemented!());
        });

        let (tx_message, rx_message) = mpsc::unbounded();
        let on_message = Closure::wrap(Box::new(move || {
            let _ = tx_message.unbounded_send(unimplemented!());
        }) as Box<dyn FnMut()>);

        let (tx_err, rx_err) = oneshot::channel();
        let on_err = Closure::once(move || {
            let _ = tx_err.send(unimplemented!());
        });

        ws.set_onopen(Some(on_open.as_ref().unchecked_ref()));
        ws.set_onclose(Some(on_close.as_ref().unchecked_ref()));
        ws.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        ws.set_onerror(Some(on_err.as_ref().unchecked_ref()));

        let rx_close = match select(rx_open, rx_close).await {
            Either::Left((_, rx_close)) => rx_close,
            Either::Right((e, _)) => return Err(e.unwrap()),
        };

        Ok(WsStream {
            ws,
            _on_close: on_close,
            _on_err: on_err,
            _on_message: on_message,
            rx_close,
            rx_err,
            rx_message,
            connected: true,
        })
    }
}
