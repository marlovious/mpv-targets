//! `mpv-targetsd` is intentionally a foreground process.  Linux systemd is an
//! installation concern; the runtime itself must remain usable by macOS hosts.

use mpv_targets::DaemonConfig;
use std::{env, fs, path::PathBuf, process::ExitCode};

mod server;
mod supervisor;

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("mpv-targetsd: {message}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<(), String> {
    let config_path = parse_config_path(env::args().skip(1))?;
    let source = fs::read_to_string(&config_path)
        .map_err(|error| format!("cannot read {}: {error}", config_path.display()))?;
    let config = DaemonConfig::parse(&source).map_err(|error| error.to_string())?;
    let config_root = config_path
        .parent()
        .ok_or("configuration path has no parent directory")?;
    let server = server::WebServer::bind(
        config_root,
        &config.node.listen,
        &config.tls.certificate,
        &config.tls.private_key,
    )
    .await
    .map_err(|error| error.to_string())?;
    let supervisor = supervisor::Supervisor::new(config.clone(), config_path.clone());
    supervisor
        .start_configured_targets()
        .await
        .map_err(|error| format!("cannot initialize targets: {error}"))?;
    eprintln!(
        "mpv-targetsd: node `{}` started with {} configured targets",
        config.node.id,
        config.targets.len()
    );
    tokio::select! {
        signal = tokio::signal::ctrl_c() => {
            signal.map_err(|error| format!("cannot wait for shutdown signal: {error}"))?;
            eprintln!("mpv-targetsd: shutting down");
            Ok(())
        }
        result = server.run(supervisor, &config.node.listen) => {
            Err(result.unwrap_err().to_string())
        }
    }
}

fn parse_config_path(mut arguments: impl Iterator<Item = String>) -> Result<PathBuf, String> {
    match (arguments.next().as_deref(), arguments.next()) {
        (Some("--config"), Some(path)) => Ok(path.into()),
        (Some("--config"), None) => Err("--config requires a path".into()),
        (Some("--help") | Some("-h"), None) => {
            println!("usage: mpv-targetsd --config <path>");
            std::process::exit(0);
        }
        _ => Err("usage: mpv-targetsd --config <path>".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_config_path;

    #[test]
    fn requires_one_explicit_config_path() {
        assert_eq!(
            parse_config_path(["--config".into(), "a.toml".into()].into_iter())
                .unwrap()
                .to_string_lossy(),
            "a.toml"
        );
        assert!(parse_config_path(["--config".into()].into_iter()).is_err());
    }
}
