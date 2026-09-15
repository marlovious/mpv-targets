use crate::protocol::{TargetNameError, valid_channel, validate_target_name};
use serde::{Deserialize, Serialize};
use std::{collections::HashSet, net::SocketAddr, path::Path};
use thiserror::Error;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    pub node: NodeConfig,
    pub tls: TlsConfig,
    #[serde(default)]
    pub targets: Vec<TargetConfig>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NodeConfig {
    pub id: String,
    pub listen: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    pub certificate: String,
    pub private_key: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetConfig {
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub channel: Option<String>,
}

impl DaemonConfig {
    pub fn parse(source: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(source).map_err(ConfigError::Toml)?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.node.id.is_empty() {
            return Err(ConfigError::EmptyNodeId);
        }
        self.node
            .listen
            .parse::<SocketAddr>()
            .map_err(|_| ConfigError::InvalidListenAddress(self.node.listen.clone()))?;
        validate_config_path(&self.tls.certificate, "TLS certificate")?;
        validate_config_path(&self.tls.private_key, "TLS private key")?;

        let mut names = HashSet::new();
        for target in &self.targets {
            validate_target_name(&target.name).map_err(|source| {
                ConfigError::InvalidTargetName {
                    name: target.name.clone(),
                    source,
                }
            })?;
            if !names.insert(&target.name) {
                return Err(ConfigError::DuplicateTarget(target.name.clone()));
            }
            if let Some(channel) = &target.channel {
                validate_channel(channel)?;
            }
        }
        Ok(())
    }
}

fn validate_config_path(value: &str, label: &'static str) -> Result<(), ConfigError> {
    let path = Path::new(value);
    if value.is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|component| component.as_os_str() == "..")
    {
        return Err(ConfigError::InvalidPath {
            label,
            path: value.to_owned(),
        });
    }
    Ok(())
}

fn validate_channel(value: &str) -> Result<(), ConfigError> {
    if valid_channel(value) {
        Ok(())
    } else {
        Err(ConfigError::InvalidChannel(value.to_owned()))
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("invalid TOML: {0}")]
    Toml(toml::de::Error),
    #[error("node id must not be empty")]
    EmptyNodeId,
    #[error("invalid listener address: {0}")]
    InvalidListenAddress(String),
    #[error("invalid {label} path: {path}")]
    InvalidPath { label: &'static str, path: String },
    #[error("invalid target name `{name}`: {source}")]
    InvalidTargetName {
        name: String,
        source: TargetNameError,
    },
    #[error("duplicate target name: {0}")]
    DuplicateTarget(String),
    #[error("channel must be an absolute .m3u/.m3u8 path or URL: {0}")]
    InvalidChannel(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = r#"
[node]
id = "fez"
listen = "127.0.0.1:9876"

[tls]
certificate = "tls/server.crt"
private_key = "tls/server.key"

[[targets]]
name = "music"
channel = "https://example.test/music.m3u"

[[targets]]
name = "movies"
disabled = true
channel = "/srv/media/movies.m3u"
"#;

    #[test]
    fn documented_configuration_parses() {
        let config = DaemonConfig::parse(EXAMPLE).unwrap();
        assert_eq!(config.node.id, "fez");
        assert!(
            config
                .targets
                .iter()
                .find(|target| target.name == "movies")
                .unwrap()
                .disabled
        );
    }

    #[test]
    fn rejects_duplicate_target_and_ambiguous_paths() {
        let duplicate = EXAMPLE.replacen("name = \"movies\"", "name = \"music\"", 1);
        assert!(matches!(
            DaemonConfig::parse(&duplicate),
            Err(ConfigError::DuplicateTarget(_))
        ));
        let relative = EXAMPLE.replace("/srv/media/movies.m3u", "media/movies.m3u");
        assert!(matches!(
            DaemonConfig::parse(&relative),
            Err(ConfigError::InvalidChannel(_))
        ));
        let non_m3u = EXAMPLE.replace("/srv/media/movies.m3u", "/srv/media/movie.mkv");
        assert!(matches!(
            DaemonConfig::parse(&non_m3u),
            Err(ConfigError::InvalidChannel(_))
        ));
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let typo = EXAMPLE.replace("disabled = true", "disable = true");
        assert!(matches!(
            DaemonConfig::parse(&typo),
            Err(ConfigError::Toml(_))
        ));
    }
}
