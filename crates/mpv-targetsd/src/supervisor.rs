use mpv_targets::{
    DaemonConfig, Event, Health, HealthState, TargetChanged, TargetConfig, TargetRecord,
    TargetState,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    process::Command,
    sync::{Mutex, broadcast},
    time::sleep,
};

const IPC_WAIT: Duration = Duration::from_secs(5);
const RECOVERY_ATTEMPTS: u8 = 5;
const OBSERVED_PROPERTIES: &[(&str, &str)] = &[
    ("pause", "paused"),
    ("mute", "muted"),
    ("volume", "volume"),
    ("media-title", "title"),
    ("playlist-pos", "playlist_position"),
    ("playlist-count", "playlist_count"),
    ("idle-active", "idle_active"),
    ("duration", "duration"),
    ("time-pos", "position"),
    ("loop-file", "loop_file"),
    ("loop-playlist", "loop_playlist"),
    ("aid", "audio_track"),
    ("sid", "subtitle_track"),
    ("sub-visibility", "subtitles_visible"),
];

fn loop_state(value: &Value) -> Option<String> {
    let enabled = match value {
        Value::Bool(enabled) => *enabled,
        Value::Number(number) => number.as_i64()? != 0,
        Value::String(value) => !matches!(value.as_str(), "no" | "off" | "false" | "0"),
        _ => return None,
    };
    Some(if enabled { "on" } else { "off" }.into())
}

fn normalize_observed_value(field: &str, value: Value) -> Value {
    if matches!(field, "loop_file" | "loop_playlist")
        && let Some(state) = loop_state(&value)
    {
        return json!(state);
    }
    if field == "position"
        && let Some(position) = value.as_f64()
    {
        return json!(position.floor());
    }
    value
}

fn mpv_command_values(command: &str, args: &[Value]) -> Vec<Value> {
    let mut values = Vec::with_capacity(args.len() + 2);
    if command == "show-text"
        && args
            .first()
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with("${osd-ass-cc/0}"))
    {
        values.push(Value::String("expand-properties".into()));
    }
    values.push(Value::String(command.to_owned()));
    values.extend_from_slice(args);
    values
}

fn copy_directory(source: &Path, destination: &Path) -> io::Result<()> {
    if !source.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let target = destination.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            fs::create_dir_all(&target)?;
            copy_directory(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target)?;
        }
    }
    Ok(())
}

/// Portable target supervision.  It relies only on Unix-domain sockets, which
/// are available on both Linux and macOS; systemd is never consulted here.
#[derive(Clone)]
pub struct Supervisor {
    inner: Arc<Mutex<Inner>>,
    config_path: PathBuf,
    config_root: PathBuf,
    runtime_root: PathBuf,
    changes: broadcast::Sender<Event>,
}

struct Inner {
    config: DaemonConfig,
    targets: BTreeMap<String, RuntimeTarget>,
}

struct RuntimeTarget {
    config: TargetConfig,
    observed: TargetState,
    stopped: bool,
    online: bool,
    launching: bool,
    revision: u64,
    recovery_attempt: u8,
}

#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("unknown target: {0}")]
    UnknownTarget(String),
    #[error("target is disabled; enable it before starting")]
    TargetDisabled,
    #[error("target is already stopped")]
    TargetAlreadyStopped,
    #[error("target is not running")]
    TargetNotRunning,
    #[error("target must be stopped before rename")]
    RenameRequiresStopped,
    #[error("mpv did not create its configured IPC socket: {0}")]
    IpcUnavailable(String),
    #[error("mpv rejected the command: {0}")]
    MpvRejected(String),
    #[error("cannot launch mpv: {0}")]
    Launch(#[source] io::Error),
    #[error("cannot persist configuration: {0}")]
    Config(#[source] io::Error),
    #[error("configuration serialization failed: {0}")]
    ConfigSerialization(#[source] toml::ser::Error),
    #[error("target already exists: {0}")]
    TargetExists(String),
}

impl Supervisor {
    pub fn new(config: DaemonConfig, config_path: PathBuf) -> Self {
        let config_root = config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_owned();
        let targets = config
            .targets
            .iter()
            .cloned()
            .map(|target| {
                let stopped = target.disabled;
                (
                    target.name.clone(),
                    RuntimeTarget {
                        config: target,
                        observed: TargetState::default(),
                        stopped,
                        online: false,
                        launching: false,
                        revision: 0,
                        recovery_attempt: 0,
                    },
                )
            })
            .collect();
        let runtime_root = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| config_root.join("runtime"))
            .join("mpv-targets");
        let (changes, _) = broadcast::channel(128);
        Self {
            inner: Arc::new(Mutex::new(Inner { config, targets })),
            config_path,
            config_root,
            runtime_root,
            changes,
        }
    }

    pub async fn start_configured_targets(&self) -> Result<(), SupervisorError> {
        let targets: Vec<_> = self.inner.lock().await.targets.keys().cloned().collect();
        for name in targets {
            let disabled = self.inner.lock().await.targets[&name].config.disabled;
            if disabled {
                self.ensure_disabled_off(&name).await?;
            } else if self.socket_is_live(&name).await {
                self.mark_online(&name).await;
            } else if let Err(error) = self.start(&name).await {
                eprintln!("mpv-targetsd: target `{name}` initial launch failed: {error}");
                self.schedule_recovery(name);
            }
        }
        Ok(())
    }

    pub async fn snapshot_and_subscribe(
        &self,
    ) -> (mpv_targets::Snapshot, broadcast::Receiver<Event>) {
        let inner = self.inner.lock().await;
        let events = self.changes.subscribe();
        (Self::snapshot_from(&inner), events)
    }

    fn snapshot_from(inner: &Inner) -> mpv_targets::Snapshot {
        let targets: Vec<_> = inner
            .targets
            .iter()
            .map(|(name, target)| TargetRecord {
                name: name.clone(),
                disabled: target.config.disabled,
                stopped: target.stopped,
                online: target.online,
                channel: target.config.channel.clone(),
                state: target.observed.clone(),
                revision: target.revision,
            })
            .collect();
        let expected = targets
            .iter()
            .filter(|target| !target.disabled && !target.stopped)
            .count() as u32;
        let online = targets
            .iter()
            .filter(|target| !target.disabled && !target.stopped && target.online)
            .count() as u32;
        let health = Health {
            state: if expected == 0 {
                HealthState::Idle
            } else if expected == online {
                HealthState::Ready
            } else {
                HealthState::Degraded
            },
            targets_configured: targets.len() as u32,
            targets_expected_online: expected,
            targets_online: online,
        };
        mpv_targets::Snapshot {
            protocol_version: mpv_targets::protocol::PROTOCOL_VERSION,
            node_id: inner.config.node.id.clone(),
            health,
            targets,
        }
    }

    pub async fn start(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        let should_launch = {
            let mut inner = self.inner.lock().await;
            let target = inner
                .targets
                .get_mut(name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            if target.config.disabled {
                return Err(SupervisorError::TargetDisabled);
            }
            if target.stopped {
                target.stopped = false;
                target.revision += 1;
                self.emit_changed(name, target.revision, json!({"stopped": false}));
            }
            if target.online || target.launching {
                false
            } else {
                target.launching = true;
                true
            }
        };
        if should_launch && let Err(error) = self.launch(name).await {
            self.clear_launching(name).await;
            return Err(error);
        }
        self.target_record(name).await
    }

    pub async fn stop(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        let was_online = {
            let mut inner = self.inner.lock().await;
            let target = inner
                .targets
                .get_mut(name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            if target.stopped {
                return Err(SupervisorError::TargetAlreadyStopped);
            }
            target.stopped = true;
            target.revision += 1;
            self.emit_changed(name, target.revision, json!({"stopped": true}));
            target.online
        };
        if was_online {
            self.ipc_command(name, json!({"command": ["quit"]})).await?;
            self.wait_for_socket_gone(name).await?;
        }
        self.clear_observed(name).await;
        self.mark_offline(name).await;
        self.target_record(name).await
    }

    pub async fn restart(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        let disabled = self.target_disabled(name).await?;
        if disabled {
            self.ensure_disabled_off(name).await?;
            return self.target_record(name).await;
        }
        if !self.target_stopped(name).await? {
            self.stop(name).await?;
        }
        self.start(name).await
    }

    pub async fn enable(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        self.persist_target_change(name, |target| {
            if target.disabled {
                target.disabled = false;
                Some(json!({"disabled": false}))
            } else {
                None
            }
        })
        .await?;
        self.target_record(name).await
    }

    pub async fn disable(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        if !self.target_stopped(name).await? {
            self.stop(name).await?;
        }
        self.persist_target_change(name, |target| {
            if target.disabled {
                None
            } else {
                target.disabled = true;
                Some(json!({"disabled": true}))
            }
        })
        .await?;
        self.target_record(name).await
    }

    pub async fn set_channel(
        &self,
        name: &str,
        channel: Option<String>,
    ) -> Result<TargetRecord, SupervisorError> {
        self.persist_target_change(name, |target| {
            if target.channel == channel {
                None
            } else {
                target.channel = channel.clone();
                Some(json!({"channel": channel}))
            }
        })
        .await?;
        self.target_record(name).await
    }

    pub async fn add(
        &self,
        name: &str,
        from: Option<&str>,
        enable: bool,
    ) -> Result<TargetRecord, SupervisorError> {
        let (target_dir, source_dir, source_channel) = {
            let inner = self.inner.lock().await;
            if inner.targets.contains_key(name) {
                return Err(SupervisorError::TargetExists(name.into()));
            }
            let (source_dir, source_channel) = match from {
                Some(source) if inner.targets.contains_key(source) => (
                    Some(self.config_root.join("targets").join(source)),
                    inner
                        .targets
                        .get(source)
                        .and_then(|target| target.config.channel.clone()),
                ),
                Some(source) => return Err(SupervisorError::UnknownTarget(source.into())),
                None => (None, None),
            };
            (
                self.config_root.join("targets").join(name),
                source_dir,
                source_channel,
            )
        };
        if target_dir.exists() {
            return Err(SupervisorError::TargetExists(name.into()));
        }
        let preparation = (|| -> io::Result<()> {
            fs::create_dir_all(target_dir.join("scripts"))?;
            if let Some(source) = source_dir {
                fs::copy(source.join("mpv.conf"), target_dir.join("mpv.conf")).map(|_| ())?;
                copy_directory(&source.join("scripts"), &target_dir.join("scripts"))?;
            } else {
                fs::write(
                    target_dir.join("mpv.conf"),
                    mpv_targets::base_mpv_conf(true, true, 100.0),
                )?;
            }
            Ok(())
        })();
        if let Err(error) = preparation {
            let _ = fs::remove_dir_all(&target_dir);
            return Err(SupervisorError::Config(error));
        }
        let disabled = !enable;
        let target = TargetConfig {
            name: name.into(),
            disabled,
            channel: source_channel,
        };
        let candidate = {
            let inner = self.inner.lock().await;
            let mut candidate = inner.config.clone();
            candidate.targets.push(target.clone());
            candidate
        };
        if let Err(error) = candidate
            .validate()
            .map_err(|error| SupervisorError::MpvRejected(error.to_string()))
            .and_then(|_| self.persist_config(&candidate))
        {
            let _ = fs::remove_dir_all(&target_dir);
            return Err(error);
        }
        {
            let mut inner = self.inner.lock().await;
            inner.config = candidate;
            inner.targets.insert(
                name.into(),
                RuntimeTarget {
                    config: target,
                    observed: TargetState::default(),
                    stopped: true,
                    online: false,
                    launching: false,
                    revision: 0,
                    recovery_attempt: 0,
                },
            );
            self.emit_snapshot(&inner);
        }
        self.target_record(name).await
    }

    pub async fn remove(&self, name: &str) -> Result<(), SupervisorError> {
        let was_stopped = {
            let inner = self.inner.lock().await;
            inner
                .targets
                .get(name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?
                .stopped
        };
        if !was_stopped {
            self.stop(name).await?;
        }
        let directory = self.config_root.join("targets").join(name);
        let temporary = self
            .config_root
            .join("targets")
            .join(format!(".remove-{name}-{}", std::process::id()));
        if directory.exists() {
            fs::rename(&directory, &temporary).map_err(SupervisorError::Config)?;
        }
        let candidate = {
            let inner = self.inner.lock().await;
            let mut candidate = inner.config.clone();
            candidate.targets.retain(|target| target.name != name);
            candidate
        };
        if let Err(error) = candidate
            .validate()
            .map_err(|error| SupervisorError::MpvRejected(error.to_string()))
            .and_then(|_| self.persist_config(&candidate))
        {
            if temporary.exists() {
                let _ = fs::rename(&temporary, &directory);
            }
            return Err(error);
        }
        {
            let mut inner = self.inner.lock().await;
            inner.config = candidate;
            inner.targets.remove(name);
            self.emit_snapshot(&inner);
        }
        if temporary.exists() {
            fs::remove_dir_all(temporary).map_err(SupervisorError::Config)?;
        }
        Ok(())
    }

    pub async fn rename(
        &self,
        name: &str,
        replacement: &str,
    ) -> Result<TargetRecord, SupervisorError> {
        {
            let inner = self.inner.lock().await;
            let target = inner
                .targets
                .get(name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            if inner.targets.contains_key(replacement) {
                return Err(SupervisorError::TargetExists(replacement.into()));
            }
            if !target.stopped {
                return Err(SupervisorError::RenameRequiresStopped);
            }
        }
        let old_dir = self.config_root.join("targets").join(name);
        let new_dir = self.config_root.join("targets").join(replacement);
        if new_dir.exists() {
            return Err(SupervisorError::TargetExists(replacement.into()));
        }
        fs::rename(&old_dir, &new_dir).map_err(SupervisorError::Config)?;
        let candidate = {
            let inner = self.inner.lock().await;
            let mut candidate = inner.config.clone();
            let target = candidate
                .targets
                .iter_mut()
                .find(|target| target.name == name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            target.name = replacement.into();
            candidate
        };
        if let Err(error) = candidate
            .validate()
            .map_err(|error| SupervisorError::MpvRejected(error.to_string()))
            .and_then(|_| self.persist_config(&candidate))
        {
            let _ = fs::rename(&new_dir, &old_dir);
            return Err(error);
        }
        {
            let mut inner = self.inner.lock().await;
            inner.config = candidate;
            let mut runtime = inner.targets.remove(name).expect("checked target exists");
            runtime.config.name = replacement.into();
            runtime.revision += 1;
            inner.targets.insert(replacement.into(), runtime);
            self.emit_snapshot(&inner);
        }
        self.target_record(replacement).await
    }

    pub fn list_channels(&self) -> Result<Vec<String>, SupervisorError> {
        let directory = self.config_root.join("channels");
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(SupervisorError::Config(error)),
        };
        let mut channels = Vec::new();
        for entry in entries {
            let entry = entry.map_err(SupervisorError::Config)?;
            if entry
                .file_type()
                .map_err(SupervisorError::Config)?
                .is_file()
                && is_channel_path(&entry.path())
            {
                channels.push(entry.path().to_string_lossy().into_owned());
            }
        }
        channels.sort();
        Ok(channels)
    }

    pub async fn mpv_command(
        &self,
        name: &str,
        command: &str,
        args: &[Value],
    ) -> Result<Value, SupervisorError> {
        if !self.target_online(name).await? {
            return Err(SupervisorError::TargetNotRunning);
        }
        let values = mpv_command_values(command, args);
        self.ipc_command(name, json!({"command": values})).await
    }

    async fn launch(&self, name: &str) -> Result<(), SupervisorError> {
        let config = self.target_config(name).await?;
        let socket = self.socket_path(name);
        if self.socket_is_live(name).await {
            self.mark_online(name).await;
            return Ok(());
        }
        if tokio::fs::try_exists(&socket).await.unwrap_or(false) {
            tokio::fs::remove_file(&socket)
                .await
                .map_err(SupervisorError::Launch)?;
        }
        tokio::fs::create_dir_all(&self.runtime_root)
            .await
            .map_err(SupervisorError::Launch)?;
        let target_directory = self.config_root.join("targets").join(name);
        let logs_directory = self.config_root.join("logs");
        fs::create_dir_all(&logs_directory).map_err(SupervisorError::Launch)?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(logs_directory.join(format!("{name}.log")))
            .map_err(SupervisorError::Launch)?;
        let log_stdout = log.try_clone().map_err(SupervisorError::Launch)?;
        let mut command = Command::new("mpv");
        command
            .arg(format!("--input-ipc-server={}", socket.display()))
            .arg(format!("--config-dir={}", target_directory.display()))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_stdout))
            .stderr(Stdio::from(log));
        if let Some(channel) = config.channel {
            command.arg(channel);
        }
        let mut child = command.spawn().map_err(SupervisorError::Launch)?;
        let wait_result = self.wait_for_socket(name).await;
        if let Err(error) = wait_result {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return Err(error);
        }
        self.mark_online(name).await;
        tokio::spawn(async move {
            let _ = child.wait().await;
        });
        Ok(())
    }

    async fn on_target_lost(&self, name: String) {
        let should_recover = {
            let mut inner = self.inner.lock().await;
            let Some(target) = inner.targets.get_mut(&name) else {
                return;
            };
            if !target.online {
                return;
            }
            target.online = false;
            target.launching = false;
            target.revision += 1;
            self.emit_changed(&name, target.revision, json!({"online": false}));
            !target.config.disabled && !target.stopped
        };
        self.clear_observed(&name).await;
        if should_recover {
            self.schedule_recovery(name);
        }
    }

    fn schedule_recovery(&self, name: String) {
        let supervisor = self.clone();
        tokio::spawn(async move {
            let should_recover = {
                let mut inner = supervisor.inner.lock().await;
                let Some(target) = inner.targets.get_mut(&name) else {
                    return;
                };
                if target.config.disabled || target.stopped || target.recovery_attempt != 0 {
                    false
                } else {
                    target.recovery_attempt = 1;
                    true
                }
            };
            if !should_recover {
                return;
            }
            for attempt in 1..=RECOVERY_ATTEMPTS {
                sleep(Duration::from_secs(1_u64 << (attempt - 1))).await;
                let still_expected = {
                    let mut inner = supervisor.inner.lock().await;
                    let Some(target) = inner.targets.get_mut(&name) else {
                        return;
                    };
                    if target.config.disabled || target.stopped {
                        target.recovery_attempt = 0;
                        false
                    } else {
                        true
                    }
                };
                if !still_expected {
                    return;
                }
                // Box the recursive observer/recovery launch cycle.
                if Box::pin(supervisor.start(&name)).await.is_ok() {
                    return;
                }
                eprintln!("mpv-targetsd: target `{name}` recovery attempt {attempt} failed");
                if let Some(target) = supervisor.inner.lock().await.targets.get_mut(&name) {
                    target.recovery_attempt = attempt.saturating_add(1);
                }
            }
        });
    }

    async fn ensure_disabled_off(&self, name: &str) -> Result<(), SupervisorError> {
        if self.socket_is_live(name).await {
            self.ipc_command(name, json!({"command": ["quit"]})).await?;
            self.wait_for_socket_gone(name).await?;
        }
        self.clear_observed(name).await;
        {
            let mut inner = self.inner.lock().await;
            let target = inner
                .targets
                .get_mut(name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            let mut changed = serde_json::Map::new();
            if !target.stopped {
                target.stopped = true;
                changed.insert("stopped".into(), Value::Bool(true));
            }
            if target.online {
                target.online = false;
                changed.insert("online".into(), Value::Bool(false));
            }
            target.launching = false;
            if !changed.is_empty() {
                target.revision += 1;
                self.emit_changed(name, target.revision, Value::Object(changed));
            }
        }
        Ok(())
    }

    async fn clear_observed(&self, name: &str) {
        let changed = {
            let mut inner = self.inner.lock().await;
            let Some(target) = inner.targets.get_mut(name) else {
                return;
            };
            if target.observed == TargetState::default() {
                None
            } else {
                let changed = serde_json::to_value(&target.observed)
                    .expect("target state must serialize")
                    .as_object()
                    .expect("target state must serialize as an object")
                    .iter()
                    .filter(|(_, value)| !value.is_null())
                    .map(|(field, _)| (field.clone(), Value::Null))
                    .collect();
                target.observed = TargetState::default();
                target.revision += 1;
                Some((target.revision, Value::Object(changed)))
            }
        };
        if let Some((revision, changed)) = changed {
            self.emit_changed(name, revision, changed);
        }
    }

    async fn mark_online(&self, name: &str) {
        let changed = {
            let mut inner = self.inner.lock().await;
            let Some(target) = inner.targets.get_mut(name) else {
                return;
            };
            if target.online {
                false
            } else {
                target.online = true;
                target.launching = false;
                target.recovery_attempt = 0;
                target.revision += 1;
                self.emit_changed(name, target.revision, json!({"online": true}));
                true
            }
        };
        if !changed {
            return;
        }
        let supervisor = self.clone();
        let target = name.to_owned();
        tokio::spawn(async move {
            supervisor.observe_target(target).await;
        });
    }

    async fn mark_offline(&self, name: &str) {
        {
            let mut inner = self.inner.lock().await;
            if let Some(target) = inner.targets.get_mut(name)
                && target.online
            {
                target.online = false;
                target.launching = false;
                target.revision += 1;
                self.emit_changed(name, target.revision, json!({"online": false}));
            }
        }
    }

    async fn clear_launching(&self, name: &str) {
        if let Some(target) = self.inner.lock().await.targets.get_mut(name) {
            target.launching = false;
        }
    }

    async fn persist_target_change(
        &self,
        name: &str,
        mutate: impl FnOnce(&mut TargetConfig) -> Option<Value>,
    ) -> Result<(), SupervisorError> {
        {
            let mut inner = self.inner.lock().await;
            let target_index = inner
                .config
                .targets
                .iter()
                .position(|target| target.name == name)
                .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
            let mut candidate = inner.config.clone();
            let Some(changed) = mutate(&mut candidate.targets[target_index]) else {
                return Ok(());
            };
            candidate
                .validate()
                .map_err(|error| SupervisorError::MpvRejected(error.to_string()))?;
            self.persist_config(&candidate)?;
            inner.config = candidate;
            let persisted_target = inner.config.targets[target_index].clone();
            let target = inner
                .targets
                .get_mut(name)
                .expect("configured target must have runtime state");
            target.config = persisted_target;
            target.revision += 1;
            self.emit_changed(name, target.revision, changed);
        }
        Ok(())
    }

    fn persist_config(&self, config: &DaemonConfig) -> Result<(), SupervisorError> {
        let source =
            toml::to_string_pretty(config).map_err(SupervisorError::ConfigSerialization)?;
        let temporary = self
            .config_path
            .with_extension(format!("toml.tmp-{}", std::process::id()));
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .map_err(SupervisorError::Config)?;
        if let Err(error) = (|| -> io::Result<()> {
            file.write_all(source.as_bytes())?;
            file.sync_all()?;
            fs::rename(&temporary, &self.config_path)?;
            Ok(())
        })() {
            let _ = fs::remove_file(&temporary);
            return Err(SupervisorError::Config(error));
        }
        Ok(())
    }

    fn emit_changed(&self, name: &str, revision: u64, changed: Value) {
        let _ = self.changes.send(Event::TargetChanged(TargetChanged {
            target: name.to_owned(),
            revision,
            changed,
        }));
    }

    fn emit_snapshot(&self, inner: &Inner) {
        let _ = self.changes.send(Event::Snapshot {
            snapshot: Self::snapshot_from(inner),
        });
    }

    async fn observe_target(&self, name: String) {
        let socket = self.socket_path(&name);
        let mut stream = match UnixStream::connect(socket).await {
            Ok(stream) => stream,
            Err(_) => {
                self.on_target_lost(name).await;
                return;
            }
        };
        for (id, (property, _)) in OBSERVED_PROPERTIES.iter().enumerate() {
            let command = json!({"command": ["observe_property", id + 1, property]});
            let line = serde_json::to_string(&command).expect("mpv command must be JSON");
            if stream.write_all(line.as_bytes()).await.is_err()
                || stream.write_all(b"\n").await.is_err()
            {
                self.on_target_lost(name).await;
                return;
            }
        }
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            if message.get("event").and_then(Value::as_str) != Some("property-change") {
                continue;
            }
            let Some(property) = message.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some((_, field)) = OBSERVED_PROPERTIES
                .iter()
                .find(|(observed, _)| *observed == property)
            else {
                continue;
            };
            let value = message.get("data").cloned().unwrap_or(Value::Null);
            self.update_observed(&name, field, value).await;
        }
        self.on_target_lost(name).await;
    }

    async fn update_observed(&self, name: &str, field: &str, value: Value) {
        let value = normalize_observed_value(field, value);
        let mut inner = self.inner.lock().await;
        let Some(target) = inner.targets.get_mut(name) else {
            return;
        };
        macro_rules! set_observed {
            ($slot:expr, $next:expr) => {{
                let slot = &mut $slot;
                let next = $next;
                let changed = slot.as_ref() != Some(&next);
                if changed {
                    *slot = Some(next);
                }
                changed
            }};
        }
        let changed = match field {
            "paused" => value
                .as_bool()
                .map(|value| set_observed!(target.observed.paused, value)),
            "muted" => value
                .as_bool()
                .map(|value| set_observed!(target.observed.muted, value)),
            "volume" => value
                .as_f64()
                .map(|value| set_observed!(target.observed.volume, value)),
            "title" => value
                .as_str()
                .map(|value| set_observed!(target.observed.title, value.to_owned())),
            "playlist_position" => value
                .as_i64()
                .and_then(|value| u32::try_from(value).ok())
                .map(|value| set_observed!(target.observed.playlist_position, value)),
            "playlist_count" => value
                .as_i64()
                .and_then(|value| u32::try_from(value).ok())
                .map(|value| set_observed!(target.observed.playlist_count, value)),
            "idle_active" => value
                .as_bool()
                .map(|value| set_observed!(target.observed.idle_active, value)),
            "duration" => value
                .as_f64()
                .map(|value| set_observed!(target.observed.duration, value)),
            "position" => value
                .as_f64()
                .map(|value| set_observed!(target.observed.position, value)),
            "loop_file" => {
                loop_state(&value).map(|value| set_observed!(target.observed.loop_file, value))
            }
            "loop_playlist" => {
                loop_state(&value).map(|value| set_observed!(target.observed.loop_playlist, value))
            }
            "audio_track" => value
                .as_i64()
                .map(|value| set_observed!(target.observed.audio_track, value.to_string())),
            "subtitle_track" => value
                .as_i64()
                .map(|value| set_observed!(target.observed.subtitle_track, value.to_string())),
            "subtitles_visible" => value
                .as_bool()
                .map(|value| set_observed!(target.observed.subtitles_visible, value)),
            _ => None,
        };
        let Some(changed) = changed else {
            return;
        };
        if !changed {
            return;
        }
        target.revision += 1;
        let mut changed_fields = serde_json::Map::new();
        changed_fields.insert(field.to_owned(), value);
        self.emit_changed(name, target.revision, Value::Object(changed_fields));
    }

    async fn target_record(&self, name: &str) -> Result<TargetRecord, SupervisorError> {
        let inner = self.inner.lock().await;
        let target = inner
            .targets
            .get(name)
            .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))?;
        Ok(TargetRecord {
            name: name.into(),
            disabled: target.config.disabled,
            stopped: target.stopped,
            online: target.online,
            channel: target.config.channel.clone(),
            state: target.observed.clone(),
            revision: target.revision,
        })
    }

    async fn target_config(&self, name: &str) -> Result<TargetConfig, SupervisorError> {
        self.inner
            .lock()
            .await
            .targets
            .get(name)
            .map(|target| target.config.clone())
            .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))
    }

    async fn target_disabled(&self, name: &str) -> Result<bool, SupervisorError> {
        self.inner
            .lock()
            .await
            .targets
            .get(name)
            .map(|target| target.config.disabled)
            .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))
    }

    async fn target_stopped(&self, name: &str) -> Result<bool, SupervisorError> {
        self.inner
            .lock()
            .await
            .targets
            .get(name)
            .map(|target| target.stopped)
            .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))
    }

    async fn target_online(&self, name: &str) -> Result<bool, SupervisorError> {
        self.inner
            .lock()
            .await
            .targets
            .get(name)
            .map(|target| target.online)
            .ok_or_else(|| SupervisorError::UnknownTarget(name.into()))
    }

    fn socket_path(&self, name: &str) -> PathBuf {
        self.runtime_root.join(format!("{name}.sock"))
    }

    async fn socket_is_live(&self, name: &str) -> bool {
        UnixStream::connect(self.socket_path(name)).await.is_ok()
    }

    async fn wait_for_socket(&self, name: &str) -> Result<(), SupervisorError> {
        let deadline = tokio::time::Instant::now() + IPC_WAIT;
        loop {
            if self.socket_is_live(name).await {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SupervisorError::IpcUnavailable(name.into()));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn wait_for_socket_gone(&self, name: &str) -> Result<(), SupervisorError> {
        let deadline = tokio::time::Instant::now() + IPC_WAIT;
        loop {
            if !self.socket_is_live(name).await {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(SupervisorError::IpcUnavailable(name.into()));
            }
            sleep(Duration::from_millis(50)).await;
        }
    }

    async fn ipc_command(&self, name: &str, command: Value) -> Result<Value, SupervisorError> {
        let socket = self.socket_path(name);
        let mut stream = UnixStream::connect(&socket)
            .await
            .map_err(|_| SupervisorError::IpcUnavailable(name.into()))?;
        let serialized = serde_json::to_string(&command).expect("mpv command must be JSON");
        stream
            .write_all(serialized.as_bytes())
            .await
            .map_err(|_| SupervisorError::IpcUnavailable(name.into()))?;
        stream
            .write_all(b"\n")
            .await
            .map_err(|_| SupervisorError::IpcUnavailable(name.into()))?;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .await
            .map_err(|_| SupervisorError::IpcUnavailable(name.into()))?;
        let response: Value = serde_json::from_str(&line)
            .map_err(|_| SupervisorError::IpcUnavailable(name.into()))?;
        if response
            .get("error")
            .and_then(Value::as_str)
            .is_some_and(|error| error != "success")
        {
            return Err(SupervisorError::MpvRejected(response.to_string()));
        }
        Ok(response.get("data").cloned().unwrap_or(Value::Null))
    }
}

fn is_channel_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| matches!(extension.to_ascii_lowercase().as_str(), "m3u" | "m3u8"))
}

#[allow(dead_code)]
fn _portable_unix_only(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configuration_mutations_copy_channel_and_leave_disabled_targets_stopped() {
        let root = std::env::temp_dir().join(format!(
            "mpv-targets-add-channel-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let source_dir = root.join("targets/source");
        fs::create_dir_all(source_dir.join("scripts")).unwrap();
        fs::write(source_dir.join("mpv.conf"), "idle=yes\n").unwrap();
        let config_path = root.join("mpv-targets.toml");
        let config = DaemonConfig::parse(
            r#"[node]
id = "test"
listen = "127.0.0.1:9876"
[tls]
certificate = "tls/server.crt"
private_key = "tls/server.key"
[[targets]]
name = "source"
channel = "/srv/channels/movies.m3u8"
"#,
        )
        .unwrap();
        fs::write(&config_path, toml::to_string_pretty(&config).unwrap()).unwrap();
        let supervisor = Supervisor::new(config, config_path);

        let added = supervisor.add("copy", Some("source"), false).await.unwrap();

        assert_eq!(added.channel.as_deref(), Some("/srv/channels/movies.m3u8"));
        let persisted = fs::read_to_string(root.join("mpv-targets.toml")).unwrap();
        assert!(persisted.contains("channel = \"/srv/channels/movies.m3u8\""));

        let disabled = supervisor.disable("source").await.unwrap();
        assert!(disabled.disabled);
        assert!(disabled.stopped);
        assert!(!disabled.online);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn position_updates_are_whole_seconds() {
        assert_eq!(
            normalize_observed_value("position", json!(12.987)),
            json!(12.0)
        );
        assert_eq!(
            normalize_observed_value("duration", json!(12.987)),
            json!(12.987)
        );
    }

    #[test]
    fn loop_updates_are_normalized_for_state_and_events() {
        assert_eq!(
            normalize_observed_value("loop_file", json!("inf")),
            json!("on")
        );
        assert_eq!(
            normalize_observed_value("loop_playlist", json!("no")),
            json!("off")
        );
        assert_eq!(loop_state(&json!("off")).as_deref(), Some("off"));
    }

    #[test]
    fn formatted_show_text_enables_property_expansion() {
        assert_eq!(
            mpv_command_values(
                "show-text",
                &[json!("${osd-ass-cc/0}{\\fs100}movies"), json!(4000)]
            ),
            vec![
                json!("expand-properties"),
                json!("show-text"),
                json!("${osd-ass-cc/0}{\\fs100}movies"),
                json!(4000)
            ]
        );
        assert_eq!(
            mpv_command_values("show-text", &[json!("movies"), json!(4000)]),
            vec![json!("show-text"), json!("movies"), json!(4000)]
        );
    }

    #[test]
    fn channel_catalog_accepts_only_m3u_files() {
        assert!(is_channel_path(Path::new("movies.m3u")));
        assert!(is_channel_path(Path::new("cameras.M3U8")));
        assert!(!is_channel_path(Path::new("movie.mkv")));
        assert!(!is_channel_path(Path::new("playlist")));
    }
}
