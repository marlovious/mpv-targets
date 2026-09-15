//! Non-interactive deployment input.  This is deliberately separate from the
//! daemon's local runtime configuration.

use crate::{config::NodeConfig, protocol::validate_target_name};
use serde::Deserialize;
use std::collections::HashSet;
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerFile {
    #[serde(default = "generic_host_profile")]
    pub host_profile: String,
    pub node: NodeConfig,
    pub tls: AnswerTls,
    pub service: ServiceAnswer,
    #[serde(default)]
    pub targets: Vec<AnswerTarget>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerTls {
    #[serde(default)]
    pub generate_self_signed: bool,
    pub certificate: Option<String>,
    pub private_key: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServiceAnswer {
    pub install: bool,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct AnswerTarget {
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default = "default_start_paused")]
    pub start_paused: bool,
    #[serde(default = "default_start_muted")]
    pub start_muted: bool,
    #[serde(default = "default_volume")]
    pub volume: f64,
}

impl AnswerFile {
    pub fn parse(source: &str) -> Result<Self, AnswerError> {
        let answer: Self = toml::from_str(source).map_err(AnswerError::Toml)?;
        answer.validate()?;
        Ok(answer)
    }

    pub fn validate(&self) -> Result<(), AnswerError> {
        if self.host_profile.is_empty() {
            return Err(AnswerError::EmptyHostProfile);
        }
        if self.node.id.is_empty() {
            return Err(AnswerError::EmptyNodeId);
        }
        self.node
            .listen
            .parse::<std::net::SocketAddr>()
            .map_err(|_| AnswerError::InvalidListenAddress(self.node.listen.clone()))?;
        let supplied_pair = self.certificate_pair()?;
        if self.tls.generate_self_signed == supplied_pair.is_some() {
            return Err(AnswerError::InvalidTlsChoice);
        }

        let mut names = HashSet::new();
        for target in &self.targets {
            validate_target_name(&target.name).map_err(|error| AnswerError::InvalidTargetName {
                name: target.name.clone(),
                error,
            })?;
            if !names.insert(&target.name) {
                return Err(AnswerError::DuplicateTarget(target.name.clone()));
            }
            if !target.volume.is_finite() || target.volume < 0.0 {
                return Err(AnswerError::InvalidVolume {
                    name: target.name.clone(),
                    volume: target.volume,
                });
            }
        }
        Ok(())
    }

    /// Returns the supplied certificate/key source pair, after ensuring it is
    /// complete. `None` means the answer chose self-signed generation.
    pub fn certificate_pair(&self) -> Result<Option<(&str, &str)>, AnswerError> {
        match (&self.tls.certificate, &self.tls.private_key) {
            (None, None) => Ok(None),
            (Some(certificate), Some(private_key))
                if !certificate.is_empty() && !private_key.is_empty() =>
            {
                Ok(Some((certificate, private_key)))
            }
            _ => Err(AnswerError::IncompleteTlsPair),
        }
    }
}

fn generic_host_profile() -> String {
    "generic".into()
}

fn default_start_paused() -> bool {
    true
}

fn default_start_muted() -> bool {
    true
}

fn default_volume() -> f64 {
    100.0
}

#[derive(Debug, Error)]
pub enum AnswerError {
    #[error("invalid answer-file TOML: {0}")]
    Toml(toml::de::Error),
    #[error("host_profile must not be empty")]
    EmptyHostProfile,
    #[error("node id must not be empty")]
    EmptyNodeId,
    #[error("invalid listener address: {0}")]
    InvalidListenAddress(String),
    #[error("choose either generate_self_signed = true or a complete certificate/private_key pair")]
    InvalidTlsChoice,
    #[error("certificate and private_key must be supplied together")]
    IncompleteTlsPair,
    #[error("invalid target name `{name}`: {error}")]
    InvalidTargetName {
        name: String,
        error: crate::protocol::TargetNameError,
    },
    #[error("duplicate target name: {0}")]
    DuplicateTarget(String),
    #[error("target `{name}` has invalid startup volume: {volume}")]
    InvalidVolume { name: String, volume: f64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[node]
id = "living-room"
listen = "127.0.0.1:9876"

[tls]
generate_self_signed = true

[service]
install = true

[[targets]]
name = "cameras"
start_paused = true
start_muted = true
volume = 100

[[targets]]
name = "movies"
start_paused = true
start_muted = true
volume = 100

[[targets]]
name = "music"
# Deployment creates one shared channels/ directory for user-managed M3U files.
# Add .m3u or .m3u8 files there, then use `targets show-channels` and
# `targets set-channel TARGET NAME` after deployment.
start_paused = true
start_muted = true
volume = 100
"#;

    #[test]
    fn small_documented_answer_file_parses_with_generic_default() {
        let answer = AnswerFile::parse(EXAMPLE).unwrap();
        assert_eq!(answer.host_profile, "generic");
        assert!(answer.service.install);
        assert!(answer.tls.generate_self_signed);
    }

    #[test]
    fn tls_choice_must_be_unambiguous() {
        let missing = EXAMPLE.replace("generate_self_signed = true", "");
        assert!(matches!(
            AnswerFile::parse(&missing),
            Err(AnswerError::InvalidTlsChoice)
        ));
        let both = EXAMPLE.replace(
            "generate_self_signed = true",
            "generate_self_signed = true\ncertificate = \"cert.pem\"\nprivate_key = \"key.pem\"",
        );
        assert!(matches!(
            AnswerFile::parse(&both),
            Err(AnswerError::InvalidTlsChoice)
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let typo = EXAMPLE.replace("start_paused = true", "start_pause = true");
        assert!(matches!(
            AnswerFile::parse(&typo),
            Err(AnswerError::Toml(_))
        ));
    }

    #[test]
    fn distributed_answer_file_parses() {
        AnswerFile::parse(include_str!("../../../examples/setup.toml")).unwrap();
    }
}
