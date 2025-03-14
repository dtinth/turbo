use std::{
    pin::Pin,
    task::{Context, Poll},
};

use anyhow::{Context as _, Error, Result};
use futures::{prelude::*, ready, stream::FusedStream, SinkExt};
use hyper::{upgrade::Upgraded, HeaderMap, Uri};
use hyper_tungstenite::{tungstenite::Message, HyperWebsocket, WebSocketStream};
use pin_project_lite::pin_project;
use tokio::select;
use tokio_stream::StreamMap;
use turbo_tasks::{TransientInstance, TurboTasksApi};
use turbopack_cli_utils::issue::ConsoleUiVc;
use turbopack_core::version::Update;

use super::{
    protocol::{ClientMessage, ClientUpdateInstruction, Issue, ResourceIdentifier},
    stream::UpdateStream,
};
use crate::{
    source::{request::SourceRequest, resolve::resolve_source_request, Body},
    update::stream::UpdateStreamItem,
    SourceProvider,
};

/// A server that listens for updates and sends them to connected clients.
pub(crate) struct UpdateServer<P: SourceProvider> {
    source_provider: P,
    console_ui: ConsoleUiVc,
}

impl<P: SourceProvider + Clone + Send + Sync> UpdateServer<P> {
    /// Create a new update server with the given websocket and content source.
    pub fn new(source_provider: P, console_ui: ConsoleUiVc) -> Self {
        Self {
            source_provider,
            console_ui,
        }
    }

    /// Run the update server loop.
    pub fn run(self, tt: &dyn TurboTasksApi, ws: HyperWebsocket) {
        tt.run_once_process(Box::pin(async move {
            if let Err(err) = self.run_internal(ws).await {
                println!("[UpdateServer]: error {:#}", err);
            }
            Ok(())
        }));
    }

    async fn run_internal(self, ws: HyperWebsocket) -> Result<()> {
        let mut client: UpdateClient = ws.await?.into();

        let mut streams = StreamMap::new();

        loop {
            select! {
                message = client.try_next() => {
                    match message? {
                        Some(ClientMessage::Subscribe { resource }) => {
                            let get_content = {
                                let source_provider = self.source_provider.clone();
                                let request = resource_to_request(&resource)?;
                                move || {
                                    let request = request.clone();
                                    let source = source_provider.get_source();
                                    resolve_source_request(
                                        source,
                                        TransientInstance::new(request),
                                        self.console_ui
                                    )
                                }
                            };
                            let stream = UpdateStream::new(TransientInstance::new(Box::new(get_content))).await?;
                            streams.insert(resource, stream);
                        }
                        Some(ClientMessage::Unsubscribe { resource }) => {
                            streams.remove(&resource);
                        }
                        None => {
                            // WebSocket was closed, stop sending updates
                            break;
                        }
                    }
                }
                Some((resource, update)) = streams.next() => {
                    Self::send_update(&mut client, resource, &update).await?;
                }
                else => break
            }
        }

        Ok(())
    }

    async fn send_update(
        client: &mut UpdateClient,
        resource: ResourceIdentifier,
        update: &UpdateStreamItem,
    ) -> Result<()> {
        let issues = update
            .issues
            .iter()
            .map(|p| (&**p).into())
            .collect::<Vec<Issue<'_>>>();

        match &*update.update {
            Update::Partial(partial) => {
                let partial_instruction = partial.instruction.await?;
                client
                    .send(ClientUpdateInstruction::partial(
                        &resource,
                        &partial_instruction,
                        &issues,
                    ))
                    .await?;
            }
            Update::Total(_total) => {
                client
                    .send(ClientUpdateInstruction::restart(&resource, &issues))
                    .await?;
            }
            Update::None => {
                client
                    .send(ClientUpdateInstruction::issues(&resource, &issues))
                    .await?;
            }
        }

        Ok(())
    }
}

fn resource_to_request(resource: &ResourceIdentifier) -> Result<SourceRequest> {
    let mut headers = HeaderMap::new();

    if let Some(res_headers) = &resource.headers {
        for (name, value) in res_headers {
            headers.append(
                hyper::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                hyper::header::HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
    }

    Ok(SourceRequest {
        uri: Uri::try_from(format!("/{}", resource.path))?,
        headers,
        method: "GET".to_string(),
        body: Body::new(vec![]),
    })
}

pin_project! {
    struct UpdateClient {
        #[pin]
        ws: WebSocketStream<Upgraded>,
        ended: bool,
    }
}

impl Stream for UpdateClient {
    type Item = Result<ClientMessage>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.ended {
            return Poll::Ready(None);
        }

        let this = self.project();
        let item = ready!(this.ws.poll_next(cx));

        let msg = match item {
            Some(Ok(Message::Text(msg))) => msg,
            Some(Err(err)) => {
                *this.ended = true;

                let err = Error::new(err).context("reading from websocket");
                return Poll::Ready(Some(Err(err)));
            }
            _ => {
                *this.ended = true;
                return Poll::Ready(None);
            }
        };

        match serde_json::from_str(&msg) {
            Ok(msg) => Poll::Ready(Some(Ok(msg))),
            Err(err) => {
                *this.ended = true;

                let err = Error::new(err).context("deserializing websocket message");
                Poll::Ready(Some(Err(err)))
            }
        }
    }
}

impl FusedStream for UpdateClient {
    fn is_terminated(&self) -> bool {
        self.ended || self.ws.is_terminated()
    }
}

impl<'a> Sink<ClientUpdateInstruction<'a>> for UpdateClient {
    type Error = Error;

    fn poll_ready(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.project()
            .ws
            .poll_ready(cx)
            .map(|res| res.context("polling WebSocket ready"))
    }

    fn start_send(
        self: Pin<&mut Self>,
        item: ClientUpdateInstruction<'a>,
    ) -> std::result::Result<(), Self::Error> {
        let msg = Message::text(serde_json::to_string(&item)?);

        self.project()
            .ws
            .start_send(msg)
            .context("sending to WebSocket")
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.project()
            .ws
            .poll_flush(cx)
            .map(|res| res.context("flushing WebSocket"))
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        self.project()
            .ws
            .poll_close(cx)
            .map(|res| res.context("closing WebSocket"))
    }
}

impl From<WebSocketStream<Upgraded>> for UpdateClient {
    fn from(ws: WebSocketStream<Upgraded>) -> Self {
        Self { ws, ended: false }
    }
}
