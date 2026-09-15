use mpv_targets::{
    ClientOptions, DaemonConfig, LoadMode, LoopState, Operation, RemoteClient, Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    env, fs,
    io::{self, Write},
    path::PathBuf,
};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("targets: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let mut args: Vec<String> = env::args().skip(1).collect();
    if args.is_empty() || args[0] == "help" || args[0] == "--help" {
        print_help();
        return Ok(());
    }
    let explicit_url = take_option(&mut args, "--url")?;
    if args.is_empty() {
        print_help();
        return Ok(());
    }
    let command = args.remove(0);
    let (node, args) = resolve_selector(&command, args)?;
    validate_command_args(&command, &args)?;
    if explicit_url.is_some() && node.is_some() {
        return Err("use either --url or an @node selector, not both".into());
    }
    let book_url = if let Some(node) = node.as_deref() {
        Some(load_node(node)?)
    } else {
        None
    };
    let url = explicit_url
        .or(book_url)
        .map(Ok)
        .unwrap_or_else(local_daemon_url)?;
    let display_url = url.clone();
    let client = RemoteClient::connect(ClientOptions { url })
        .await
        .map_err(|e| e.to_string())?;
    let result = match command.as_str() {
        "status" => status(&client, &display_url, &args).await,
        "add" => add_target(&client, &args).await,
        "remove" => remove_target(&client, &args).await,
        "rename" => rename_target(&client, &args).await,
        "identify" => identify(&client, &args).await,
        "mpv" => native_mpv(&client, &args).await,
        "start" | "stop" | "restart" | "enable" | "disable" => {
            lifecycle(&client, &command, &args).await
        }
        "playlist" => playlist(&client, &args).await,
        "show-channels" => show_channels(&client).await,
        "set-channel" | "clear-channel" => channel(&client, &command, &args).await,
        "play" | "pause" | "toggle-play" | "loadfile" | "next" | "previous" | "mute" | "unmute"
        | "toggle-mute" | "fullscreen" | "loop" | "repeat" | "shuffle" | "unshuffle"
        | "cycle-audio" | "cycle-subtitle" | "disable-subtitle" => {
            mpv_action(&client, &command, &args).await
        }
        _ => Err(format!("unknown command `{command}`; use `targets help`")),
    };
    client.close().await;
    if result.is_ok() && !(command == "status" && args.iter().any(|arg| arg == "--json")) {
        println!();
    }
    result
}

#[derive(Debug, Deserialize)]
struct NodeBook {
    nodes: Vec<NodeEntry>,
}

#[derive(Debug, Deserialize)]
struct NodeEntry {
    id: String,
    url: String,
}

fn resolve_selector(
    command: &str,
    mut args: Vec<String>,
) -> Result<(Option<String>, Vec<String>), String> {
    let Some(first) = args.first().cloned() else {
        return Ok((None, args));
    };
    if !first.starts_with('@') {
        return Ok((None, args));
    }
    let selector = first.trim_start_matches('@');
    let (node, target) = selector
        .split_once('/')
        .map_or((selector, None), |(node, target)| (node, Some(target)));
    if node.is_empty() || (target == Some("")) {
        return Err("invalid @node selector".into());
    }
    args.remove(0);
    if let Some(target) = target {
        args.insert(0, target.to_owned());
    }
    if command == "identify" || command == "status" {
        return Ok((Some(node.to_owned()), args));
    }
    Ok((Some(node.to_owned()), args))
}

fn load_node(id: &str) -> Result<String, String> {
    let path = node_book_path()?;
    let source = fs::read_to_string(&path)
        .map_err(|e| format!("cannot read node book {}: {e}", path.display()))?;
    let book: NodeBook = toml::from_str(&source).map_err(|e| format!("invalid node book: {e}"))?;
    let node = book
        .nodes
        .into_iter()
        .find(|node| node.id == id)
        .ok_or_else(|| format!("node `{id}` is not in the node book"))?;
    Ok(node.url)
}

fn node_book_path() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("mpv-targets/nodes.toml"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/mpv-targets/nodes.toml"))
}

fn config_root() -> Result<PathBuf, String> {
    if let Some(path) = env::var_os("XDG_CONFIG_HOME") {
        return Ok(PathBuf::from(path).join("mpv-targets"));
    }
    let home = env::var_os("HOME").ok_or("HOME is not set")?;
    Ok(PathBuf::from(home).join(".config/mpv-targets"))
}

fn local_daemon_url() -> Result<String, String> {
    let path = config_root()?.join("mpv-targets.toml");
    let source = fs::read_to_string(&path).map_err(|error| {
        format!(
            "cannot read local daemon config {}: {error}",
            path.display()
        )
    })?;
    let config = DaemonConfig::parse(&source)
        .map_err(|error| format!("invalid local daemon config {}: {error}", path.display()))?;
    daemon_url(&config)
}

fn daemon_url(config: &DaemonConfig) -> Result<String, String> {
    let mut address: std::net::SocketAddr = config
        .node
        .listen
        .parse()
        .map_err(|_| "local daemon has an invalid listen address".to_owned())?;
    if address.ip().is_unspecified() {
        address.set_ip(if address.is_ipv4() {
            std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        });
    }
    Ok(format!("wss://{address}"))
}

fn validate_command_args(command: &str, args: &[String]) -> Result<(), String> {
    let exact = |forms: &[&[&str]]| {
        forms.iter().any(|form| {
            form.len() == args.len()
                && form.iter().zip(args).all(|(expected, actual)| {
                    if expected.starts_with('<') {
                        !actual.starts_with("--")
                    } else {
                        *expected == actual
                    }
                })
        })
    };
    let valid = match command {
        "status" => exact(&[&[], &["<target>"], &["--json"], &["<target>", "--json"]]),
        "add" => return validate_add_args(args),
        "remove" => exact(&[&["<target>"], &["<target>", "--yes"]]),
        "rename" => exact(&[
            &["<target>", "<new-target>"],
            &["<target>", "<new-target>", "--yes"],
        ]),
        "start" | "stop" | "restart" => exact(&[&["<target>"]]),
        "enable" => exact(&[&["<target>"], &["<target>", "--start"]]),
        "disable" => exact(&[&["<target>"], &["<target>", "--stop"]]),
        "show-channels" => exact(&[&[]]),
        "set-channel" => exact(&[
            &["<target>", "<channel>"],
            &["<target>", "<channel>", "--restart"],
        ]),
        "clear-channel" => exact(&[&["<target>"], &["<target>", "--restart"]]),
        "playlist" => exact(&[&["<target>"], &["<target>", "<item>"]]),
        "loadfile" => exact(&[&["<target>", "<source>"]]),
        "play" | "pause" | "toggle-play" | "next" | "previous" | "mute" | "unmute"
        | "toggle-mute" | "fullscreen" | "loop" | "repeat" | "shuffle" | "unshuffle"
        | "cycle-audio" | "cycle-subtitle" | "disable-subtitle" => exact(&[&["<target>"]]),
        "identify" => args.is_empty(),
        "mpv" => args.len() >= 2,
        _ => return Err(format!("unknown command `{command}`; use `targets help`")),
    };
    valid
        .then_some(())
        .ok_or_else(|| format!("invalid arguments for `{command}`; use `targets help`"))
}

fn validate_add_args(args: &[String]) -> Result<(), String> {
    if args.is_empty() {
        return Err("target is required".into());
    }
    let mut index = 1;
    let mut seen = std::collections::HashSet::new();
    while index < args.len() {
        let option = args[index].as_str();
        if !matches!(option, "--from" | "--channel" | "--enable" | "--start") {
            return Err(format!("unknown add option `{option}`"));
        }
        if !seen.insert(option) {
            return Err(format!("duplicate add option `{option}`"));
        }
        index += 1;
        if matches!(option, "--from" | "--channel") {
            if index == args.len() || args[index].starts_with("--") {
                return Err(format!("{option} requires a value"));
            }
            index += 1;
        }
    }
    Ok(())
}

async fn status(client: &RemoteClient, url: &str, args: &[String]) -> Result<(), String> {
    let snapshot = client
        .snapshot()
        .await
        .ok_or("daemon did not send a snapshot")?;
    if args.iter().any(|arg| arg == "--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&snapshot).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    clear_interactive_terminal();
    println!(
        "NODE::{} [{}] :: TARGETS ::{}/{}",
        snapshot.node_id,
        url.trim_start_matches("wss://"),
        snapshot.health.targets_online,
        snapshot.health.targets_configured
    );
    println!(
        "===================================================================================================="
    );
    println!();
    for target in snapshot.targets {
        if args.first().is_none_or(|name| name == &target.name) {
            let playback = if target.disabled {
                "Disabled"
            } else if target.stopped {
                "Stopped"
            } else if !target.online {
                "Offline"
            } else if target.state.idle_active == Some(true) {
                "Idle"
            } else if target.state.paused == Some(true) {
                "Paused"
            } else {
                "Playing"
            };
            let volume = if target.state.muted == Some(true) {
                "Vol:Mute".to_owned()
            } else if let Some(volume) = target.state.volume {
                format!("Vol:{volume:3.0}")
            } else {
                "Vol:  ?".to_owned()
            };
            let loop_file = on_off_label(target.state.loop_file.as_deref());
            let loop_playlist = on_off_label(target.state.loop_playlist.as_deref());
            let title = target.state.title.as_deref().unwrap_or("-");
            let title = truncate_text(title, 37);
            println!(
                "{:<16} :: [{playback:<8}] :: [{volume:<8}] :: [Loop:{loop_file}] :: [Repeat:{loop_playlist}] :: {title}",
                quoted_target(&target.name),
            );
        }
    }
    Ok(())
}

async fn lifecycle(client: &RemoteClient, command: &str, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    let target_client = client.target(target);
    let response = match command {
        "start" => target_client.start().await,
        "stop" => target_client.stop_target().await,
        "restart" => target_client.restart().await,
        "enable" => target_client.enable().await,
        "disable" => target_client.disable().await,
        _ => unreachable!(),
    }
    .map_err(|e| e.to_string())?;
    let follow_up = (command == "enable" && args.iter().any(|arg| arg == "--start"))
        || (command == "disable" && args.iter().any(|arg| arg == "--stop"));
    let response = if follow_up {
        let result = if command == "enable" {
            target_client.start().await
        } else {
            target_client.stop_target().await
        };
        result.map_err(|e| e.to_string())?
    } else {
        response
    };
    print_target_response(response, target, command)
}

async fn remove_target(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    let confirmed = args.iter().any(|arg| arg == "--yes") || confirm_remove(target)?;
    if !confirmed {
        return Ok(());
    }
    print_target_response(
        client
            .target(target)
            .remove_target()
            .await
            .map_err(|e| e.to_string())?,
        target,
        "remove",
    )
}

async fn rename_target(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    let old = args.first().ok_or("target is required")?;
    let new = args.get(1).ok_or("new target name is required")?;
    let confirmed = args.iter().any(|arg| arg == "--yes") || confirm_rename(old, new)?;
    if !confirmed {
        return Ok(());
    }
    response_data(
        client
            .target(old)
            .rename(new.clone())
            .await
            .map_err(|e| e.to_string())?,
    )?;
    println!(
        "{} -> {} :: [Renamed]",
        quoted_target(old),
        quoted_target(new)
    );
    Ok(())
}

async fn identify(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    if let Some(node) = args.first()
        && !node.starts_with('@')
    {
        return Err("identify accepts an optional @node selector".into());
    }
    let snapshot = client
        .snapshot()
        .await
        .ok_or("daemon did not send a snapshot")?;
    for target in snapshot.targets.into_iter().filter(|target| target.online) {
        print_target_response(
            client
                .target(&target.name)
                .show_text(target.name.clone(), 4000)
                .await
                .map_err(|e| e.to_string())?,
            &target.name,
            "identify",
        )?;
    }
    Ok(())
}

async fn native_mpv(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    let command = args.get(1).ok_or("allowed native command is required")?;
    let native_args = args[2..].iter().map(|arg| json!(arg)).collect();
    print_target_response(
        client
            .request(
                target,
                Operation::Mpv {
                    command: command.clone(),
                    args: native_args,
                },
            )
            .await
            .map_err(|e| e.to_string())?,
        target,
        &format!("mpv:{command}"),
    )
}

fn confirm_rename(old: &str, new: &str) -> Result<bool, String> {
    use std::io::{self, IsTerminal, Write};
    if !io::stdin().is_terminal() {
        return Err("non-interactive rename requires --yes".into());
    }
    print_work_receipt("Rename", &format!("{old} -> {new}"));
    print_work_receipt("Move", &format!("targets/{old}/ -> targets/{new}/"));
    print!("\ncontinue? [y/N] ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

fn confirm_remove(target: &str) -> Result<bool, String> {
    use std::io::{self, IsTerminal, Write};
    if !io::stdin().is_terminal() {
        return Err("non-interactive removal requires --yes".into());
    }
    print_work_receipt("Remove", &format!("target {target}"));
    print!("This removes its target directory. Continue? [y/N] ");
    io::stdout().flush().map_err(|e| e.to_string())?;
    let mut answer = String::new();
    io::stdin()
        .read_line(&mut answer)
        .map_err(|e| e.to_string())?;
    Ok(matches!(answer.trim(), "y" | "Y"))
}

async fn add_target(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    let name = args.first().ok_or("target is required")?;
    let from = option_value(args, "--from")?;
    let channel = option_value(args, "--channel")?;
    let enable = args.iter().any(|arg| arg == "--enable");
    let start = args.iter().any(|arg| arg == "--start");
    let response = client
        .target(name)
        .add(from.clone(), enable || start)
        .await
        .map_err(|e| e.to_string())?;
    let data = response_data(response)?;
    if let Some(channel) = channel {
        response_data(
            client
                .target(name)
                .set_channel(Some(resolve_channel(client, &channel).await?), false)
                .await
                .map_err(|e| e.to_string())?,
        )?;
    }
    let started = if start {
        Some(response_data(
            client
                .target(name)
                .start()
                .await
                .map_err(|e| e.to_string())?,
        )?)
    } else {
        None
    };
    print_work_receipt("Added", &format!("target {name}"));
    if let Some(source) = &from {
        print_work_receipt("Source", &format!("targets/{source}/"));
    }
    for item in data
        .get("copied")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        if from.is_some() {
            print_work_receipt("Copied", item);
        } else {
            print_work_receipt("Created", &format!("targets/{name}/{item}"));
        }
    }
    print_work_receipt("Created", &format!("targets/{name}/"));
    print_work_receipt("Created", &format!("targets/{name}/scripts/"));
    print_work_receipt("Updated", "mpv-targets.toml");
    if start {
        print_work_receipt("Started", name);
    }
    let enabled = !data
        .get("disabled")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    let online = started
        .as_ref()
        .unwrap_or(&data)
        .get("online")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    println!();
    println!(
        "{} :: [Added] :: [{}] :: [{}]",
        quoted_target(name),
        if enabled { "Enabled" } else { "Disabled" },
        if online { "Online" } else { "Stopped" }
    );
    Ok(())
}

fn print_work_receipt(action: &str, object: &str) {
    println!(
        "[{action}]{} :: {object}",
        " ".repeat(9usize.saturating_sub(action.len()))
    );
}

fn option_value(args: &[String], option: &str) -> Result<Option<String>, String> {
    let Some(index) = args.iter().position(|arg| arg == option) else {
        return Ok(None);
    };
    args.get(index + 1)
        .cloned()
        .map(Some)
        .ok_or_else(|| format!("{option} requires a value"))
}

async fn mpv_action(client: &RemoteClient, command: &str, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    let accepts_all = !matches!(
        command,
        "loadfile" | "cycle-audio" | "cycle-subtitle" | "disable-subtitle"
    );
    let snapshot = client
        .snapshot()
        .await
        .ok_or("daemon did not send a snapshot")?;
    let bulk = target == "all";
    let targets = if bulk {
        if !accepts_all {
            return Err(format!("{command} does not support all"));
        }
        snapshot
            .targets
            .iter()
            .filter(|target| target.online && !target.disabled)
            .map(|target| target.name.clone())
            .collect()
    } else {
        vec![target.clone()]
    };
    let all_paused = bulk
        && snapshot
            .targets
            .iter()
            .filter(|target| target.online && !target.disabled)
            .all(|target| target.state.paused == Some(true));
    let all_muted = bulk
        && snapshot
            .targets
            .iter()
            .filter(|target| target.online && !target.disabled)
            .all(|target| target.state.muted == Some(true));
    for target in targets {
        let target_client = client.target(target.as_str());
        let response = match command {
            "play" => target_client.play().await,
            "pause" => target_client.pause().await,
            "toggle-play" if bulk => {
                if all_paused {
                    target_client.play().await
                } else {
                    target_client.pause().await
                }
            }
            "toggle-play" => target_client.toggle_play().await,
            "loadfile" => target_client.load(&args[1], LoadMode::Replace).await,
            "next" => target_client.next().await,
            "previous" => target_client.previous().await,
            "mute" => target_client.mute().await,
            "unmute" => target_client.unmute().await,
            "toggle-mute" if bulk => {
                if all_muted {
                    target_client.unmute().await
                } else {
                    target_client.mute().await
                }
            }
            "toggle-mute" => target_client.toggle_mute().await,
            "fullscreen" => target_client.fullscreen().await,
            "loop" => {
                let enabled = snapshot
                    .targets
                    .iter()
                    .find(|candidate| candidate.name == target)
                    .and_then(|target| target.state.loop_file.as_deref())
                    == Some("on");
                target_client
                    .set_loop(if enabled {
                        LoopState::Off
                    } else {
                        LoopState::On
                    })
                    .await
            }
            "repeat" => {
                let enabled = snapshot
                    .targets
                    .iter()
                    .find(|candidate| candidate.name == target)
                    .and_then(|target| target.state.loop_playlist.as_deref())
                    == Some("on");
                target_client
                    .set_repeat(if enabled {
                        LoopState::Off
                    } else {
                        LoopState::On
                    })
                    .await
            }
            "shuffle" => target_client.shuffle().await,
            "unshuffle" => target_client.unshuffle().await,
            "cycle-audio" => target_client.cycle_audio().await,
            "cycle-subtitle" => target_client.cycle_subtitle().await,
            "disable-subtitle" => target_client.disable_subtitle().await,
            _ => unreachable!(),
        }
        .map_err(|e| e.to_string())?;
        print_target_response(response, &target, command)?;
    }
    Ok(())
}

async fn channel(client: &RemoteClient, command: &str, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    let selected = if command == "clear-channel" {
        None
    } else {
        Some(resolve_channel(client, args.get(1).ok_or("channel is required")?).await?)
    };
    let restart = args.iter().any(|arg| arg == "--restart");
    let data = response_data(
        client
            .target(target)
            .set_channel(selected.clone(), restart)
            .await
            .map_err(|e| e.to_string())?,
    )?;
    if let Some(selected) = selected {
        print!(
            "{} :: [{}] :: [SET]",
            quoted_target(target),
            channel_display_name(&selected)
        );
        if data
            .get("restart_completed")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            print!(" :: [Restarted]");
        }
        println!();
        Ok(())
    } else {
        print_target_data(&data, target, "channel:clear")
    }
}

async fn show_channels(client: &RemoteClient) -> Result<(), String> {
    let channels = channel_entries(client).await?;
    clear_interactive_terminal();
    println!(":: [Channels] ::\n");
    for (index, path) in channels.iter().enumerate() {
        let name = PathBuf::from(path)
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(path)
            .to_owned();
        println!("{}. {name}", index + 1);
    }
    Ok(())
}

fn channel_display_name(channel: &str) -> &str {
    channel.rsplit('/').next().unwrap_or(channel)
}

async fn resolve_channel(client: &RemoteClient, value: &str) -> Result<String, String> {
    if value.contains("://") || PathBuf::from(value).is_absolute() {
        return Ok(value.to_owned());
    }
    let entries = channel_entries(client).await?;
    if let Ok(number) = value.parse::<usize>() {
        return entries
            .get(
                number
                    .checked_sub(1)
                    .ok_or("channel numbering starts at 1")?,
            )
            .cloned()
            .ok_or_else(|| format!("channel number {number} is not in the list"));
    }
    entries
        .into_iter()
        .find(|path| {
            PathBuf::from(path)
                .file_name()
                .and_then(|name| name.to_str())
                == Some(value)
        })
        .ok_or_else(|| format!("channel `{value}` is not in the node channel directory"))
}

async fn channel_entries(client: &RemoteClient) -> Result<Vec<String>, String> {
    let data = response_data(client.list_channels().await.map_err(|e| e.to_string())?)?;
    data.get("channels")
        .and_then(Value::as_array)
        .ok_or_else(|| "daemon returned an invalid channel list".into())
        .map(|entries| {
            entries
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
}

async fn playlist(client: &RemoteClient, args: &[String]) -> Result<(), String> {
    let target = args.first().ok_or("target is required")?;
    if let Some(item) = args.get(1) {
        let index: u64 = item.parse().map_err(|_| "playlist item must be a number")?;
        if index == 0 {
            return Err("playlist numbering starts at 1".into());
        }
        return print_target_response(
            client
                .target(target)
                .playlist_play((index - 1) as u32)
                .await
                .map_err(|e| e.to_string())?,
            target,
            &format!("playlist:{index}"),
        );
    }
    let response = client
        .target(target)
        .playlist()
        .await
        .map_err(|e| e.to_string())?;
    let data = response_data(response)?;
    let entries = data
        .as_array()
        .ok_or("daemon returned an invalid playlist")?;
    clear_interactive_terminal();
    println!("{} :: [Current Playlist]", quoted_name(target));
    let stdout = io::stdout();
    let mut stdout = io::BufWriter::new(stdout.lock());
    for (index, entry) in entries.iter().enumerate() {
        let marker = if entry
            .get("current")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            ">"
        } else {
            " "
        };
        let title = playlist_item_display(entry);
        let title = truncate_text(title, 72);
        if let Err(error) = writeln!(stdout, "{marker} {:>3} {title}", index + 1) {
            if error.kind() == io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(error.to_string());
        }
    }
    match stdout.flush() {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn response_data(response: Response) -> Result<Value, String> {
    match response {
        Response::Success { data, .. } => Ok(data),
        Response::Error { error, .. } => Err(format!("{:?}: {}", error.code, error.message)),
    }
}
fn quoted_target(target: &str) -> String {
    format!("{:<16}", quoted_name(target))
}

fn clear_interactive_terminal() {
    if std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        print!("\x1b[2J\x1b[H");
    }
}

fn quoted_name(target: &str) -> String {
    format!("\"{}\"", target.to_uppercase())
}

fn playlist_item_display(entry: &Value) -> &str {
    if let Some(title) = entry
        .get("title")
        .and_then(Value::as_str)
        .filter(|title| !title.is_empty())
    {
        return title;
    }
    let filename = entry.get("filename").and_then(Value::as_str).unwrap_or("-");
    if filename.starts_with("http") {
        filename
    } else {
        filename.rsplit('/').next().unwrap_or(filename)
    }
}

fn truncate_text(value: &str, width: usize) -> String {
    if value.chars().count() <= width {
        value.to_owned()
    } else {
        format!(
            "{}...",
            value
                .chars()
                .take(width.saturating_sub(3))
                .collect::<String>()
        )
    }
}

fn on_off_label(value: Option<&str>) -> &'static str {
    if value == Some("on") { "On " } else { "Off" }
}

fn action_label(action: &str) -> String {
    action
        .split(':')
        .map(|part| {
            part.split('-')
                .map(|part| {
                    let mut chars = part.chars();
                    chars
                        .next()
                        .map(|first| first.to_uppercase().chain(chars).collect())
                        .unwrap_or_default()
                })
                .collect::<Vec<String>>()
                .join(" ")
        })
        .collect::<Vec<String>>()
        .join(": ")
}

fn result_label(action: &str, data: &Value) -> &'static str {
    if data.get("online").and_then(Value::as_bool) == Some(true) {
        "Online"
    } else if data.get("stopped").and_then(Value::as_bool) == Some(true) {
        "Stopped"
    } else if data.get("disabled").and_then(Value::as_bool) == Some(true) {
        "Disabled"
    } else if data.get("disabled").and_then(Value::as_bool) == Some(false) {
        "Enabled"
    } else if data.get("restart_completed").and_then(Value::as_bool) == Some(true) {
        "Restarted"
    } else if action == "remove" {
        "Removed"
    } else {
        "OK"
    }
}

fn print_target_response(response: Response, target: &str, action: &str) -> Result<(), String> {
    let data = response_data(response)?;
    print_target_data(&data, target, action)
}

fn print_target_data(data: &Value, target: &str, action: &str) -> Result<(), String> {
    let action = action_label(action);
    if data.is_null() || data.is_object() {
        println!(
            "{} :: [{action}] :: [{}]",
            quoted_target(target),
            result_label(&action.to_lowercase(), data)
        );
    } else {
        println!(
            "{} :: [{action}] :: {}",
            quoted_target(target),
            serde_json::to_string(data).map_err(|e| e.to_string())?
        );
    }
    Ok(())
}
fn take_option(args: &mut Vec<String>, option: &str) -> Result<Option<String>, String> {
    let matches: Vec<_> = args
        .iter()
        .enumerate()
        .filter(|(_, arg)| *arg == option)
        .map(|(index, _)| index)
        .collect();
    match matches.as_slice() {
        [] => Ok(None),
        [index] if index + 1 < args.len() && !args[index + 1].starts_with("--") => {
            let index = *index;
            args.remove(index);
            Ok(Some(args.remove(index)))
        }
        [_] => Err(format!("{option} requires a value")),
        _ => Err(format!("{option} may be supplied only once")),
    }
}
fn print_help() {
    println!(
        "targets [--url WSS_URL] <command>\n\nstatus [target] [--json]\nadd <target> [--from TARGET] [--channel NAME_OR_PATH_OR_URL] [--enable] [--start]\nremove <target> [--yes]\nstart|stop|restart <target>\nenable <target> [--start]\ndisable <target> [--stop]\nrename <target> <new-target> [--yes]\nshow-channels [@node]  list files in the node channel directory\nset-channel <target> <number|name|path-or-url> [--restart]  select the target channel\nclear-channel <target> [--restart]  clear the target channel\nplaylist <target> [item]  show or jump the current mpv playlist\nplay|pause|toggle-play <target|all>\nloadfile <target> <path-or-url>\nnext|previous <target|all>\nmute|unmute|toggle-mute <target|all>\nfullscreen|loop|repeat|shuffle|unshuffle <target|all>\ncycle-audio|cycle-subtitle|disable-subtitle <target>\nidentify [@node]\nmpv <target> <allowed-native-command> [args...]"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    #[test]
    fn playlist_display_matches_panebot_fallback() {
        let titled = json!({"filename": "/media/raw.mkv", "title": "Display title"});
        let local = json!({"filename": "/media/video/movie.mkv"});
        let remote = json!({"filename": "https://example.test/live/index.m3u8"});
        assert_eq!(playlist_item_display(&titled), "Display title");
        assert_eq!(playlist_item_display(&local), "movie.mkv");
        assert_eq!(
            playlist_item_display(&remote),
            "https://example.test/live/index.m3u8"
        );
    }

    #[test]
    fn channel_receipt_uses_the_filename() {
        assert_eq!(
            channel_display_name("/srv/channels/cameras.m3u8"),
            "cameras.m3u8"
        );
        assert_eq!(
            channel_display_name("https://example.test/live.m3u8"),
            "live.m3u8"
        );
    }

    #[test]
    fn command_grammar_rejects_ignored_arguments() {
        assert!(validate_command_args("start", &args(&["music"])).is_ok());
        assert!(validate_command_args("start", &args(&["music", "--start"])).is_err());
        assert!(validate_command_args("disable", &args(&["music", "--stop"])).is_ok());
        assert!(validate_command_args("disable", &args(&["music", "--start"])).is_err());
        assert!(
            validate_command_args(
                "loadfile",
                &args(&["music", "https://example.test/a?x=1&y=2"])
            )
            .is_ok()
        );
    }

    #[test]
    fn add_grammar_accepts_only_documented_options() {
        assert!(
            validate_command_args(
                "add",
                &args(&[
                    "music",
                    "--from",
                    "movies",
                    "--channel",
                    "cameras.m3u",
                    "--start"
                ])
            )
            .is_ok()
        );
        assert!(validate_command_args("add", &args(&["music", "--from"])).is_err());
        assert!(validate_command_args("add", &args(&["music", "--enable", "--enable"])).is_err());
        assert!(validate_command_args("add", &args(&["music", "--config", "mpv.conf"])).is_err());
        assert!(validate_command_args("add", &args(&["music", "--playlist", "old.m3u"])).is_err());
    }

    #[test]
    fn channel_command_grammar_is_explicit() {
        assert!(validate_command_args("show-channels", &[]).is_ok());
        assert!(validate_command_args("set-channel", &args(&["music", "cameras.m3u"])).is_ok());
        assert!(
            validate_command_args(
                "set-channel",
                &args(&["music", "/srv/media/cameras.m3u8", "--restart"])
            )
            .is_ok()
        );
        assert!(validate_command_args("clear-channel", &args(&["music"])).is_ok());
        assert!(validate_command_args("set-channel", &args(&["music"])).is_err());
    }

    #[test]
    fn global_url_must_be_complete_and_unique() {
        let mut values = args(&["status", "--url", "wss://127.0.0.1:9876"]);
        assert_eq!(
            take_option(&mut values, "--url").unwrap().as_deref(),
            Some("wss://127.0.0.1:9876")
        );
        assert_eq!(values, args(&["status"]));
        assert!(take_option(&mut args(&["status", "--url"]), "--url").is_err());
        assert!(take_option(&mut args(&["--url", "a", "--url", "b"]), "--url").is_err());
    }

    #[test]
    fn local_url_uses_the_configured_listener() {
        let mut config = DaemonConfig::parse(
            r#"[node]
id = "fez"
listen = "10.11.12.21:9876"
[tls]
certificate = "tls/server.crt"
private_key = "tls/server.key"
"#,
        )
        .unwrap();
        assert_eq!(daemon_url(&config).unwrap(), "wss://10.11.12.21:9876");
        config.node.listen = "0.0.0.0:9876".into();
        assert_eq!(daemon_url(&config).unwrap(), "wss://127.0.0.1:9876");
    }
}
