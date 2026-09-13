# mpv-targets v1 specification

Status: draft. This is the canonical product contract for v1. It replaces
the prior rewrite briefs. Code, deployment, clients, and documentation must
follow it; none may silently extend it.

## 1. Product

`mpv-targets` is a small standalone service for named mpv execution targets.

```text
stream, file path, URL, or M3U -> named target -> mpv
```

It owns target configuration, mpv process supervision, mpv IPC, observable
state, and the client-neutral control protocol.

It is not a media center, library, discovery service, downloader, playlist
creator, NVR, or client UI. mpv remains the player. Upstream tools bring the
streams.

The product promise is literal: send a playable source to a named target and
observe or control its execution.

### PaneBot relationship

PaneBot is the first demanding external client of `mpv-targets`, not part of
the service. Its eventual rewrite must use the same documented protocol and
shared `mpv_targets` library available to every other client. It may preserve
successful PaneBot interactions, but those interactions do not become daemon
features merely because PaneBot needs them. There are no private endpoints,
shared runtime state, or remote filesystem shortcuts.

## 2. v1 compatibility contract

v1 is a clean implementation, not a feature redesign. It preserves the
behavior already proven by the current implementation unless this specification
explicitly changes it.

The preserved public behavior includes:

- named local and remote targets;
- local files, URLs, and local or remote M3U playlists;
- configured startup playlists;
- target state snapshots, including current title, playback, mute, and online
  state;
- mpv lifecycle: start, stop, restart, recovery, and safe reattachment;
- persistent target enable/disable state;
- common mpv controls: play/pause, next/previous, mute, audio/subtitle cycle,
  shuffle, fullscreen, and native-command escape hatch;
- correlated command success and error results;
- TLS WebSocket transport and the shared Rust client;
- unattended Linux operation under an ordinary supervisor.

The v1 rewrite may simplify internal code and remove stale concepts. It must
not materially change these externally observable functions without an
explicit amendment to this specification.

## 3. Names and deliverables

```text
repository and package: mpv-targets
daemon binary:          mpv-targetsd
operator binary:        targets
Rust library:           mpv_targets
```

`targets` is the broader endpoint idea and the operator command. This product
remains explicitly mpv-specific. It does not introduce a generic executor
framework.

## 4. Non-goals

- No interactive setup wizard in the daemon or deployer.
- No bundled sender UI, media browser, picker, or playlist curator.
- No remote filesystem editor or generic mpv.conf editor.
- No authentication, account system, PKI, certificate renewal service, or
  fleet-control plane.
- No persistent database for transient playback or stopped state.
- No generic player abstraction beyond mpv.

## 5. Review order

The remaining sections are written and approved in this order:

1. filesystem, daemon configuration, and answer-file deployment;
2. target lifecycle and recovery semantics;
3. protocol, snapshots, events, errors, and shared-library API;
4. `targets` operator-client contract;
5. TLS and systemd operation;
6. acceptance matrix and implementation sequence.

## 6. Filesystem, configuration, and answer-file deployment

### 6.1 Configuration ownership

There are two declarative inputs with distinct jobs:

- An **answer file** is the non-interactive installation input. It declares a
  node and its initial target topology.
- `mpv-targets.toml` is the daemon's real, local runtime configuration after
  deployment.

The answer file is not a second runtime configuration protocol. Deployment
materializes the daemon configuration and target directories from it. Clients
may later provide a friendly editor, but that editor must produce the same
declarative input or invoke the narrow public configuration operations; it may
not create another setup model.

Deployment profiles are first-class deployment inputs. A profile carries the
hardware- and host-specific material needed to install a target box: platform
packages, service/session integration, target `mpv.conf` defaults, and any
profile assets. The answer file selects and supplies values for that profile.
Profiles may differ for a desktop, Raspberry Pi, or another Linux host; they
must all materialize the same generic `mpv-targets` service contract.

The daemon and public protocol never branch on a hardware profile. Hardware
choices belong to the deployment layer, not target semantics.

### 6.2 XDG layout

The default XDG layout for the service user is:

```text
~/.config/mpv-targets/
├── mpv-targets.toml
├── tls/
│   ├── server.crt
│   └── server.key
└── targets/
    └── <target-name>/
        ├── mpv.conf
        └── scripts/

~/.local/state/mpv-targets/
└── logs/
    └── <target-name>.log

$XDG_RUNTIME_DIR/mpv-targets/
├── service.lock
└── <target-name>.sock
```

The daemon accepts an explicit configuration path for packaging and test use.
It derives the surrounding target, state, TLS, and runtime paths from that
configuration root. It does not scan arbitrary home-directory configuration.

### 6.3 Target identity and configuration

A target name is its stable, user-facing identifier. It is the name used in
configuration, the protocol, the `targets` CLI, directories, sockets, and
operator scripts. There is no separate display-name identity in v1.

The daemon configuration owns only service-level target fields:

```toml
[node]
id = "fez"
listen = "127.0.0.1:9876"

[tls]
certificate = "tls/server.crt"
private_key = "tls/server.key"

[[targets]]
name = "music"
startup_playlist = "/home/mpv-targets/premiumize/music.m3u8"
# disabled = false
```

`disabled` defaults to `false`. Its meaning and the corresponding live
lifecycle operations are defined in the next section.

`startup_playlist` is optional. It is either an absolute local path or a URL.
The daemon passes URLs to mpv unchanged. It does not download, inspect, or
validate playlist contents. The answer-file deployer resolves any relative
source it installs before writing the daemon configuration, so daemon runtime
configuration contains no ambiguous relative playlist paths.

Target `mpv.conf` remains ordinary native mpv configuration. It owns the
launch posture, including `pause=yes` or `pause=no`, plus all platform-specific
video, audio, window, and script settings. The service never duplicates those
mpv options in its protocol configuration.

### 6.4 Answer-file contract

The answer file is intentionally small. It declares:

- node id and listener address;
- TLS input or self-signed certificate generation;
- initial targets, their `disabled` state, and startup playlists;
- the source content for each target's ordinary `mpv.conf`;
- the user service installation details.

It does not declare remote nodes, client address books, media discovery,
playlist construction, stream credentials, or client UI choices.

Deployment is non-interactive. It validates all input before changing the
target host, then creates or updates only its declared service configuration,
target configuration, TLS material, and user service unit. It does not delete
playlists, scripts, logs, or unrelated files. Any replacement behavior must be
an explicit command and state exactly which files it will replace.

Self-signed TLS is a supported default deployment mode. The deployer may
generate one certificate and private key for the node, or install an explicitly
supplied pair. It does not become a certificate authority, renewal service, or
browser-profile manager.

## 7. Target lifecycle and recovery

### 7.1 One persistent rule

Configured targets run by default. `disabled = true` is the sole persistent
exception: a disabled target must be off until an operator enables it again.

There is no `desired_online`, autostart policy class, persistent live-state
database, or second lifecycle vocabulary.

The target's native `mpv.conf` decides whether an enabled target begins paused
or playing. The daemon starts the process; it does not duplicate mpv's pause
setting.

### 7.2 Daemon startup and restart

For every configured target, the daemon performs one of these decisions at
startup:

1. If `disabled = true`, ensure the configured target pane is stopped and do
   not launch or recover it.
2. If an enabled target has a live configured IPC socket, attach to that pane.
3. If an enabled target has a confirmed stale socket, remove only that socket.
4. If an enabled target has no live pane, launch mpv and wait for its configured
   IPC socket before treating it as online.

The daemon never adopts an arbitrary mpv process. A configured target is known
only through its configured IPC socket.

A graceful daemon restart leaves healthy enabled panes alive. The replacement
daemon reattaches through those sockets. It does not create a second mpv
process for an already-live target.

### 7.3 Live operations

The service exposes these target operations:

| Operation | Effect |
| --- | --- |
| `start` | Launches an enabled, currently stopped target. Starting a disabled target is rejected with a clear error. |
| `stop` | Stops the target intentionally for the current daemon session. It does not change `disabled`. |
| `restart` | Stops and relaunches an enabled target. For a disabled target, it ensures the target is stopped and does not relaunch it. |
| `enable` | Persists `disabled = false`; it does not itself launch the target. |
| `disable` | Persists `disabled = true`; it does not itself stop a currently running target. |
| `set-startup-playlist` | Persists the startup playlist; optional restart applies it immediately. |

The `targets` CLI may compose existing operations for ergonomic forms such as
`targets disable music --stop` and `targets enable music --start`. Those are
not additional protocol operations.

### 7.4 Recovery

An intentional `stop` marks the target stopped for the current daemon session.
It remains off until an explicit `start` or `restart`, but an enabled target may
launch again after a later daemon restart.

An unexpected mpv exit or irrecoverable configured IPC loss is a failure. For
an enabled target that was not intentionally stopped, the supervisor reports
the failure and retries launch with bounded backoff. A disabled or intentionally
stopped target is never recovered.

Recovery is an internal implementation behavior. Clients observe it through
target state and events; they do not manage child processes or recovery loops.

## 8. Protocol, snapshots, events, and shared library

### 8.1 Transport and message model

The public protocol is JSON text over one TLS WebSocket connection. The daemon
sends a complete snapshot immediately after a successful connection, then sends
incremental state events. A client may issue concurrent requests; every request
has one correlated terminal success or error response.

The protocol version is an integer carried by every snapshot. v1 does not
implement protocol negotiation. A client that cannot use the advertised version
must reject the connection clearly.

Request shape:

```json
{
  "id": "client-42",
  "target": "music",
  "operation": { "kind": "target_restart" }
}
```

Response shape:

```json
{ "result": "success", "id": "client-42", "data": {} }
{ "result": "error", "id": "client-42", "error": { "code": "target_disabled", "message": "target is disabled; enable it before starting" } }
```

An accepted command response is distinct from later playback failure. For
example, a successful `loadfile` means mpv accepted the request; a later stream
failure appears through target state or an event.

### 8.2 Service operations

Service operations use explicit names so they cannot be confused with native
mpv commands:

```text
target_start
target_stop
target_restart
target_enable
target_disable
target_rename
target_set_startup_playlist
```

`target_set_startup_playlist` carries `playlist` (a string or `null`) and an
explicit `restart` boolean. Its success data reports the persisted playlist,
whether restart was requested, and whether restart completed.

`target_rename` carries one validated replacement name. It is an atomic service
operation: the daemon updates target configuration and its target configuration
directory, preserves `mpv.conf` and scripts, and reports the old and new name.
An enabled rename always stops the old target identity and starts the new target
identity. A disabled rename remains disabled and does not launch a pane. There
is no soft rename or skip-restart option for enabled targets. A failed rename
leaves the old target intact. Its success data reports `from`, `to`, and
`restarted`. `all` is reserved as the operator bulk selector and cannot be a
target name.

Service-operation responses are small, stable facts: stopped/online state for
lifecycle operations, `disabled` for enable/disable, and playlist/restart facts
for startup-playlist mutation. They do not pretend to report eventual media
playback success.

### 8.3 Native mpv operation

The native operation retains mpv's JSON argument model:

```json
{
  "id": "client-43",
  "target": "music",
  "operation": {
    "kind": "mpv",
    "command": "loadfile",
    "args": ["https://example.test/channel.m3u", "replace"]
  }
}
```

The service exposes a deliberately limited native command allowlist:

```text
get_property       set_property        cycle              add
stop               seek                revert-seek
playlist-next      playlist-prev       playlist-play-index
playlist-remove    playlist-move       playlist-shuffle
playlist-unshuffle playlist-clear
loadfile           loadlist
keypress           keydown             keyup
show-text
```

`show-text` is included for transient target identification and ordinary mpv
OSD use. Process-control and arbitrary execution commands, including `quit`,
are not exposed as native operations; target lifecycle belongs to the service.

Native-command success data is mpv's native JSON result. The service does not
invent a second typed vocabulary for mpv.

### 8.4 Snapshot and events

The initial message is a `snapshot` event. It contains:

- `protocol_version`;
- node id;
- health: `ready`, `idle`, or `degraded`, plus configured, expected-online,
  and online target counts;
- one target record per configured target.

A target record contains its authoritative name; `disabled`, `stopped`, and
`online` state; startup playlist; observed playback fields; and a monotonic
revision. Observed playback fields include paused, muted, volume, title,
playlist position/count, idle state, duration/position, loop settings, and
audio/subtitle track information.

The daemon emits `target_changed` events containing a target name, next
revision, and only changed fields. Clients apply them in order to their cached
snapshot. A revision gap is a protocol failure: the client must reconnect and
obtain a fresh snapshot rather than guessing state.

`targets_expected_online` counts targets that are both enabled and not
intentionally stopped. `targets_online` counts those expected targets that are
currently online. Health is `idle` when no target is expected online, `ready`
when all expected targets are online, and `degraded` otherwise.

### 8.5 Errors and timeouts

The stable protocol error codes are:

```text
invalid_request       unknown_target       target_already_stopped
target_not_running    target_disabled      config_rejected
target_offline        mpv_rejected         timeout
ipc_lost              shutting_down
```

Timeout means the requester did not observe a terminal response. It does not
cancel a command that may already have reached the daemon; the eventual state
must be observed through a new snapshot or event.

### 8.6 Shared Rust library

`mpv_targets` is the canonical client-facing Rust library. The daemon and
`targets` use it; PaneBot and outside clients receive the same public surface.

Its connection type supports:

```rust
RemoteClient::connect(options)
client.snapshot()
client.subscribe()
client.disconnected()
client.close()
```

It owns one reader/writer connection task, request correlation, the current
snapshot, event delivery, and explicit disconnect reporting. It does not choose
reconnection policy; a long-running client owns that decision.

The library provides typed convenience builders and methods for the healthy
common mpv client surface:

- load file/list/URL, replace, append, and append-play;
- play, pause, stop playback, next, previous, seek, and playlist selection;
- playlist clear, remove, move/reorder, shuffle, and unshuffle;
- deterministic play, pause, mute, unmute, volume, fullscreen, loop, and
  repeat controls, plus explicit playback and mute toggles;
- audio and subtitle select, disable, cycle, and visibility toggle;
- transient OSD text;
- native allowed-command escape hatch;
- every service operation in section 8.2.

These conveniences compile to the same native mpv or service operations
documented above. They remove repeated command strings from clients; they do
not create a separate client-only protocol.

## 9. `targets` operator client

`targets` is a thin, non-interactive system/operator client built entirely on
`mpv_targets`. It is not PaneBot, a sender UI, a target picker, a media browser,
or a deployment program.

### 9.1 Target and node selection

```text
music          local target named music
@fez/music     target music on node fez
@fez           node fez, where a command operates on the whole node
@fez/all       all applicable targets on node fez
```

A bare target always resolves through the local daemon configuration. Remote
access is explicit with `@node/target`; there is no hidden default remote node.
`all` and `@node/all` are reserved bulk selectors for applicable playback-wide
commands and cannot name a real target.

The optional client-side node book is separate from daemon configuration:

```toml
[[nodes]]
id = "fez"
url = "wss://fez.marlovious.net:9876"
trust_certificate = "certs/fez.crt"
```

It supplies only remote connection addresses and optional certificate trust.
It does not declare targets, alter daemon configuration, or participate in
deployment. `--url` connects directly and `--trust-certificate` overrides
certificate trust for that invocation.

### 9.2 Commands

```text
targets status [target | @node | @node/target] [--json]

targets start <target>
targets stop <target>
targets restart <target>
targets enable <target> [--start]
targets disable <target> [--stop]
targets rename <target> <new-target>

targets set-playlist <target> <path-or-url> [--restart]
targets clear-playlist <target> [--restart]

targets play <target|all>
targets pause <target|all>
targets toggle-play <target|all>
targets loadfile <target> <path-or-url>
targets next <target|all>
targets previous <target|all>
targets mute <target|all>
targets unmute <target|all>
targets toggle-mute <target|all>
targets fullscreen <target|all>
targets loop <target|all>
targets repeat <target|all>
targets shuffle <target|all>
targets unshuffle <target|all>
targets cycle-audio <target>
targets cycle-subtitle <target>
targets playlist <target> [item]
targets identify [@node]

targets mpv <target> <allowed-native-command> [args...]
```

`all` expands client-side to the currently online targets on the selected node.
It is valid for play/pause, mute/unmute, fullscreen, next/previous, loop,
repeat, shuffle, and unshuffle. It is not valid for lifecycle, configuration,
rename, loading, playlist inspection/selection, track selection, or raw native
commands.

`toggle-play all` is intentionally node-wide: if every online target is paused
it plays them all; otherwise it pauses them all. `toggle-mute all` similarly
unmutes every target only when they are all muted; otherwise it mutes them all.

`loop` toggles mpv's current-item loop. `repeat` toggles mpv's playlist loop.
Both are observable mpv state. `shuffle` and `unshuffle` remain actions: mpv
does not expose a reliable persistent shuffle-mode property, so the service and
status output do not invent one.

Each command prints a concise correlated receipt or a clear protocol error.
The client does not claim that a later media failure was part of a successful
command receipt.

`loadfile` passes a literal source to the target's mpv. It never transfers a
local file between hosts. A remote target therefore needs a URL or a path that
exists on that target, such as its normalized rclone/FUSE mount path.

`playlist <target>` prints the target's current playlist as a compact,
one-based numbered list, marking the current item. `playlist <target> <item>`
selects that item and begins playback. It is intentionally a view/jump tool,
not an interactive picker or playlist editor.

`identify` is a node-wide operator action. It sends mpv `show-text` to every
online target on the selected node with that target's canonical name and a
4,000 ms duration. For example, `targets identify @living-room` briefly shows
each living-room pane's name inside its own video window. It has no persistent
effect and introduces no service operation beyond the allowed native mpv
command.

`rename` is deliberately destructive because target names carry filesystem,
runtime, and client identity. Before making any change, `targets` prints the
full migration and requires confirmation:

```text
$ targets rename toons movies

rename:  toons -> movies
restart: toons -> movies
move:    ~/.config/mpv-targets/targets/toons
         -> ~/.config/mpv-targets/targets/movies

continue? [y/N]
```

`N` or Enter makes no change. A non-interactive invocation must supply `--yes`;
it never silently performs a rename. After confirmation, the command validates
collisions and performs the service operation atomically, then reports the old
and new name and successful restart. An error leaves the original target
usable. For a persistently disabled target, the same confirmation replaces the
`restart` line with `disabled: remains disabled`; it still lists the directory
move and makes no mpv liveness check.

### 9.3 Status

Human status is intentionally scannable and clears an interactive terminal
before printing. Piped output and `--json` do not emit terminal control codes.

```text
NODE::fez [fez.marlovious.net:9876] :: TARGETS ::4/4
====================================================================================================

TARGET         STATE     PLAYBACK   AUDIO    LOOP     REPEAT   TITLE
music          online    playing    muted    on       off      1HD Music Television (1080p)
standard       online    paused     muted    off      on       Aelita Queen Of Mars.avi
```

The count is the compact lifecycle signal. Status does not add a redundant
disabled column. It reports loop and repeat state from mpv, but not a made-up
shuffle mode.

### 9.4 Boundaries

`targets` uses no private daemon access, target-directory writes, or raw
WebSocket implementation. It routes through `mpv_targets` exactly as PaneBot
and outside Rust clients do.

## 10. TLS and systemd operation

### 10.1 TLS boundary

The daemon serves only `wss://`. TLS is mandatory transport, not an optional
feature switch. It encrypts the control connection and identifies the daemon to
clients. It does not authenticate clients: v1 has no account, token,
authorization, client-certificate, or certificate-renewal system.

The daemon loads one configured certificate/key pair at startup. Failure to
read the pair, parse it, or bind the listener is a clear startup failure. The
answer-file deployer either generates a simple self-signed pair or installs a
pair supplied by the operator. It performs no ongoing TLS work after that.

Clients use normal system trust when possible, or explicitly trust the exact
service certificate through their client configuration. Browser users may
perform the browser's normal deliberate trust step for a self-signed node.
This is a deployment/client concern, not protocol behavior.

### 10.2 Linux service operation

`mpv-targetsd` is an ordinary foreground Linux process. It does not require,
detect, or configure Hyprland, Wayland, X11, a monitor, or any other
hardware/display environment.

The default systemd installation is one systemd **user** service:

```text
~/.config/systemd/user/mpv-targets.service
```

It runs:

```text
mpv-targetsd --config %h/.config/mpv-targets/mpv-targets.toml
```

The unit runs the daemon and its mpv children as the configured target user.
The caller supplies whatever environment the target's mpv configuration needs.
For example, a visible desktop pane needs that desktop's environment; a
headless target may use a different mpv configuration. Those are mpv/platform
choices, not service behavior.

Systemd owns the daemon process and restarts it after daemon failure. The daemon
owns target processes. A deliberate daemon stop/restart must not sweep healthy
mpv panes away; the next daemon attaches through their configured IPC sockets.
Target stop/restart remains a daemon protocol operation, not a systemd unit
operation.

The daemon stays in the foreground and writes concise operational lines to its
standard output/error for journald. It reports ready, idle, degraded, target
launch/recovery, target stop, and client/protocol failures without ANSI control
codes or a secondary logging framework.

## 11. Acceptance matrix and construction order

### 11.1 Acceptance matrix

v1 is accepted only when these observable behaviors pass automated tests and a
real target-host run. The existing v2 implementation is behavioral evidence;
no v2 source file is a required implementation dependency.

#### Configuration and deployment

- Invalid names, duplicate names, invalid listener/TLS paths, and malformed
  answer files fail before deployment changes the host.
- An answer file plus a selected deployment profile materializes the same
  daemon configuration contract on every supported Linux target host.
- Reapplying deployment does not delete playlists, scripts, logs, certificates,
  or unrelated files.
- Hardware profiles affect installation assets and `mpv.conf` material only;
  they do not change the daemon protocol or target semantics.

#### Target lifecycle

- Every enabled configured target attaches to a live configured socket or
  launches one mpv process.
- A disabled target is off after daemon startup/restart.
- `stop` suppresses recovery for the current daemon session without persisting
  a second lifecycle state.
- Unexpected mpv loss recovers an enabled, non-stopped target.
- Daemon restart reattaches a live target and never duplicates its mpv process.
- A launch failure cleans up its child before recovery; it cannot leave orphan
  mpv processes behind.
- `start`, `stop`, `restart`, `enable`, `disable`, and startup-playlist changes
  produce the exact lifecycle results defined in section 7.
- Rename validates before mutation, requires CLI confirmation, moves the
  declared target paths, restarts an enabled target under the new identity,
  preserves disabled-off state, and leaves the old target intact on failure.

#### Media and state

- A local path, remote-accessible path, URL, and M3U reach mpv unchanged except
  for documented local configuration-path resolution.
- Startup playlist URLs are passed to mpv unchanged.
- Snapshots and incremental events report online, stopped, disabled, playback,
  mute, title, playlist, audio/subtitle, loop, and repeat state correctly.
- Playlist listing/jump uses one-based operator numbering and starts the chosen
  item.

#### Protocol and clients

- The first client message is a valid complete snapshot.
- Concurrent requests receive their own correlated terminal response.
- Typed protocol errors, request timeout behavior, disconnect reporting, and
  target revision gaps are handled exactly as specified.
- A client using the configured self-signed trust certificate connects by TLS;
  a client without appropriate trust fails clearly.
- The shared Rust library and `targets` perform the same operation through the
  same public protocol.
- `all` is expanded by `targets`, never implemented as a hidden daemon bulk
  operation.

#### Operator client

- Every command in section 9 prints a concise receipt or clear failure.
- Human status follows the compact fixed-column form; `--json` is a full
  machine-readable snapshot.
- Status reports actual loop/repeat state and does not fabricate shuffle mode.
- `targets` accesses remote nodes only through its separate node book, explicit
  `@node` selectors, or direct `--url`.

### 11.2 Construction order

The new repository begins with this specification and its tests. No source is
copied from the abandoned worktrees.

1. Create the package layout, protocol types, protocol serialization tests, and
   `mpv_targets` public API.
2. Implement daemon configuration, atomic persistent mutations, and answer-file
   parsing/materialization boundaries.
3. Implement native mpv IPC framing, property observation, and command
   correlation.
4. Implement the target supervisor: socket attachment, launch, intentional
   stop, recovery, duplicate prevention, and rename.
5. Implement snapshot state and events.
6. Implement TLS WebSocket transport and the concurrent shared client.
7. Implement `targets` solely through the shared library.
8. Implement deployment profiles and the non-interactive answer-file deployer.
9. Write the final README, configuration reference, deployment guide, and
   protocol/library documentation from this specification.
10. Prove the acceptance matrix on a real Linux target host before replacing
    any existing deployed service.

At every step, the implementation stays one daemon, one public library, one
thin operator client, and one deployment layer. New components, persistence
models, and client-specific service behavior require an explicit specification
amendment.
