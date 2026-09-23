use crate::supervisor::{Supervisor, SupervisorError};
use futures_util::{SinkExt, StreamExt};
use mpv_targets::{ErrorCode, Event, Operation, Request, Response, TlsMaterialError};
use rustls::ServerConfig;
use serde_json::{Value, json};
use std::{fs, path::Path, sync::Arc};
use thiserror::Error;
use tokio::{net::TcpListener, sync::mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{accept_async, tungstenite::Message};

#[derive(Debug, Error)]
pub enum ServerError {
    #[error("cannot read TLS material: {0}")]
    TlsIo(#[source] std::io::Error),
    #[error("invalid TLS material: {0}")]
    Tls(#[source] TlsMaterialError),
    #[error("cannot bind configured listener: {0}")]
    Bind(#[source] std::io::Error),
}

pub struct WebServer {
    listener: TcpListener,
    acceptor: TlsAcceptor,
}

impl WebServer {
    pub async fn bind(
        config_root: &Path,
        listen: &str,
        certificate: &str,
        private_key: &str,
    ) -> Result<Self, ServerError> {
        let tls = load_tls(
            &config_root.join(certificate),
            &config_root.join(private_key),
        )?;
        let listener = TcpListener::bind(listen).await.map_err(ServerError::Bind)?;
        Ok(Self {
            listener,
            acceptor: TlsAcceptor::from(tls),
        })
    }

    pub async fn run(self, supervisor: Supervisor, listen: &str) -> Result<(), ServerError> {
        eprintln!("mpv-targetsd: listening on wss://{listen}");
        loop {
            let (stream, peer) = self.listener.accept().await.map_err(ServerError::Bind)?;
            let acceptor = self.acceptor.clone();
            let supervisor = supervisor.clone();
            tokio::spawn(async move {
                let result = async {
                    let stream = acceptor
                        .accept(stream)
                        .await
                        .map_err(|error| error.to_string())?;
                    let socket = accept_async(stream)
                        .await
                        .map_err(|error| error.to_string())?;
                    serve_connection(socket, supervisor).await
                }
                .await;
                if let Err(error) = result {
                    eprintln!("mpv-targetsd: client {peer} disconnected: {error}");
                }
            });
        }
    }
}

fn load_tls(certificate: &Path, private_key: &Path) -> Result<Arc<ServerConfig>, ServerError> {
    let certificate = fs::read(certificate).map_err(ServerError::TlsIo)?;
    let private_key = fs::read(private_key).map_err(ServerError::TlsIo)?;
    mpv_targets::server_tls_config(&certificate, &private_key)
        .map(Arc::new)
        .map_err(ServerError::Tls)
}

async fn serve_connection<S>(
    socket: tokio_tungstenite::WebSocketStream<S>,
    supervisor: Supervisor,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut writer, mut reader) = socket.split();
    let (snapshot, mut events) = supervisor.snapshot_and_subscribe().await;
    send_event(&mut writer, Event::Snapshot { snapshot }).await?;
    let (responses, mut receive_responses) = mpsc::channel(32);
    loop {
        tokio::select! {
            message = reader.next() => match message {
                Some(Ok(Message::Text(text))) => {
                    let response_sender = responses.clone();
                    let supervisor = supervisor.clone();
                    tokio::spawn(async move {
                        let response = dispatch_request(&supervisor, &text).await;
                        let _ = response_sender.send(response).await;
                    });
                }
                Some(Ok(Message::Close(_))) | None => return Ok(()),
                Some(Ok(_)) => {},
                Some(Err(error)) => return Err(error.to_string()),
            },
            response = receive_responses.recv() => match response {
                Some(response) => send_response(&mut writer, response).await?,
                None => return Ok(()),
            },
            event = events.recv() => match event {
                Ok(event) => send_event(&mut writer, event).await?,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return Err("target event revision gap".into()),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            },
        }
    }
}

async fn dispatch_request(supervisor: &Supervisor, source: &str) -> Response {
    let value: Value = match serde_json::from_str(source) {
        Ok(value) => value,
        Err(error) => return Response::error("", ErrorCode::InvalidRequest, error.to_string()),
    };
    let id = value
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request: Request = match serde_json::from_value(value) {
        Ok(request) => request,
        Err(error) => return Response::error(id, ErrorCode::InvalidRequest, error.to_string()),
    };
    if let Err(error) = request.validate() {
        return Response::error(request.id, ErrorCode::InvalidRequest, error.to_string());
    }
    let id = request.id.clone();
    match dispatch_operation(supervisor, request).await {
        Ok(data) => Response::success(id, data),
        Err(error) => Response::error(id, error_code(&error), error.to_string()),
    }
}

async fn dispatch_operation(
    supervisor: &Supervisor,
    request: Request,
) -> Result<Value, SupervisorError> {
    let target = request.target.as_deref();
    match request.operation {
        Operation::TargetAdd { from, enable } => {
            let target = target.expect("validated target operation");
            let target = supervisor.add(target, from.as_deref(), enable).await?;
            let copied = if from.is_some() {
                vec!["mpv.conf", "scripts/"]
            } else {
                vec!["mpv.conf"]
            };
            Ok(
                json!({"name": target.name, "disabled": target.disabled, "online": target.online, "stopped": target.stopped, "channel": target.channel, "copied": copied}),
            )
        }
        Operation::TargetStart => {
            let target = supervisor
                .start(target.expect("validated target operation"))
                .await?;
            Ok(json!({"online": target.online, "stopped": target.stopped}))
        }
        Operation::TargetRemove => {
            let target = target.expect("validated target operation");
            supervisor.remove(target).await?;
            Ok(json!({"removed": target}))
        }
        Operation::TargetStop => {
            let target = supervisor
                .stop(target.expect("validated target operation"))
                .await?;
            Ok(json!({"online": target.online, "stopped": target.stopped}))
        }
        Operation::TargetRestart => {
            let target = supervisor
                .restart(target.expect("validated target operation"))
                .await?;
            Ok(json!({"online": target.online, "stopped": target.stopped}))
        }
        Operation::TargetEnable => {
            let target = supervisor
                .enable(target.expect("validated target operation"))
                .await?;
            Ok(json!({"disabled": target.disabled}))
        }
        Operation::TargetDisable => {
            let target = supervisor
                .disable(target.expect("validated target operation"))
                .await?;
            Ok(json!({"disabled": target.disabled}))
        }
        Operation::TargetSetChannel { channel, restart } => {
            let target_name = target.expect("validated target operation");
            let target = supervisor.set_channel(target_name, channel.clone()).await?;
            let target = if restart {
                supervisor.restart(target_name).await?
            } else {
                target
            };
            Ok(
                json!({"channel": channel, "restart_requested": restart, "restart_completed": restart && target.online}),
            )
        }
        Operation::NodeListChannels => {
            let channels = supervisor.list_channels()?;
            Ok(json!({"channels": channels}))
        }
        Operation::TargetRename { name } => {
            let source = target.expect("validated target operation");
            let target = supervisor.rename(source, &name).await?;
            Ok(json!({"from": source, "to": target.name}))
        }
        Operation::Mpv { command, args } => {
            supervisor
                .mpv_command(target.expect("validated target operation"), &command, &args)
                .await
        }
    }
}

fn error_code(error: &SupervisorError) -> ErrorCode {
    match error {
        SupervisorError::UnknownTarget(_) => ErrorCode::UnknownTarget,
        SupervisorError::TargetDisabled => ErrorCode::TargetDisabled,
        SupervisorError::TargetAlreadyStopped => ErrorCode::TargetAlreadyStopped,
        SupervisorError::TargetNotRunning => ErrorCode::TargetNotRunning,
        SupervisorError::RenameRequiresStopped => ErrorCode::ConfigRejected,
        SupervisorError::IpcUnavailable(_) => ErrorCode::IpcLost,
        SupervisorError::MpvRejected(_) => ErrorCode::MpvRejected,
        SupervisorError::Launch(_) => ErrorCode::TargetOffline,
        SupervisorError::Config(_) | SupervisorError::ConfigSerialization(_) => {
            ErrorCode::ConfigRejected
        }
        SupervisorError::TargetExists(_) => ErrorCode::ConfigRejected,
    }
}

async fn send_event<S>(
    writer: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    event: Event,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    writer
        .send(Message::Text(
            serde_json::to_string(&event)
                .expect("protocol event must serialize")
                .into(),
        ))
        .await
        .map_err(|error| error.to_string())
}

async fn send_response<S>(
    writer: &mut futures_util::stream::SplitSink<tokio_tungstenite::WebSocketStream<S>, Message>,
    response: Response,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    writer
        .send(Message::Text(
            serde_json::to_string(&response)
                .expect("protocol response must serialize")
                .into(),
        ))
        .await
        .map_err(|error| error.to_string())
}
