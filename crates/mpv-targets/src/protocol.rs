use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use thiserror::Error;

pub const PROTOCOL_VERSION: u32 = 1;

/// A client request. `id` is opaque to the service and is echoed in exactly one
/// terminal response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Request {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    pub operation: Operation,
}

impl Request {
    pub fn validate(&self) -> Result<(), RequestValidationError> {
        if self.id.is_empty() {
            return Err(RequestValidationError::EmptyId);
        }
        match (&self.target, &self.operation) {
            (None, Operation::NodeListChannels) => {}
            (Some(_), Operation::NodeListChannels) => {
                return Err(RequestValidationError::UnexpectedTarget);
            }
            (Some(target), _) => {
                validate_target_name(target).map_err(RequestValidationError::InvalidTarget)?;
            }
            (None, _) => return Err(RequestValidationError::MissingTarget),
        }
        self.operation.validate()
    }
}

/// The explicit service vocabulary and the restricted native mpv escape hatch.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Operation {
    TargetAdd {
        from: Option<String>,
        enable: bool,
    },
    TargetRemove,
    TargetStart,
    TargetStop,
    TargetRestart,
    TargetEnable,
    TargetDisable,
    TargetRename {
        name: String,
    },
    TargetSetChannel {
        channel: Option<String>,
        restart: bool,
    },
    NodeListChannels,
    Mpv {
        command: String,
        #[serde(default)]
        args: Vec<Value>,
    },
}

impl Operation {
    pub fn validate(&self) -> Result<(), RequestValidationError> {
        match self {
            Self::TargetAdd { from, .. } => {
                if let Some(source) = from {
                    validate_target_name(source).map_err(RequestValidationError::InvalidTarget)?;
                }
                Ok(())
            }
            Self::TargetRename { name } => {
                validate_target_name(name).map_err(RequestValidationError::InvalidReplacement)
            }
            Self::TargetSetChannel {
                channel: Some(channel),
                ..
            } if !valid_channel(channel) => {
                Err(RequestValidationError::InvalidChannel(channel.clone()))
            }
            Self::Mpv { command, .. } if !is_allowed_mpv_command(command) => Err(
                RequestValidationError::NativeCommandNotAllowed(command.clone()),
            ),
            _ => Ok(()),
        }
    }
}

pub(crate) fn valid_channel(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    if value.contains("://") {
        return true;
    }
    let path = Path::new(value);
    path.is_absolute()
        && path
            .extension()
            .and_then(|extension| extension.to_str())
            .is_some_and(|extension| {
                matches!(extension.to_ascii_lowercase().as_str(), "m3u" | "m3u8")
            })
}

/// Kept as a public alias so callers can name the service-operation family.
pub type ServiceOperation = Operation;
/// Kept as a public alias for native mpv operations.
pub type MpvOperation = Operation;

/// A terminal response. It always has the request's id.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Response {
    Success {
        id: String,
        #[serde(default)]
        data: Value,
    },
    Error {
        id: String,
        error: ErrorResponse,
    },
}

impl Response {
    pub fn success(id: impl Into<String>, data: Value) -> Self {
        Self::Success {
            id: id.into(),
            data,
        }
    }

    pub fn error(id: impl Into<String>, code: ErrorCode, message: impl Into<String>) -> Self {
        Self::Error {
            id: id.into(),
            error: ErrorResponse {
                code,
                message: message.into(),
            },
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Success { id, .. } | Self::Error { id, .. } => id,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ErrorResponse {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnknownTarget,
    TargetAlreadyStopped,
    TargetNotRunning,
    TargetDisabled,
    ConfigRejected,
    TargetOffline,
    MpvRejected,
    Timeout,
    IpcLost,
    ShuttingDown,
}

/// Server-to-client messages. A snapshot is always the first message on a
/// successfully established connection.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    Snapshot {
        #[serde(flatten)]
        snapshot: Snapshot,
    },
    TargetChanged(TargetChanged),
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Snapshot {
    pub protocol_version: u32,
    pub node_id: String,
    pub health: Health,
    pub targets: Vec<TargetRecord>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Health {
    pub state: HealthState,
    pub targets_configured: u32,
    pub targets_expected_online: u32,
    pub targets_online: u32,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthState {
    Ready,
    Idle,
    Degraded,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetRecord {
    pub name: String,
    pub disabled: bool,
    pub stopped: bool,
    pub online: bool,
    pub channel: Option<String>,
    pub state: TargetState,
    pub revision: u64,
}

/// Observed state only. The daemon never treats these fields as desired state.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
pub struct TargetState {
    pub paused: Option<bool>,
    pub muted: Option<bool>,
    pub volume: Option<f64>,
    pub title: Option<String>,
    pub playlist_position: Option<u32>,
    pub playlist_count: Option<u32>,
    pub idle_active: Option<bool>,
    pub duration: Option<f64>,
    pub position: Option<f64>,
    pub loop_file: Option<String>,
    pub loop_playlist: Option<String>,
    pub audio_track: Option<String>,
    pub subtitle_track: Option<String>,
    pub subtitles_visible: Option<bool>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct TargetChanged {
    pub target: String,
    pub revision: u64,
    pub changed: Value,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum RequestValidationError {
    #[error("request id must not be empty")]
    EmptyId,
    #[error("invalid target: {0}")]
    InvalidTarget(TargetNameError),
    #[error("target is required for this operation")]
    MissingTarget,
    #[error("node_list_channels must not specify a target")]
    UnexpectedTarget,
    #[error("channel must be an absolute .m3u/.m3u8 path or URL: {0}")]
    InvalidChannel(String),
    #[error("invalid replacement target name: {0}")]
    InvalidReplacement(TargetNameError),
    #[error("native mpv command is not allowed: {0}")]
    NativeCommandNotAllowed(String),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TargetNameError {
    #[error("target name must not be empty")]
    Empty,
    #[error("`all` is reserved for the operator bulk selector")]
    Reserved,
    #[error("target name must be 1–64 characters")]
    InvalidLength,
    #[error("target name must begin with a lowercase ASCII letter or digit")]
    InvalidStart,
    #[error("target name may use only lowercase ASCII letters, digits, `-`, and `_`")]
    InvalidCharacters,
}

pub fn validate_target_name(name: &str) -> Result<(), TargetNameError> {
    if name.is_empty() {
        return Err(TargetNameError::Empty);
    }
    if name == "all" {
        return Err(TargetNameError::Reserved);
    }
    if name.len() > 64 {
        return Err(TargetNameError::InvalidLength);
    }
    let first = name.as_bytes()[0];
    if !first.is_ascii_lowercase() && !first.is_ascii_digit() {
        return Err(TargetNameError::InvalidStart);
    }
    if name.bytes().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
    }) {
        Ok(())
    } else {
        Err(TargetNameError::InvalidCharacters)
    }
}

pub fn is_allowed_mpv_command(command: &str) -> bool {
    matches!(
        command,
        "get_property"
            | "set_property"
            | "cycle"
            | "add"
            | "stop"
            | "seek"
            | "revert-seek"
            | "playlist-next"
            | "playlist-prev"
            | "playlist-play-index"
            | "playlist-remove"
            | "playlist-move"
            | "playlist-shuffle"
            | "playlist-unshuffle"
            | "playlist-clear"
            | "loadfile"
            | "loadlist"
            | "keypress"
            | "keydown"
            | "keyup"
            | "show-text"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips_in_the_documented_shape() {
        let request = Request {
            id: "client-43".into(),
            target: Some("music".into()),
            operation: Operation::Mpv {
                command: "loadfile".into(),
                args: vec![json!("https://example.test/channel.m3u"), json!("replace")],
            },
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "id": "client-43",
                "target": "music",
                "operation": {
                    "kind": "mpv",
                    "command": "loadfile",
                    "args": ["https://example.test/channel.m3u", "replace"]
                }
            })
        );
        assert_eq!(
            serde_json::from_value::<Request>(serde_json::to_value(request.clone()).unwrap())
                .unwrap(),
            request
        );
    }

    #[test]
    fn channel_catalog_request_is_the_only_targetless_operation() {
        let request = Request {
            id: "client-44".into(),
            target: None,
            operation: Operation::NodeListChannels,
        };
        assert!(request.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({"id": "client-44", "operation": {"kind": "node_list_channels"}})
        );

        let mut targeted = request.clone();
        targeted.target = Some("music".into());
        assert!(matches!(
            targeted.validate(),
            Err(RequestValidationError::UnexpectedTarget)
        ));

        let targetless_start = Request {
            id: "client-45".into(),
            target: None,
            operation: Operation::TargetStart,
        };
        assert!(matches!(
            targetless_start.validate(),
            Err(RequestValidationError::MissingTarget)
        ));
    }

    #[test]
    fn channel_mutation_rejects_ambiguous_local_paths() {
        for channel in ["relative.m3u", "/srv/media/movie.mkv", ""] {
            let request = Request {
                id: "client-46".into(),
                target: Some("music".into()),
                operation: Operation::TargetSetChannel {
                    channel: Some(channel.into()),
                    restart: false,
                },
            };
            assert!(matches!(
                request.validate(),
                Err(RequestValidationError::InvalidChannel(_))
            ));
        }
    }

    #[test]
    fn every_service_operation_has_a_stable_wire_name() {
        let cases = [
            (Operation::TargetStart, json!({"kind": "target_start"})),
            (Operation::TargetStop, json!({"kind": "target_stop"})),
            (Operation::TargetRestart, json!({"kind": "target_restart"})),
            (Operation::TargetEnable, json!({"kind": "target_enable"})),
            (Operation::TargetDisable, json!({"kind": "target_disable"})),
            (
                Operation::TargetRename {
                    name: "movies".into(),
                },
                json!({"kind": "target_rename", "name": "movies"}),
            ),
            (
                Operation::TargetSetChannel {
                    channel: None,
                    restart: false,
                },
                json!({"kind": "target_set_channel", "channel": null, "restart": false}),
            ),
            (
                Operation::NodeListChannels,
                json!({"kind": "node_list_channels"}),
            ),
        ];
        for (operation, expected) in cases {
            assert_eq!(serde_json::to_value(operation).unwrap(), expected);
        }
    }

    #[test]
    fn responses_are_correlated_terminal_messages() {
        assert_eq!(
            serde_json::to_value(Response::success("client-42", json!({"online": true}))).unwrap(),
            json!({"result": "success", "id": "client-42", "data": {"online": true}})
        );
        assert_eq!(
            serde_json::to_value(Response::error(
                "client-42",
                ErrorCode::TargetDisabled,
                "target is disabled; enable it before starting",
            ))
            .unwrap(),
            json!({
                "result": "error",
                "id": "client-42",
                "error": {
                    "code": "target_disabled",
                    "message": "target is disabled; enable it before starting"
                }
            })
        );
    }

    #[test]
    fn error_codes_are_the_complete_documented_vocabulary() {
        let codes = [
            (ErrorCode::InvalidRequest, "invalid_request"),
            (ErrorCode::UnknownTarget, "unknown_target"),
            (ErrorCode::TargetAlreadyStopped, "target_already_stopped"),
            (ErrorCode::TargetNotRunning, "target_not_running"),
            (ErrorCode::TargetDisabled, "target_disabled"),
            (ErrorCode::ConfigRejected, "config_rejected"),
            (ErrorCode::TargetOffline, "target_offline"),
            (ErrorCode::MpvRejected, "mpv_rejected"),
            (ErrorCode::Timeout, "timeout"),
            (ErrorCode::IpcLost, "ipc_lost"),
            (ErrorCode::ShuttingDown, "shutting_down"),
        ];
        for (code, expected) in codes {
            assert_eq!(serde_json::to_value(code).unwrap(), json!(expected));
        }
    }

    #[test]
    fn events_have_an_explicit_event_discriminator() {
        let event = Event::TargetChanged(TargetChanged {
            target: "music".into(),
            revision: 3,
            changed: json!({"muted": true}),
        });
        assert_eq!(
            serde_json::to_value(event).unwrap(),
            json!({
                "event": "target_changed", "target": "music", "revision": 3,
                "changed": {"muted": true}
            })
        );
    }

    #[test]
    fn snapshot_event_is_complete_and_versioned() {
        let event = Event::Snapshot {
            snapshot: Snapshot {
                protocol_version: PROTOCOL_VERSION,
                node_id: "fez".into(),
                health: Health {
                    state: HealthState::Idle,
                    targets_configured: 0,
                    targets_expected_online: 0,
                    targets_online: 0,
                },
                targets: vec![],
            },
        };
        assert_eq!(serde_json::to_value(event).unwrap()["event"], "snapshot");
    }

    #[test]
    fn reserved_and_unsafe_target_names_are_rejected() {
        for name in [
            "",
            "all",
            "Living Room",
            "video/path",
            "music.",
            "-music",
            "_music",
        ] {
            assert!(
                validate_target_name(name).is_err(),
                "{name} must be rejected"
            );
        }
        for name in ["music", "living-room", "cam_1"] {
            assert!(
                validate_target_name(name).is_ok(),
                "{name} must be accepted"
            );
        }
        assert!(validate_target_name(&"a".repeat(65)).is_err());
    }

    #[test]
    fn native_command_allowlist_excludes_process_control() {
        assert!(is_allowed_mpv_command("loadfile"));
        assert!(is_allowed_mpv_command("show-text"));
        assert!(!is_allowed_mpv_command("quit"));
        assert!(!is_allowed_mpv_command("run"));
    }

    #[test]
    fn add_operation_uses_only_an_existing_target_source() {
        let request = Request {
            id: "add-1".into(),
            target: Some("footage".into()),
            operation: Operation::TargetAdd {
                from: Some("cameras".into()),
                enable: false,
            },
        };
        assert!(request.validate().is_ok());
        assert_eq!(
            serde_json::to_value(request.operation).unwrap(),
            json!({"kind": "target_add", "from": "cameras", "enable": false})
        );
    }
}
