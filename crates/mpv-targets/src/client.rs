//! The shared native WebSocket client used by `targets`, PaneBot, and other
//! clients.  It owns transport, request correlation, the current snapshot,
//! and event delivery; reconnection remains the caller's decision.

use crate::protocol::PROTOCOL_VERSION;
use crate::{
    Event, HealthState, MediaEndReason, Operation, Request, Response, Snapshot, TargetChanged,
};
use futures_util::{SinkExt, StreamExt};
use rustls::{
    ClientConfig, DigitallySignedStruct, SignatureScheme,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use thiserror::Error;
use tokio::{
    net::TcpStream,
    sync::{RwLock, broadcast, mpsc, oneshot, watch},
    time::{Duration, timeout},
};
use tokio_rustls::TlsConnector;
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type PendingMap = Arc<
    tokio::sync::Mutex<
        std::collections::HashMap<String, oneshot::Sender<Result<Response, ClientError>>>,
    >,
>;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

enum ClientCommand {
    Request(Request),
    Close(oneshot::Sender<()>),
}

#[derive(Clone)]
struct ConnectionState {
    pending: PendingMap,
    events: broadcast::Sender<Event>,
    snapshot: watch::Sender<Option<Snapshot>>,
    disconnected: Arc<AtomicBool>,
    disconnect_error: Arc<RwLock<Option<ClientError>>>,
    disconnect: watch::Sender<Option<DisconnectReason>>,
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub url: String,
    pub connect_timeout: Duration,
    pub request_timeout: Duration,
}

impl ClientOptions {
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
        }
    }

    pub fn with_connect_timeout(mut self, value: Duration) -> Self {
        self.connect_timeout = value;
        self
    }

    pub fn with_request_timeout(mut self, value: Duration) -> Self {
        self.request_timeout = value;
        self
    }
}

#[derive(Clone, Debug)]
pub enum DisconnectReason {
    Closed,
    Failed(ClientError),
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct LifecycleResult {
    pub online: bool,
    pub stopped: bool,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct EnableResult {
    pub disabled: bool,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct AddTargetResult {
    pub name: String,
    pub disabled: bool,
    pub online: bool,
    pub stopped: bool,
    pub channel: Option<String>,
    pub copied: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct RemoveTargetResult {
    pub removed: String,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct RenameTargetResult {
    pub from: String,
    pub to: String,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct ChannelMutationResult {
    pub channel: Option<String>,
    pub restart_requested: bool,
    pub restart_completed: bool,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct ChannelCatalog {
    pub channels: Vec<String>,
}

#[derive(Clone, Debug, serde::Deserialize, Eq, PartialEq)]
pub struct PlaylistItem {
    pub id: u64,
    pub filename: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub current: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TargetStatus {
    Disabled,
    Stopped,
    Offline,
    Idle,
    Paused,
    Playing,
}

impl TargetStatus {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "Disabled",
            Self::Stopped => "Stopped",
            Self::Offline => "Offline",
            Self::Idle => "Idle",
            Self::Paused => "Paused",
            Self::Playing => "Playing",
        }
    }
}

pub fn target_status(target: &crate::TargetRecord) -> TargetStatus {
    if target.disabled {
        TargetStatus::Disabled
    } else if target.stopped {
        TargetStatus::Stopped
    } else if !target.online {
        TargetStatus::Offline
    } else if target.state.idle_active == Some(true) {
        TargetStatus::Idle
    } else if target.state.paused == Some(true) {
        TargetStatus::Paused
    } else {
        TargetStatus::Playing
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoadMode {
    Replace,
    Append,
    AppendPlay,
}

impl LoadMode {
    fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Append => "append",
            Self::AppendPlay => "append-play",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoopState {
    Off,
    On,
}

impl LoopState {
    fn as_mpv_value(self) -> &'static str {
        match self {
            Self::Off => "no",
            Self::On => "inf",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Track {
    Auto,
    Disabled,
    Id(u32),
}

impl Track {
    fn as_value(self) -> serde_json::Value {
        match self {
            Self::Auto => serde_json::json!("auto"),
            Self::Disabled => serde_json::json!("no"),
            Self::Id(id) => serde_json::json!(id),
        }
    }
}

#[derive(Clone, Debug, Error)]
pub enum ClientError {
    #[error("client URL must use wss://")]
    InvalidUrl,
    #[error("invalid client URL: {0}")]
    Url(String),
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("TLS setup failed: {0}")]
    Tls(String),
    #[error("transport is disconnected")]
    Disconnected,
    #[error("request timed out")]
    Timeout,
    #[error("server returned {0:?}: {1}")]
    Remote(crate::ErrorCode, String),
    #[error("invalid server message: {0}")]
    Protocol(String),
}

#[derive(Clone)]
pub struct RemoteClient {
    writer: mpsc::Sender<ClientCommand>,
    pending: PendingMap,
    snapshot: watch::Receiver<Option<Snapshot>>,
    events: broadcast::Sender<Event>,
    disconnected: Arc<AtomicBool>,
    disconnect_error: Arc<RwLock<Option<ClientError>>>,
    disconnect: watch::Receiver<Option<DisconnectReason>>,
    next_id: Arc<AtomicU64>,
    request_timeout: Duration,
}

impl RemoteClient {
    pub async fn connect(options: ClientOptions) -> Result<Self, ClientError> {
        let connect_timeout = options.connect_timeout;
        timeout(connect_timeout, Self::connect_inner(options))
            .await
            .map_err(|_| ClientError::Timeout)?
    }

    async fn connect_inner(options: ClientOptions) -> Result<Self, ClientError> {
        let ClientOptions {
            url,
            request_timeout,
            ..
        } = options;
        let endpoint = Endpoint::parse(&url)?;
        let request = url
            .clone()
            .into_client_request()
            .map_err(|e| ClientError::Url(e.to_string()))?;
        let stream = TcpStream::connect((endpoint.host.as_str(), endpoint.port))
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let tls_builder = ClientConfig::builder();
        let signature_schemes = rustls::crypto::CryptoProvider::get_default()
            .expect("ClientConfig builder installs a crypto provider")
            .signature_verification_algorithms
            .supported_schemes();
        let tls = tls_builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnyServerCertificate {
                signature_schemes,
            }))
            .with_no_client_auth();
        let name = ServerName::try_from(endpoint.host.clone())
            .map_err(|e| ClientError::Tls(e.to_string()))?;
        let tls_stream = TlsConnector::from(Arc::new(tls))
            .connect(name, stream)
            .await
            .map_err(|e| ClientError::Tls(e.to_string()))?;
        let (socket, _) = client_async(request, tls_stream)
            .await
            .map_err(|e| ClientError::Connect(e.to_string()))?;
        let (writer, mut reader) = socket.split();
        let (tx, mut commands) = mpsc::channel(32);
        let (events, _) = broadcast::channel(64);
        let (snapshot_tx, snapshot) = watch::channel(None);
        let (disconnect_tx, disconnect) = watch::channel(None);
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnect_error = Arc::new(RwLock::new(None));
        let pending: PendingMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let connection_state = ConnectionState {
            pending: pending.clone(),
            events: events.clone(),
            snapshot: snapshot_tx,
            disconnected: disconnected.clone(),
            disconnect_error: disconnect_error.clone(),
            disconnect: disconnect_tx,
        };
        tokio::spawn(async move {
            run_connection(writer, &mut reader, &mut commands, connection_state).await;
        });
        let client = Self {
            writer: tx,
            pending,
            snapshot,
            events,
            disconnected,
            disconnect_error,
            disconnect,
            next_id: Arc::new(AtomicU64::new(1)),
            request_timeout,
        };
        let mut snapshots = client.subscribe_snapshots();
        let mut disconnects = client.subscribe_disconnect();
        loop {
            if snapshots.borrow().is_some() {
                return Ok(client);
            }
            if let Some(reason) = disconnects.borrow().clone() {
                return Err(disconnect_reason_error(reason));
            }
            tokio::select! {
                changed = snapshots.changed() => {
                    changed.map_err(|_| ClientError::Disconnected)?;
                }
                changed = disconnects.changed() => {
                    changed.map_err(|_| ClientError::Disconnected)?;
                }
            }
        }
    }

    pub async fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot.borrow().clone()
    }
    pub fn subscribe_snapshots(&self) -> watch::Receiver<Option<Snapshot>> {
        self.snapshot.clone()
    }
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }
    pub fn target(&self, target: impl Into<String>) -> TargetClient {
        TargetClient {
            client: self.clone(),
            target: target.into(),
        }
    }
    pub fn disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }
    pub async fn disconnect_error(&self) -> Option<ClientError> {
        self.disconnect_error.read().await.clone()
    }
    pub fn subscribe_disconnect(&self) -> watch::Receiver<Option<DisconnectReason>> {
        self.disconnect.clone()
    }
    pub async fn wait_disconnected(&self) -> DisconnectReason {
        let mut disconnect = self.subscribe_disconnect();
        loop {
            if let Some(reason) = disconnect.borrow().clone() {
                return reason;
            }
            if disconnect.changed().await.is_err() {
                return DisconnectReason::Failed(ClientError::Disconnected);
            }
        }
    }
    pub async fn close(&self) {
        if self.disconnected.swap(true, Ordering::AcqRel) {
            return;
        }
        let (complete, closed) = oneshot::channel();
        if self
            .writer
            .send(ClientCommand::Close(complete))
            .await
            .is_ok()
        {
            let _ = closed.await;
        }
    }

    pub async fn request(
        &self,
        target: impl Into<String>,
        operation: Operation,
    ) -> Result<Response, ClientError> {
        self.request_inner(Some(target.into()), operation).await
    }

    pub async fn list_channels(&self) -> Result<ChannelCatalog, ClientError> {
        decode_success(
            self.request_inner(None, Operation::NodeListChannels)
                .await?,
        )
    }

    async fn request_inner(
        &self,
        target: Option<String>,
        operation: Operation,
    ) -> Result<Response, ClientError> {
        if self.disconnected() {
            return Err(self
                .disconnect_error()
                .await
                .unwrap_or(ClientError::Disconnected));
        }
        let id = format!("client-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let request = Request {
            id: id.clone(),
            target,
            operation,
        };
        request
            .validate()
            .map_err(|e| ClientError::Protocol(e.to_string()))?;
        let (response, receive) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), response);
        if self
            .writer
            .send(ClientCommand::Request(request))
            .await
            .is_err()
        {
            self.pending.lock().await.remove(&id);
            return Err(self
                .disconnect_error()
                .await
                .unwrap_or(ClientError::Disconnected));
        }
        match timeout(self.request_timeout, receive).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(self
                .disconnect_error()
                .await
                .unwrap_or(ClientError::Disconnected)),
            Err(_) => {
                self.pending.lock().await.remove(&id);
                Err(ClientError::Timeout)
            }
        }
    }
}

fn disconnect_reason_error(reason: DisconnectReason) -> ClientError {
    match reason {
        DisconnectReason::Closed => ClientError::Disconnected,
        DisconnectReason::Failed(error) => error,
    }
}

fn decode_success<T: serde::de::DeserializeOwned>(response: Response) -> Result<T, ClientError> {
    match response {
        Response::Success { data, .. } => {
            serde_json::from_value(data).map_err(|error| ClientError::Protocol(error.to_string()))
        }
        Response::Error { error, .. } => Err(ClientError::Remote(error.code, error.message)),
    }
}

#[derive(Clone)]
pub struct TargetClient {
    client: RemoteClient,
    target: String,
}

impl TargetClient {
    pub fn name(&self) -> &str {
        &self.target
    }

    pub async fn request(&self, operation: Operation) -> Result<Response, ClientError> {
        self.client.request(self.target.clone(), operation).await
    }

    pub async fn add(
        &self,
        from: Option<String>,
        enable: bool,
    ) -> Result<AddTargetResult, ClientError> {
        decode_success(
            self.request(Operation::TargetAdd {
                from,
                enable,
                start_paused: true,
                start_muted: true,
            })
            .await?,
        )
    }
    pub async fn add_base(
        &self,
        enable: bool,
        start_paused: bool,
        start_muted: bool,
    ) -> Result<AddTargetResult, ClientError> {
        decode_success(
            self.request(Operation::TargetAdd {
                from: None,
                enable,
                start_paused,
                start_muted,
            })
            .await?,
        )
    }
    pub async fn remove_target(&self) -> Result<RemoveTargetResult, ClientError> {
        decode_success(self.request(Operation::TargetRemove).await?)
    }
    pub async fn start(&self) -> Result<LifecycleResult, ClientError> {
        decode_success(self.request(Operation::TargetStart).await?)
    }
    pub async fn stop_target(&self) -> Result<LifecycleResult, ClientError> {
        decode_success(self.request(Operation::TargetStop).await?)
    }
    pub async fn restart(&self) -> Result<LifecycleResult, ClientError> {
        decode_success(self.request(Operation::TargetRestart).await?)
    }
    pub async fn enable(&self) -> Result<EnableResult, ClientError> {
        decode_success(self.request(Operation::TargetEnable).await?)
    }
    pub async fn disable(&self) -> Result<EnableResult, ClientError> {
        decode_success(self.request(Operation::TargetDisable).await?)
    }
    pub async fn rename(
        &self,
        replacement: impl Into<String>,
    ) -> Result<RenameTargetResult, ClientError> {
        decode_success(
            self.request(Operation::TargetRename {
                name: replacement.into(),
            })
            .await?,
        )
    }
    pub async fn set_channel(
        &self,
        channel: Option<String>,
        restart: bool,
    ) -> Result<ChannelMutationResult, ClientError> {
        decode_success(
            self.request(Operation::TargetSetChannel { channel, restart })
                .await?,
        )
    }

    async fn mpv(
        &self,
        command: &str,
        args: Vec<serde_json::Value>,
    ) -> Result<Response, ClientError> {
        self.request(Operation::Mpv {
            command: command.into(),
            args,
        })
        .await
    }

    pub async fn load(
        &self,
        source: impl Into<String>,
        mode: LoadMode,
    ) -> Result<Response, ClientError> {
        self.mpv(
            "loadfile",
            vec![
                serde_json::json!(source.into()),
                serde_json::json!(mode.as_str()),
            ],
        )
        .await
    }
    pub async fn play(&self) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("pause"), serde_json::json!(false)],
        )
        .await
    }
    pub async fn pause(&self) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("pause"), serde_json::json!(true)],
        )
        .await
    }
    pub async fn stop(&self) -> Result<Response, ClientError> {
        self.mpv("stop", vec![]).await
    }
    pub async fn toggle_play(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("pause")]).await
    }
    pub async fn next(&self) -> Result<Response, ClientError> {
        self.mpv("playlist-next", vec![]).await
    }
    pub async fn previous(&self) -> Result<Response, ClientError> {
        self.mpv("playlist-prev", vec![]).await
    }
    pub async fn seek(&self, seconds: f64) -> Result<Response, ClientError> {
        self.mpv(
            "seek",
            vec![serde_json::json!(seconds), serde_json::json!("relative")],
        )
        .await
    }
    pub async fn playlist_play(&self, index: u32) -> Result<Response, ClientError> {
        self.mpv("playlist-play-index", vec![serde_json::json!(index)])
            .await
    }
    pub async fn load_list(
        &self,
        source: impl Into<String>,
        mode: LoadMode,
    ) -> Result<Response, ClientError> {
        self.mpv(
            "loadlist",
            vec![
                serde_json::json!(source.into()),
                serde_json::json!(mode.as_str()),
            ],
        )
        .await
    }
    pub async fn playlist(&self) -> Result<Vec<PlaylistItem>, ClientError> {
        decode_success(
            self.mpv("get_property", vec![serde_json::json!("playlist")])
                .await?,
        )
    }
    pub async fn playlist_clear(&self) -> Result<Response, ClientError> {
        self.mpv("playlist-clear", vec![]).await
    }
    pub async fn playlist_remove(&self, index: u32) -> Result<Response, ClientError> {
        self.mpv("playlist-remove", vec![serde_json::json!(index)])
            .await
    }
    pub async fn playlist_move(&self, from: u32, to: u32) -> Result<Response, ClientError> {
        self.mpv(
            "playlist-move",
            vec![serde_json::json!(from), serde_json::json!(to)],
        )
        .await
    }
    pub async fn shuffle(&self) -> Result<Response, ClientError> {
        self.mpv("playlist-shuffle", vec![]).await
    }
    pub async fn unshuffle(&self) -> Result<Response, ClientError> {
        self.mpv("playlist-unshuffle", vec![]).await
    }
    pub async fn set_volume(&self, volume: f64) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("volume"), serde_json::json!(volume)],
        )
        .await
    }
    pub async fn adjust_volume(&self, delta: f64) -> Result<Response, ClientError> {
        self.mpv(
            "add",
            vec![serde_json::json!("volume"), serde_json::json!(delta)],
        )
        .await
    }
    pub async fn mute(&self) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("mute"), serde_json::json!(true)],
        )
        .await
    }
    pub async fn unmute(&self) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("mute"), serde_json::json!(false)],
        )
        .await
    }
    pub async fn toggle_mute(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("mute")]).await
    }
    pub async fn fullscreen(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("fullscreen")])
            .await
    }
    pub async fn set_fullscreen(&self, enabled: bool) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("fullscreen"), serde_json::json!(enabled)],
        )
        .await
    }
    pub async fn set_loop(&self, state: LoopState) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![
                serde_json::json!("loop-file"),
                serde_json::json!(state.as_mpv_value()),
            ],
        )
        .await
    }
    pub async fn set_repeat(&self, state: LoopState) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![
                serde_json::json!("loop-playlist"),
                serde_json::json!(state.as_mpv_value()),
            ],
        )
        .await
    }
    pub async fn audio(&self, track: Track) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("aid"), track.as_value()],
        )
        .await
    }
    pub async fn subtitle(&self, track: Track) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![serde_json::json!("sid"), track.as_value()],
        )
        .await
    }
    pub async fn cycle_audio(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("aid")]).await
    }
    pub async fn cycle_subtitle(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("sid")]).await
    }
    pub async fn toggle_subtitle(&self) -> Result<Response, ClientError> {
        self.mpv("cycle", vec![serde_json::json!("sub-visibility")])
            .await
    }
    pub async fn subtitle_visibility(&self, visible: bool) -> Result<Response, ClientError> {
        self.mpv(
            "set_property",
            vec![
                serde_json::json!("sub-visibility"),
                serde_json::json!(visible),
            ],
        )
        .await
    }
    pub async fn show_text(
        &self,
        text: impl Into<String>,
        duration_ms: u32,
    ) -> Result<Response, ClientError> {
        self.mpv(
            "show-text",
            vec![
                serde_json::json!(text.into()),
                serde_json::json!(duration_ms),
            ],
        )
        .await
    }
    pub async fn keypress(&self, key: impl Into<String>) -> Result<Response, ClientError> {
        self.mpv("keypress", vec![serde_json::json!(key.into())])
            .await
    }
}

async fn run_connection<S>(
    mut writer: futures_util::stream::SplitSink<WebSocketStream<S>, Message>,
    reader: &mut futures_util::stream::SplitStream<WebSocketStream<S>>,
    commands: &mut mpsc::Receiver<ClientCommand>,
    state: ConnectionState,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let result: Result<(), ClientError> = async {
        loop { tokio::select! {
            command = commands.recv() => match command {
                Some(ClientCommand::Request(request)) => { let text = serde_json::to_string(&request).map_err(|e| ClientError::Protocol(e.to_string()))?; writer.send(Message::Text(text.into())).await.map_err(|_| ClientError::Disconnected)?; }
                Some(ClientCommand::Close(complete)) => { writer.send(Message::Close(None)).await.ok(); let _ = complete.send(()); return Ok(()); }
                None => { writer.send(Message::Close(None)).await.ok(); return Ok(()); }
            },
            message = reader.next() => match message {
                Some(Ok(Message::Text(text))) => handle_message(&text, &state.pending, &state.events, &state.snapshot).await?,
                Some(Ok(Message::Close(_))) | None => return Err(ClientError::Disconnected),
                Some(Ok(_)) => {},
                Some(Err(e)) => return Err(ClientError::Connect(e.to_string())),
            }
        }}
    }.await;
    let terminal_error = result.err();
    *state.disconnect_error.write().await = terminal_error.clone();
    state.disconnected.store(true, Ordering::Release);
    state
        .disconnect
        .send_replace(Some(match terminal_error.clone() {
            Some(error) => DisconnectReason::Failed(error),
            None => DisconnectReason::Closed,
        }));
    for (_, sender) in state.pending.lock().await.drain() {
        let _ = sender.send(Err(terminal_error
            .clone()
            .unwrap_or(ClientError::Disconnected)));
    }
}

async fn handle_message(
    text: &str,
    pending: &PendingMap,
    events: &broadcast::Sender<Event>,
    snapshot: &watch::Sender<Option<Snapshot>>,
) -> Result<(), ClientError> {
    if let Ok(event) = serde_json::from_str::<Event>(text) {
        let next = match &event {
            Event::Snapshot { snapshot: value } => {
                if value.protocol_version != PROTOCOL_VERSION {
                    return Err(ClientError::Protocol(format!(
                        "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                        value.protocol_version
                    )));
                }
                value.clone()
            }
            Event::TargetChanged(change) => {
                let mut current = snapshot.borrow().clone().ok_or_else(|| {
                    ClientError::Protocol("target change received before snapshot".into())
                })?;
                apply_target_change(&mut current, change)?;
                current
            }
        };
        snapshot.send_replace(Some(next));
        let _ = events.send(event);
        return Ok(());
    }
    let response: Response =
        serde_json::from_str(text).map_err(|e| ClientError::Protocol(e.to_string()))?;
    if let Some(sender) = pending.lock().await.remove(response.id()) {
        let result = match &response {
            Response::Success { .. } => Ok(response),
            Response::Error { error, .. } => {
                Err(ClientError::Remote(error.code, error.message.clone()))
            }
        };
        let _ = sender.send(result);
    }
    Ok(())
}

fn apply_target_change(snapshot: &mut Snapshot, change: &TargetChanged) -> Result<(), ClientError> {
    let target = snapshot
        .targets
        .iter_mut()
        .find(|target| target.name == change.target)
        .ok_or_else(|| {
            ClientError::Protocol(format!(
                "target change references unknown target `{}`",
                change.target
            ))
        })?;
    if change.revision != target.revision + 1 {
        return Err(ClientError::Protocol(format!(
            "target `{}` revision gap: expected {}, received {}",
            change.target,
            target.revision + 1,
            change.revision
        )));
    }
    let fields = change.changed.as_object().ok_or_else(|| {
        ClientError::Protocol("target change fields must be a JSON object".into())
    })?;
    for (field, value) in fields {
        match field.as_str() {
            "disabled" => target.disabled = required_bool(field, value)?,
            "stopped" => target.stopped = required_bool(field, value)?,
            "online" => target.online = required_bool(field, value)?,
            "channel" => {
                target.channel = optional_string(field, value)?;
            }
            "paused" => target.state.paused = optional_bool(field, value)?,
            "muted" => target.state.muted = optional_bool(field, value)?,
            "volume" => target.state.volume = optional_number(field, value)?,
            "title" => target.state.title = optional_string(field, value)?,
            "playlist_position" => {
                target.state.playlist_position = optional_u32(field, value)?;
            }
            "playlist_count" => target.state.playlist_count = optional_u32(field, value)?,
            "idle_active" => target.state.idle_active = optional_bool(field, value)?,
            "duration" => target.state.duration = optional_number(field, value)?,
            "position" => target.state.position = optional_number(field, value)?,
            "loop_file" => target.state.loop_file = optional_string(field, value)?,
            "loop_playlist" => target.state.loop_playlist = optional_string(field, value)?,
            "audio_track" => target.state.audio_track = optional_track(field, value)?,
            "subtitle_track" => target.state.subtitle_track = optional_track(field, value)?,
            "subtitles_visible" => {
                target.state.subtitles_visible = optional_bool(field, value)?;
            }
            "last_end_reason" => {
                target.state.last_end_reason = optional_end_reason(field, value)?;
            }
            "last_end_error" => {
                target.state.last_end_error = optional_string(field, value)?;
            }
            _ => {
                return Err(ClientError::Protocol(format!(
                    "unknown target change field `{field}`"
                )));
            }
        }
    }
    target.revision = change.revision;
    refresh_health(snapshot);
    Ok(())
}

fn required_bool(field: &str, value: &serde_json::Value) -> Result<bool, ClientError> {
    value.as_bool().ok_or_else(|| invalid_change_field(field))
}

fn optional_bool(field: &str, value: &serde_json::Value) -> Result<Option<bool>, ClientError> {
    if value.is_null() {
        Ok(None)
    } else {
        required_bool(field, value).map(Some)
    }
}

fn optional_number(field: &str, value: &serde_json::Value) -> Result<Option<f64>, ClientError> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_f64()
            .map(Some)
            .ok_or_else(|| invalid_change_field(field))
    }
}

fn optional_u32(field: &str, value: &serde_json::Value) -> Result<Option<u32>, ClientError> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .map(Some)
        .ok_or_else(|| invalid_change_field(field))
}

fn optional_string(field: &str, value: &serde_json::Value) -> Result<Option<String>, ClientError> {
    if value.is_null() {
        Ok(None)
    } else {
        value
            .as_str()
            .map(|value| Some(value.to_owned()))
            .ok_or_else(|| invalid_change_field(field))
    }
}

fn optional_track(field: &str, value: &serde_json::Value) -> Result<Option<String>, ClientError> {
    if value.is_null() {
        Ok(None)
    } else if let Some(value) = value.as_str() {
        Ok(Some(value.to_owned()))
    } else if let Some(value) = value.as_i64() {
        Ok(Some(value.to_string()))
    } else {
        Err(invalid_change_field(field))
    }
}

fn optional_end_reason(
    field: &str,
    value: &serde_json::Value,
) -> Result<Option<MediaEndReason>, ClientError> {
    if value.is_null() {
        return Ok(None);
    }
    serde_json::from_value(value.clone())
        .map(Some)
        .map_err(|_| invalid_change_field(field))
}

fn invalid_change_field(field: &str) -> ClientError {
    ClientError::Protocol(format!("invalid value for target change field `{field}`"))
}

fn refresh_health(snapshot: &mut Snapshot) {
    let expected = snapshot
        .targets
        .iter()
        .filter(|target| !target.disabled && !target.stopped)
        .count() as u32;
    let online = snapshot
        .targets
        .iter()
        .filter(|target| !target.disabled && !target.stopped && target.online)
        .count() as u32;
    snapshot.health.targets_configured = snapshot.targets.len() as u32;
    snapshot.health.targets_expected_online = expected;
    snapshot.health.targets_online = online;
    snapshot.health.state = if expected == 0 {
        HealthState::Idle
    } else if expected == online {
        HealthState::Ready
    } else {
        HealthState::Degraded
    };
}

#[derive(Debug)]
struct AcceptAnyServerCertificate {
    signature_schemes: Vec<SignatureScheme>,
}

impl ServerCertVerifier for AcceptAnyServerCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.signature_schemes.clone()
    }
}

struct Endpoint {
    host: String,
    port: u16,
}
impl Endpoint {
    fn parse(value: &str) -> Result<Self, ClientError> {
        let rest = value
            .strip_prefix("wss://")
            .ok_or(ClientError::InvalidUrl)?;
        let authority = rest.split('/').next().unwrap_or(rest);
        let (host, port) = authority
            .rsplit_once(':')
            .ok_or_else(|| ClientError::Url("URL must include host and port".into()))?;
        let port = port
            .parse()
            .map_err(|_| ClientError::Url("invalid port".into()))?;
        if host.is_empty() {
            return Err(ClientError::Url("URL must include host".into()));
        }
        Ok(Self {
            host: host.trim_matches('[').trim_matches(']').into(),
            port,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Health, TargetRecord, TargetState};
    use serde_json::json;

    fn target_with_status(
        disabled: bool,
        stopped: bool,
        online: bool,
        idle: Option<bool>,
        paused: Option<bool>,
    ) -> TargetRecord {
        TargetRecord {
            name: "test".into(),
            disabled,
            stopped,
            online,
            channel: None,
            state: TargetState {
                idle_active: idle,
                paused,
                ..TargetState::default()
            },
            revision: 0,
        }
    }

    #[test]
    fn derived_target_status_has_one_canonical_precedence() {
        assert_eq!(
            target_status(&target_with_status(
                true,
                true,
                false,
                Some(true),
                Some(true)
            )),
            TargetStatus::Disabled
        );
        assert_eq!(
            target_status(&target_with_status(
                false,
                true,
                false,
                Some(true),
                Some(true)
            )),
            TargetStatus::Stopped
        );
        assert_eq!(
            target_status(&target_with_status(
                false,
                false,
                false,
                Some(true),
                Some(true)
            )),
            TargetStatus::Offline
        );
        assert_eq!(
            target_status(&target_with_status(
                false,
                false,
                true,
                Some(true),
                Some(true)
            )),
            TargetStatus::Idle
        );
        assert_eq!(
            target_status(&target_with_status(
                false,
                false,
                true,
                Some(false),
                Some(true)
            )),
            TargetStatus::Paused
        );
        assert_eq!(
            target_status(&target_with_status(
                false,
                false,
                true,
                Some(false),
                Some(false)
            )),
            TargetStatus::Playing
        );
    }

    #[test]
    fn stable_success_shapes_decode_to_public_types() {
        let response = Response::Success {
            id: "test".into(),
            data: json!({
                "name": "music",
                "disabled": false,
                "online": true,
                "stopped": false,
                "channel": "/srv/channels/music.m3u8",
                "copied": ["mpv.conf"]
            }),
        };
        assert_eq!(
            decode_success::<AddTargetResult>(response).unwrap(),
            AddTargetResult {
                name: "music".into(),
                disabled: false,
                online: true,
                stopped: false,
                channel: Some("/srv/channels/music.m3u8".into()),
                copied: vec!["mpv.conf".into()],
            }
        );

        let response = Response::Success {
            id: "playlist".into(),
            data: json!([{
                "id": 42,
                "filename": "https://example.test/live.m3u8",
                "title": "Live",
                "current": true
            }]),
        };
        assert_eq!(
            decode_success::<Vec<PlaylistItem>>(response).unwrap(),
            vec![PlaylistItem {
                id: 42,
                filename: "https://example.test/live.m3u8".into(),
                title: Some("Live".into()),
                current: true,
            }]
        );
    }

    #[test]
    fn target_changes_update_cached_state_and_enforce_revisions() {
        let mut snapshot = Snapshot {
            protocol_version: PROTOCOL_VERSION,
            node_id: "test".into(),
            health: Health {
                state: HealthState::Ready,
                targets_configured: 1,
                targets_expected_online: 1,
                targets_online: 1,
            },
            targets: vec![TargetRecord {
                name: "music".into(),
                disabled: false,
                stopped: false,
                online: true,
                channel: None,
                state: TargetState::default(),
                revision: 4,
            }],
        };
        apply_target_change(
            &mut snapshot,
            &TargetChanged {
                target: "music".into(),
                revision: 5,
                changed: json!({"stopped": true, "online": false, "paused": true}),
            },
        )
        .unwrap();

        let target = &snapshot.targets[0];
        assert!(target.stopped);
        assert!(!target.online);
        assert_eq!(target.state.paused, Some(true));
        assert_eq!(target.revision, 5);
        assert_eq!(snapshot.health.state, HealthState::Idle);

        let error = apply_target_change(
            &mut snapshot,
            &TargetChanged {
                target: "music".into(),
                revision: 7,
                changed: json!({"muted": true}),
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("revision gap"));
    }

    #[test]
    fn typed_target_values_match_mpvs_wire_values() {
        assert_eq!(LoadMode::Replace.as_str(), "replace");
        assert_eq!(LoadMode::Append.as_str(), "append");
        assert_eq!(LoadMode::AppendPlay.as_str(), "append-play");
        assert_eq!(LoopState::Off.as_mpv_value(), "no");
        assert_eq!(LoopState::On.as_mpv_value(), "inf");
        assert_eq!(Track::Auto.as_value(), json!("auto"));
        assert_eq!(Track::Disabled.as_value(), json!("no"));
        assert_eq!(Track::Id(3).as_value(), json!(3));
    }

    #[test]
    fn target_changes_accept_media_end_observation() {
        let mut snapshot = Snapshot {
            protocol_version: PROTOCOL_VERSION,
            node_id: "test".into(),
            health: Health {
                state: HealthState::Ready,
                targets_configured: 1,
                targets_expected_online: 1,
                targets_online: 1,
            },
            targets: vec![TargetRecord {
                name: "music".into(),
                disabled: false,
                stopped: false,
                online: true,
                channel: None,
                state: TargetState::default(),
                revision: 0,
            }],
        };
        apply_target_change(
            &mut snapshot,
            &TargetChanged {
                target: "music".into(),
                revision: 1,
                changed: json!({"last_end_reason": "error", "last_end_error": "network"}),
            },
        )
        .unwrap();
        assert_eq!(
            snapshot.targets[0].state.last_end_reason,
            Some(MediaEndReason::Error)
        );
        assert_eq!(
            snapshot.targets[0].state.last_end_error.as_deref(),
            Some("network")
        );
    }
}
