//! The shared native WebSocket client used by `targets`, PaneBot, and other
//! clients.  It owns transport, request correlation, the current snapshot,
//! and event delivery; reconnection remains the caller's decision.

use crate::protocol::PROTOCOL_VERSION;
use crate::{Event, HealthState, Operation, Request, Response, Snapshot, TargetChanged};
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
    sync::{RwLock, broadcast, mpsc, oneshot},
    time::{Duration, sleep, timeout},
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

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

enum ClientCommand {
    Request(Request),
    Close(oneshot::Sender<()>),
}

#[derive(Clone)]
struct ConnectionState {
    pending: PendingMap,
    events: broadcast::Sender<Event>,
    snapshot: Arc<RwLock<Option<Snapshot>>>,
    disconnected: Arc<AtomicBool>,
    disconnect_error: Arc<RwLock<Option<ClientError>>>,
}

#[derive(Clone, Debug)]
pub struct ClientOptions {
    pub url: String,
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
    snapshot: Arc<RwLock<Option<Snapshot>>>,
    events: broadcast::Sender<Event>,
    disconnected: Arc<AtomicBool>,
    disconnect_error: Arc<RwLock<Option<ClientError>>>,
    next_id: Arc<AtomicU64>,
}

impl RemoteClient {
    pub async fn connect(options: ClientOptions) -> Result<Self, ClientError> {
        let endpoint = Endpoint::parse(&options.url)?;
        let request = options
            .url
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
        let snapshot = Arc::new(RwLock::new(None));
        let disconnected = Arc::new(AtomicBool::new(false));
        let disconnect_error = Arc::new(RwLock::new(None));
        let pending: PendingMap =
            Arc::new(tokio::sync::Mutex::new(std::collections::HashMap::new()));
        let connection_state = ConnectionState {
            pending: pending.clone(),
            events: events.clone(),
            snapshot: snapshot.clone(),
            disconnected: disconnected.clone(),
            disconnect_error: disconnect_error.clone(),
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
            next_id: Arc::new(AtomicU64::new(1)),
        };
        for _ in 0..500 {
            if client.snapshot.read().await.is_some() {
                return Ok(client);
            }
            if client.disconnected() {
                return Err(client
                    .disconnect_error()
                    .await
                    .unwrap_or(ClientError::Disconnected));
            }
            sleep(Duration::from_millis(10)).await;
        }
        Err(ClientError::Timeout)
    }

    pub async fn snapshot(&self) -> Option<Snapshot> {
        self.snapshot.read().await.clone()
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

    pub async fn list_channels(&self) -> Result<Response, ClientError> {
        self.request_inner(None, Operation::NodeListChannels).await
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
        match timeout(REQUEST_TIMEOUT, receive).await {
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

    pub async fn add(&self, from: Option<String>, enable: bool) -> Result<Response, ClientError> {
        self.request(Operation::TargetAdd { from, enable }).await
    }
    pub async fn remove_target(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetRemove).await
    }
    pub async fn start(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetStart).await
    }
    pub async fn stop_target(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetStop).await
    }
    pub async fn restart(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetRestart).await
    }
    pub async fn enable(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetEnable).await
    }
    pub async fn disable(&self) -> Result<Response, ClientError> {
        self.request(Operation::TargetDisable).await
    }
    pub async fn rename(&self, replacement: impl Into<String>) -> Result<Response, ClientError> {
        self.request(Operation::TargetRename {
            name: replacement.into(),
        })
        .await
    }
    pub async fn set_channel(
        &self,
        channel: Option<String>,
        restart: bool,
    ) -> Result<Response, ClientError> {
        self.request(Operation::TargetSetChannel { channel, restart })
            .await
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
    pub async fn playlist(&self) -> Result<Response, ClientError> {
        self.mpv("get_property", vec![serde_json::json!("playlist")])
            .await
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
    snapshot: &Arc<RwLock<Option<Snapshot>>>,
) -> Result<(), ClientError> {
    if let Ok(event) = serde_json::from_str::<Event>(text) {
        {
            let mut cached = snapshot.write().await;
            match &event {
                Event::Snapshot { snapshot: value } => {
                    if value.protocol_version != PROTOCOL_VERSION {
                        return Err(ClientError::Protocol(format!(
                            "unsupported protocol version {}; expected {PROTOCOL_VERSION}",
                            value.protocol_version
                        )));
                    }
                    *cached = Some(value.clone());
                }
                Event::TargetChanged(change) => {
                    let current = cached.as_mut().ok_or_else(|| {
                        ClientError::Protocol("target change received before snapshot".into())
                    })?;
                    apply_target_change(current, change)?;
                }
            }
        }
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
}
