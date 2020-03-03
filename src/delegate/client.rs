//! Types for sending data to and from the language client.

use std::fmt::Display;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use dashmap::DashMap;
use futures::channel::mpsc::{Receiver, Sender};
use futures::sink::SinkExt;
use futures::stream::StreamExt;
use jsonrpc_core::types::{ErrorCode, Id, Output, Version};
use jsonrpc_core::{Error, Result};
use log::{error, trace};
use lsp_types::notification::{Notification, *};
use lsp_types::request::{ApplyWorkspaceEdit, RegisterCapability, Request, UnregisterCapability};
use lsp_types::*;
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;

/// Handle for communicating with the language client.
#[derive(Debug)]
pub struct Client {
    sender: Sender<String>,
    initialized: Arc<AtomicBool>,
    request_id: AtomicU64,
    pending_requests: Arc<DashMap<u64, Option<Output>>>,
}

impl Client {
    pub(super) fn new(
        sender: Sender<String>,
        mut receiver: Receiver<Output>,
        initialized: Arc<AtomicBool>,
    ) -> Self {
        let pending_requests = Arc::new(DashMap::default());

        let pending = pending_requests.clone();
        tokio::spawn(async move {
            loop {
                while let Some(response) = receiver.next().await {
                    if let Id::Num(ref id) = response.id() {
                        pending.insert(*id, Some(response));
                    } else {
                        error!("received response from client with non-numeric ID");
                    }
                }
            }
        });

        Client {
            sender,
            initialized,
            request_id: AtomicU64::new(0),
            pending_requests,
        }
    }

    /// Notifies the client to log a particular message.
    ///
    /// This corresponds to the [`window/logMessage`] notification.
    ///
    /// [`window/logMessage`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#window_logMessage
    pub fn log_message<M: Display>(&self, typ: MessageType, message: M) {
        self.send_notification::<LogMessage>(LogMessageParams {
            typ,
            message: message.to_string(),
        });
    }

    /// Notifies the client to display a particular message in the user interface.
    ///
    /// This corresponds to the [`window/showMessage`] notification.
    ///
    /// [`window/showMessage`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#window_showMessage
    pub fn show_message<M: Display>(&self, typ: MessageType, message: M) {
        self.send_notification::<ShowMessage>(ShowMessageParams {
            typ,
            message: message.to_string(),
        });
    }

    /// Notifies the client to log a telemetry event.
    ///
    /// This corresponds to the [`telemetry/event`] notification.
    ///
    /// [`telemetry/event`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#telemetry_event
    pub fn telemetry_event<S: Serialize>(&self, data: S) {
        match serde_json::to_value(data) {
            Err(e) => error!("invalid JSON in `telemetry/event` notification: {}", e),
            Ok(mut value) => {
                if !value.is_null() && !value.is_array() && !value.is_object() {
                    value = Value::Array(vec![value]);
                }

                self.send_notification::<TelemetryEvent>(value);
            }
        }
    }

    /// Register a new capability with the client.
    ///
    /// This corresponds to the [`client/registerCapability`] request.
    ///
    /// [`client/registerCapability`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#client_registerCapability
    pub async fn register_capability(&self, registrations: Vec<Registration>) -> Result<()> {
        self.send_request_initialized::<RegisterCapability>(RegistrationParams { registrations })
            .await
    }

    /// Unregister a capability with the client.
    ///
    /// This corresponds to the [`client/unregisterCapability`] request.
    ///
    /// [`client/unregisterCapability`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#client_unregisterCapability
    pub async fn unregister_capability(&self, unregisterations: Vec<Unregistration>) -> Result<()> {
        self.send_request_initialized::<UnregisterCapability>(UnregistrationParams {
            unregisterations,
        })
        .await
    }

    /// Requests a workspace resource be edited on the client side and returns whether the edit was
    /// applied.
    ///
    /// This corresponds to the [`workspace/applyEdit`] request.
    ///
    /// [`workspace/applyEdit`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#workspace_applyEdit
    pub async fn apply_edit(&self, edit: WorkspaceEdit) -> Result<ApplyWorkspaceEditResponse> {
        self.send_request_initialized::<ApplyWorkspaceEdit>(ApplyWorkspaceEditParams { edit })
            .await
    }

    /// Submits validation diagnostics for an open file with the given URI.
    ///
    /// This corresponds to the [`textDocument/publishDiagnostics`] notification.
    ///
    /// [`textDocument/publishDiagnostics`]: https://microsoft.github.io/language-server-protocol/specifications/specification-current/#textDocument_publishDiagnostics
    pub fn publish_diagnostics(&self, uri: Url, diags: Vec<Diagnostic>, version: Option<i64>) {
        self.send_notification_initialized::<PublishDiagnostics>(PublishDiagnosticsParams::new(
            uri, diags, version,
        ));
    }

    /// Sends a custom notification to the client.
    pub fn send_custom_notification<N>(&self, params: N::Params)
    where
        N: Notification,
        N::Params: Serialize,
    {
        self.send_notification_initialized::<N>(params);
    }

    async fn send_request<R>(&self, params: R::Params) -> Result<R::Result>
    where
        R: Request,
        R::Params: Serialize,
        R::Result: DeserializeOwned,
    {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        self.pending_requests.insert(id, None);

        let message = make_request::<R>(id, params);
        if self.sender.clone().send(message).await.is_err() {
            error!("failed to send request");
            return Err(Error::internal_error());
        }

        loop {
            let response = self
                .pending_requests
                .remove_if(&id, |_, v| v.is_some())
                .and_then(|entry| entry.1);

            match response {
                Some(Output::Success(s)) => {
                    return serde_json::from_value(s.result).map_err(|e| Error {
                        code: ErrorCode::ParseError,
                        message: e.to_string(),
                        data: None,
                    });
                }
                Some(Output::Failure(f)) => return Err(f.error),
                None => tokio::task::yield_now().await,
            }
        }
    }

    fn send_notification<N>(&self, params: N::Params)
    where
        N: Notification,
        N::Params: Serialize,
    {
        let mut sender = self.sender.clone();
        let message = make_notification::<N>(params);
        tokio::spawn(async move {
            if sender.send(message).await.is_err() {
                error!("failed to send notification")
            }
        });
    }

    async fn send_request_initialized<R>(&self, params: R::Params) -> Result<R::Result>
    where
        R: Request,
        R::Params: Serialize,
        R::Result: DeserializeOwned,
    {
        if self.initialized.load(Ordering::SeqCst) {
            self.send_request::<R>(params).await
        } else {
            let id = self.request_id.load(Ordering::SeqCst) + 1;
            let msg = make_request::<R>(id, params);
            trace!("server not initialized, supressing message: {}", msg);
            Err(Error::internal_error())
        }
    }

    fn send_notification_initialized<N>(&self, params: N::Params)
    where
        N: Notification,
        N::Params: Serialize,
    {
        if self.initialized.load(Ordering::SeqCst) {
            self.send_notification::<N>(params);
        } else {
            let msg = make_notification::<N>(params);
            trace!("server not initialized, supressing message: {}", msg);
        }
    }
}

/// Constructs a JSON-RPC request from its corresponding LSP type.
fn make_request<R>(id: u64, params: R::Params) -> String
where
    R: Request,
    R::Params: Serialize,
{
    #[derive(Serialize)]
    struct RawRequest<T> {
        jsonrpc: Version,
        method: &'static str,
        params: T,
        id: Id,
    }

    // Since these types come from the `lsp-types` crate and validity is enforced via the
    // `Request` trait, the `unwrap()` call below should never fail.
    serde_json::to_string(&RawRequest {
        jsonrpc: Version::V2,
        id: Id::Num(id),
        method: R::METHOD,
        params,
    })
    .unwrap()
}

/// Constructs a JSON-RPC notification from its corresponding LSP type.
fn make_notification<N>(params: N::Params) -> String
where
    N: Notification,
    N::Params: Serialize,
{
    #[derive(Serialize)]
    struct RawNotification<T> {
        jsonrpc: Version,
        method: &'static str,
        params: T,
    }

    // Since these types come from the `lsp-types` crate and validity is enforced via the
    // `Notification` trait, the `unwrap()` call below should never fail.
    serde_json::to_string(&RawNotification {
        jsonrpc: Version::V2,
        method: N::METHOD,
        params,
    })
    .unwrap()
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;
    use serde_json::json;

    use super::*;

    async fn assert_client_messages<F: FnOnce(Client)>(f: F, expected: String) {
        let (request_tx, request_rx) = mpsc::channel(1);
        let (response_tx, response_rx) = mpsc::channel(1);

        let client = Client::new(request_tx, response_rx, Arc::new(AtomicBool::new(true)));
        f(client);
        drop(response_tx);

        let messages: Vec<_> = request_rx.collect().await;
        assert_eq!(messages, vec![expected]);
    }

    #[tokio::test]
    async fn log_message() {
        let (typ, message) = (MessageType::Log, "foo bar".to_owned());
        let expected = make_notification::<LogMessage>(LogMessageParams {
            typ,
            message: message.clone(),
        });

        assert_client_messages(|p| p.log_message(typ, message), expected).await;
    }

    #[tokio::test]
    async fn show_message() {
        let (typ, message) = (MessageType::Log, "foo bar".to_owned());
        let expected = make_notification::<ShowMessage>(ShowMessageParams {
            typ,
            message: message.clone(),
        });

        assert_client_messages(|p| p.show_message(typ, message), expected).await;
    }

    #[tokio::test]
    async fn telemetry_event() {
        let null = json!(null);
        let expected = make_notification::<TelemetryEvent>(null.clone());
        assert_client_messages(|p| p.telemetry_event(null), expected).await;

        let array = json!([1, 2, 3]);
        let expected = make_notification::<TelemetryEvent>(array.clone());
        assert_client_messages(|p| p.telemetry_event(array), expected).await;

        let object = json!({});
        let expected = make_notification::<TelemetryEvent>(object.clone());
        assert_client_messages(|p| p.telemetry_event(object), expected).await;

        let anything_else = json!("hello");
        let wrapped = Value::Array(vec![anything_else.clone()]);
        let expected = make_notification::<TelemetryEvent>(wrapped);
        assert_client_messages(|p| p.telemetry_event(anything_else), expected).await;
    }

    #[tokio::test]
    async fn publish_diagnostics() {
        let uri: Url = "file:///path/to/file".parse().unwrap();
        let diagnostics = vec![Diagnostic::new_simple(Default::default(), "example".into())];

        let params = PublishDiagnosticsParams::new(uri.clone(), diagnostics.clone(), None);
        let expected = make_notification::<PublishDiagnostics>(params);

        assert_client_messages(|p| p.publish_diagnostics(uri, diagnostics, None), expected).await;
    }
}