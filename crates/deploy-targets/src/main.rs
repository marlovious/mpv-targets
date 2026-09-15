use mpv_targets::{AnswerFile, DaemonConfig, TargetConfig, TlsConfig, TlsMaterialError};
use rcgen::generate_simple_self_signed;
use std::{
    env, fs,
    io::{self, BufRead, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode},
};
use thiserror::Error;

const SERVICE_UNIT: &str = "mpv-targets.service";

fn main() -> ExitCode {
    match run(env::args().skip(1).collect()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("deploy-targets: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(arguments: Vec<String>) -> Result<(), DeployError> {
    let (answer_path, overwrite) = match arguments.as_slice() {
        [flag] if flag == "--help" || flag == "-h" => {
            println!("usage: deploy-targets [--overwrite] <answer-file>");
            return Ok(());
        }
        [path] => (PathBuf::from(path), false),
        [flag, path] if flag == "--overwrite" => (PathBuf::from(path), true),
        _ => return Err(DeployError::Usage),
    };
    let answer_path = answer_path
        .canonicalize()
        .map_err(|source| DeployError::Input {
            path: answer_path,
            source,
        })?;
    let answer_source = fs::read_to_string(&answer_path).map_err(|source| DeployError::Input {
        path: answer_path.clone(),
        source,
    })?;
    let answer = AnswerFile::parse(&answer_source).map_err(DeployError::Answer)?;
    let root = config_root()?;
    let root_exists = root.exists();
    if root_exists && !overwrite {
        return Err(DeployError::ExistingConfig(root));
    }
    let daemon_binary = load_daemon_binary()?;
    let plan = Plan::build(&answer, &answer_path, root.clone(), daemon_binary)?;
    if root_exists {
        confirm_overwrite(&root)?;
        remove_existing_root(&root)?;
    }
    plan.apply()
}

struct Plan {
    install_service: bool,
    config: DaemonConfig,
    config_root: PathBuf,
    target_configs: Vec<(String, Vec<u8>)>,
    certificate: Vec<u8>,
    private_key: Vec<u8>,
    daemon_binary: Vec<u8>,
}

impl Plan {
    fn build(
        answer: &AnswerFile,
        answer_path: &Path,
        config_root: PathBuf,
        daemon_binary: Vec<u8>,
    ) -> Result<Self, DeployError> {
        if answer.host_profile != "generic" {
            return Err(DeployError::UnsupportedHostProfile(
                answer.host_profile.clone(),
            ));
        }
        let answer_dir = answer_path.parent().unwrap_or(Path::new(".")).to_owned();
        let (certificate, private_key) =
            match answer.certificate_pair().map_err(DeployError::Answer)? {
                Some((certificate, private_key)) => (
                    fs::read(resolve_source(&answer_dir, certificate)).map_err(|source| {
                        DeployError::Input {
                            path: resolve_source(&answer_dir, certificate),
                            source,
                        }
                    })?,
                    fs::read(resolve_source(&answer_dir, private_key)).map_err(|source| {
                        DeployError::Input {
                            path: resolve_source(&answer_dir, private_key),
                            source,
                        }
                    })?,
                ),
                None => generate_certificate(&answer.node.id, &answer.node.listen)?,
            };
        mpv_targets::server_tls_config(&certificate, &private_key)
            .map_err(DeployError::InvalidTlsMaterial)?;
        let mut target_configs = Vec::with_capacity(answer.targets.len());
        let mut targets = Vec::with_capacity(answer.targets.len());
        for target in &answer.targets {
            target_configs.push((target.name.clone(), generated_mpv_conf(target).into_bytes()));
            targets.push(TargetConfig {
                name: target.name.clone(),
                disabled: target.disabled,
                channel: None,
            });
        }
        let config = DaemonConfig {
            node: answer.node.clone(),
            tls: TlsConfig {
                certificate: "tls/server.crt".into(),
                private_key: "tls/server.key".into(),
            },
            targets,
        };
        config
            .validate()
            .map_err(|error| DeployError::Config(error.to_string()))?;
        Ok(Self {
            install_service: answer.service.install,
            config,
            config_root,
            target_configs,
            certificate,
            private_key,
            daemon_binary,
        })
    }

    fn apply(&self) -> Result<(), DeployError> {
        self.apply_at(&user_unit_path()?, &user_daemon_path()?)
    }

    fn apply_at(&self, unit_path: &Path, daemon_path: &Path) -> Result<(), DeployError> {
        if let Some(parent) = unit_path.parent() {
            fs::create_dir_all(parent).map_err(|source| DeployError::Output {
                path: parent.to_owned(),
                source,
            })?;
        }
        fs::create_dir_all(self.config_root.join("targets")).map_err(|source| {
            DeployError::Output {
                path: self.config_root.clone(),
                source,
            }
        })?;
        fs::create_dir_all(self.config_root.join("tls")).map_err(|source| DeployError::Output {
            path: self.config_root.join("tls"),
            source,
        })?;
        fs::create_dir_all(self.config_root.join("logs")).map_err(|source| {
            DeployError::Output {
                path: self.config_root.join("logs"),
                source,
            }
        })?;
        fs::create_dir_all(self.config_root.join("channels")).map_err(|source| {
            DeployError::Output {
                path: self.config_root.join("channels"),
                source,
            }
        })?;
        for (name, contents) in &self.target_configs {
            let directory = self.config_root.join("targets").join(name);
            fs::create_dir_all(directory.join("scripts")).map_err(|source| {
                DeployError::Output {
                    path: directory.clone(),
                    source,
                }
            })?;
            atomic_write(&directory.join("mpv.conf"), contents, None)?;
        }
        let certificate_path = self.config_root.join("tls/server.crt");
        let private_key_path = self.config_root.join("tls/server.key");
        atomic_write(&certificate_path, &self.certificate, None)?;
        atomic_write(&private_key_path, &self.private_key, Some(0o600))?;
        let config_toml = toml::to_string_pretty(&self.config)
            .map_err(|source| DeployError::Serialize(source.to_string()))?;
        atomic_write(
            &self.config_root.join("mpv-targets.toml"),
            config_toml.as_bytes(),
            None,
        )?;
        if let Some(parent) = daemon_path.parent() {
            fs::create_dir_all(parent).map_err(|source| DeployError::Output {
                path: parent.to_owned(),
                source,
            })?;
        }
        atomic_write(daemon_path, &self.daemon_binary, Some(0o755))?;
        let unit_was_installed = unit_path.exists();
        let unit = format!(
            "[Unit]\nDescription=mpv-targets daemon\nAfter=default.target\n\n[Service]\nExecStart=%h/.local/bin/mpv-targetsd --config {}\nRestart=on-failure\nKillMode=process\n\n[Install]\nWantedBy=default.target\n",
            self.config_root.join("mpv-targets.toml").display()
        );
        atomic_write(unit_path, unit.as_bytes(), None)?;
        print_work_receipt("Created", &self.config_root.display().to_string());
        print_work_receipt(
            "Created",
            &self.config_root.join("logs").display().to_string(),
        );
        print_work_receipt(
            "Created",
            &self.config_root.join("tls").display().to_string(),
        );
        print_work_receipt(
            "Created",
            &self.config_root.join("channels").display().to_string(),
        );
        for (name, _) in &self.target_configs {
            let directory = self.config_root.join("targets").join(name);
            print_work_receipt("Created", &directory.display().to_string());
            print_work_receipt("Created", &directory.join("mpv.conf").display().to_string());
            print_work_receipt("Created", &directory.join("scripts").display().to_string());
        }
        print_work_receipt(
            "Created",
            &self
                .config_root
                .join("mpv-targets.toml")
                .display()
                .to_string(),
        );
        print_work_receipt("Installed", &daemon_path.display().to_string());
        if unit_was_installed {
            print_work_receipt("Replaced", &unit_path.display().to_string());
        } else {
            print_work_receipt("Installed", &unit_path.display().to_string());
        }
        if self.install_service {
            install_service()?;
            print_work_receipt("Started", SERVICE_UNIT);
            show_status()?;
        } else {
            print_work_receipt("Saved", &self.config_root.display().to_string());
            println!(
                "to start manually: systemctl --user daemon-reload && systemctl --user enable --now {SERVICE_UNIT}"
            );
        }
        Ok(())
    }
}

fn print_work_receipt(action: &str, object: &str) {
    println!(
        "[{action}]{} :: {object}",
        " ".repeat(9usize.saturating_sub(action.len()))
    );
}

fn generate_certificate(node_id: &str, listen: &str) -> Result<(Vec<u8>, Vec<u8>), DeployError> {
    let mut names = vec![node_id.to_owned(), "localhost".into()];
    if let Ok(address) = listen.parse::<std::net::SocketAddr>()
        && !address.ip().is_unspecified()
    {
        names.push(address.ip().to_string());
    }
    let generated = generate_simple_self_signed(names)
        .map_err(|error| DeployError::Certificate(error.to_string()))?;
    Ok((
        generated.cert.pem().into_bytes(),
        generated.key_pair.serialize_pem().into_bytes(),
    ))
}

fn resolve_source(base: &Path, source: &str) -> PathBuf {
    let path = Path::new(source);
    if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    }
}

fn generated_mpv_conf(target: &mpv_targets::AnswerTarget) -> String {
    mpv_targets::base_mpv_conf(target.start_paused, target.start_muted, target.volume)
}

fn config_root() -> Result<PathBuf, DeployError> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("mpv-targets"));
    }
    let home = env::var_os("HOME").ok_or(DeployError::MissingHome)?;
    Ok(PathBuf::from(home).join(".config/mpv-targets"))
}

fn user_unit_path() -> Result<PathBuf, DeployError> {
    let root = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or(DeployError::MissingHome)?;
    Ok(root.join("systemd/user").join(SERVICE_UNIT))
}

fn user_daemon_path() -> Result<PathBuf, DeployError> {
    let home = env::var_os("HOME").ok_or(DeployError::MissingHome)?;
    Ok(PathBuf::from(home).join(".local/bin/mpv-targetsd"))
}

fn load_daemon_binary() -> Result<Vec<u8>, DeployError> {
    let deployer = env::current_exe().map_err(DeployError::CurrentExecutable)?;
    let daemon = deployer
        .parent()
        .ok_or_else(|| DeployError::MissingDaemonBinary(deployer.clone()))?
        .join("mpv-targetsd");
    if !daemon.is_file() {
        return Err(DeployError::MissingDaemonBinary(daemon));
    }
    fs::read(&daemon).map_err(|source| DeployError::Input {
        path: daemon,
        source,
    })
}

fn remove_existing_root(root: &Path) -> Result<(), DeployError> {
    match fs::symlink_metadata(root) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(DeployError::SymlinkRoot(root.to_owned()))
        }
        Ok(metadata) if metadata.is_dir() => {
            fs::remove_dir_all(root).map_err(|source| DeployError::Output {
                path: root.to_owned(),
                source,
            })
        }
        Ok(_) => Err(DeployError::ExistingConfig(root.to_owned())),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(DeployError::Output {
            path: root.to_owned(),
            source,
        }),
    }
}

fn confirm_overwrite(root: &Path) -> Result<(), DeployError> {
    if !io::stdin().is_terminal() {
        return Err(DeployError::OverwriteRequiresTerminal);
    }
    eprintln!(
        "This will overwrite the existing mpv-targets configuration at:\n{}\n\nContinue? [y/N]",
        root.display()
    );
    io::stderr().flush().map_err(DeployError::PromptIo)?;
    let mut response = String::new();
    io::stdin()
        .lock()
        .read_line(&mut response)
        .map_err(DeployError::PromptIo)?;
    if matches!(response.trim(), "y" | "Y") {
        Ok(())
    } else {
        Err(DeployError::OverwriteCancelled)
    }
}

fn atomic_write(path: &Path, contents: &[u8], mode: Option<u32>) -> Result<(), DeployError> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(|source| DeployError::Output {
            path: temporary.clone(),
            source,
        })?;
    file.write_all(contents)
        .and_then(|_| file.sync_all())
        .map_err(|source| DeployError::Output {
            path: temporary.clone(),
            source,
        })?;
    if let Some(mode) = mode {
        set_mode(&temporary, mode)?;
    }
    fs::rename(&temporary, path).map_err(|source| {
        let _ = fs::remove_file(&temporary);
        DeployError::Output {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) -> Result<(), DeployError> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).map_err(|source| {
        DeployError::Output {
            path: path.to_owned(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: u32) -> Result<(), DeployError> {
    Ok(())
}

fn install_service() -> Result<(), DeployError> {
    let status = Command::new("systemctl")
        .args(["--user", "daemon-reload"])
        .status()
        .map_err(DeployError::Systemctl)?;
    if !status.success() {
        return Err(DeployError::Systemctl(io::Error::other(
            "systemctl --user daemon-reload failed",
        )));
    }
    let status = Command::new("systemctl")
        .args(["--user", "enable", "--now", SERVICE_UNIT])
        .status()
        .map_err(DeployError::Systemctl)?;
    if !status.success() {
        return Err(DeployError::Systemctl(io::Error::other(
            "systemctl --user enable --now failed",
        )));
    }
    Ok(())
}

fn show_status() -> Result<(), DeployError> {
    let status = Command::new("systemctl")
        .args(["--user", "status", "--no-pager", SERVICE_UNIT])
        .status()
        .map_err(DeployError::Systemctl)?;
    if status.success() {
        Ok(())
    } else {
        Err(DeployError::Systemctl(io::Error::other(
            "systemctl --user status failed",
        )))
    }
}

#[derive(Debug, Error)]
enum DeployError {
    #[error("usage: deploy-targets [--overwrite] <answer-file>")]
    Usage,
    #[error(
        "existing mpv-targets configuration found at {0}; to redeploy the answer file use --overwrite"
    )]
    ExistingConfig(PathBuf),
    #[error("refusing to overwrite symlink deployment root: {0}")]
    SymlinkRoot(PathBuf),
    #[error("--overwrite requires an interactive terminal; no files were changed")]
    OverwriteRequiresTerminal,
    #[error("overwrite cancelled; no files were changed")]
    OverwriteCancelled,
    #[error("cannot read overwrite confirmation: {0}")]
    PromptIo(#[source] io::Error),
    #[error("cannot determine the deploying user's home directory")]
    MissingHome,
    #[error("cannot locate the running deploy-targets executable: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("mpv-targetsd must be beside deploy-targets: {0}")]
    MissingDaemonBinary(PathBuf),
    #[error("cannot read {path}: {source}")]
    Input { path: PathBuf, source: io::Error },
    #[error("answer file rejected: {0}")]
    Answer(#[source] mpv_targets::AnswerError),
    #[error("configuration rejected: {0}")]
    Config(String),
    #[error("host profile is not available: {0}")]
    UnsupportedHostProfile(String),
    #[error("cannot generate TLS certificate: {0}")]
    Certificate(String),
    #[error("cannot write {path}: {source}")]
    Output { path: PathBuf, source: io::Error },
    #[error("invalid TLS certificate/private-key pair: {0}")]
    InvalidTlsMaterial(#[source] TlsMaterialError),
    #[error("cannot serialize daemon configuration: {0}")]
    Serialize(String),
    #[error("systemd command failed: {0}")]
    Systemctl(#[source] io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_and_materialization_generate_target_config() {
        let root = env::temp_dir().join(format!("mpv-targets-deploy-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("answer")).unwrap();
        let answer_path = root.join("answer/setup.toml");
        fs::write(
            &answer_path,
            r#"[node]
id = "living-room"
listen = "127.0.0.1:9876"

[tls]
generate_self_signed = true

[service]
install = false

[[targets]]
name = "music"
start_paused = false
start_muted = true
volume = 80
"#,
        )
        .unwrap();
        let answer = AnswerFile::parse(&fs::read_to_string(&answer_path).unwrap()).unwrap();
        let config_root = root.join("installed/mpv-targets");
        let plan = Plan::build(
            &answer,
            &answer_path,
            config_root.clone(),
            b"test daemon".to_vec(),
        )
        .unwrap();
        let unit_path = root.join("installed/systemd/user/mpv-targets.service");
        let daemon_path = root.join("installed/bin/mpv-targetsd");
        plan.apply_at(&unit_path, &daemon_path).unwrap();
        let daemon_config =
            DaemonConfig::parse(&fs::read_to_string(config_root.join("mpv-targets.toml")).unwrap())
                .unwrap();
        assert_eq!(daemon_config.node.id, "living-room");
        assert_eq!(daemon_config.targets.len(), 1);
        assert!(!daemon_config.targets[0].disabled);
        assert_eq!(daemon_config.targets[0].channel, None);
        assert!(config_root.join("targets/music/scripts").is_dir());
        assert!(config_root.join("channels").is_dir());
        assert!(config_root.join("logs").is_dir());
        let mpv_conf = fs::read_to_string(config_root.join("targets/music/mpv.conf")).unwrap();
        assert!(mpv_conf.contains("pause=no"));
        assert!(mpv_conf.contains("volume=80"));
        assert!(config_root.join("tls/server.crt").is_file());
        let unit = fs::read_to_string(&unit_path).unwrap();
        assert!(unit.contains("Restart=on-failure\nKillMode=process"));
        assert!(unit.contains(&format!(
            "ExecStart=%h/.local/bin/mpv-targetsd --config {}",
            config_root.join("mpv-targets.toml").display()
        )));
        assert_eq!(fs::read(&daemon_path).unwrap(), b"test daemon");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(config_root.join("tls/server.key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        fs::remove_dir_all(root).unwrap();
    }
}
