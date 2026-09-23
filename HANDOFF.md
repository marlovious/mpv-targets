# mpv-targets handoff

This document is the durable context for continuing the current mpv-targets
work from another machine. Read `AGENTS.md` and all of `SPEC.md` before making
any implementation or design decision. `SPEC.md` is the canonical implemented
contract; this file records product intent, operational state, test evidence,
and discussions that have not yet become contract.

## Product thesis

mpv-targets is named-target supervision and remote control for mpv.

**Just the streams.**

It should feel like a small Unix utility, not a platform. mpv remains the media
executor. mpv-targets names mpv instances, supervises their processes, exposes
their observable state, and gives thin clients one stable way to control them.

The project explicitly does not own:

- media libraries, discovery, indexing, metadata, or search;
- downloading, accounts, NVR behavior, or content management;
- playlist editing or curation;
- geometry, desktop layout, dashboard UI, or window management;
- generic host provisioning inside the daemon;
- arbitrary remote file editing or arbitrary `mpv.conf` injection;
- authentication or an elaborate PKI policy.

PaneBot, the `targets` operator CLI, and future tablet/desktop clients must all
use the same public `mpv_targets` library and the same protocol. There are no
private PaneBot endpoints or filesystem shortcuts.

## Repository and publication state

The canonical repository is:

```text
https://github.com/marlovious/mpv-targets
~/gitz/mpv-targets
```

The GitHub repository was deliberately deleted and recreated so the abandoned
implementation history could not contaminate this greenfield rewrite. The
clean history began with:

```text
9604342 Establish canonical v1 specification
ccd0e9b Require first host install through deployer
2ac11f1 Implement canonical mpv-targets v1
cc26cc5 Rename subtitle visibility command
```

The old local `mpv-targets` and linked `mpv-targets-v2` worktrees were deleted.
Do not recover or merge their structure. Old implementations are behavioral
evidence only.

Hardware provisioning is a separate project:

```text
~/gitz/mpv-targets.node
```

Hardware profiles, Hyprland deployment, host packages, mounts, and stationary
node preparation belong there or in another deployment layer. The daemon and
protocol remain generic.

## Current implementation

This is a Rust workspace containing:

- `crates/mpv-targets`: public protocol, configuration, TLS, answer-file, and
  typed async client library;
- `crates/mpv-targetsd`: foreground daemon, target supervisor, mpv IPC, state
  observation, recovery, and WSS service;
- `crates/targets`: thin non-interactive operator client built entirely on the
  public library;
- `crates/deploy-targets`: non-interactive Linux deployment tool and optional
  systemd user-service installer.

The daemon is not systemd-specific. On Linux, deployment may install it as a
systemd user service. On macOS, PaneBot will embed and start/stop the daemon
with the application. A future optional macOS service installer is outside the
current scope, but the runtime must remain portable.

### Lifecycle

The implemented lifecycle deliberately distinguishes:

- `disabled`: persistent configuration; do not run after daemon restart;
- `stopped`: session-only suppression of recovery;
- `online`: a supervised mpv process with IPC available.

Important behavior:

- enabled configured targets attach to an existing socket or launch once;
- daemon restart reattaches and does not duplicate mpv;
- unexpected loss recovers enabled, non-stopped targets;
- disabled targets stay off;
- rename requires an already-stopped target and never auto-stops/restarts;
- add defaults to disabled and stopped;
- `--start` on add implies enable;
- removing a target removes its target directory and configuration entry;
- every mutation returns one correlated terminal result;
- later media/stream failure is observable state, not a retroactive command
  failure.

Do not add process orchestration layers, rollback state machines, broad
monitoring, or speculative cleanup logic. The current simple supervision was
hardware-tested.

### TLS

TLS exists because browser clients require WSS. It is intentionally minimal:

- the daemon serves WSS using generated or supplied certificate files;
- native clients accept the certificate without validation, like PaneBot;
- browsers handle their own trust requirements;
- proper DNS certificates remain possible by supplying certificate files;
- there is no native system-root policy, pinning, hostname policy, or
  trust-certificate option.

Do not rebuild the removed TLS policy machinery.

## Channels and live playlists

This distinction is central.

### Implemented channel model

A channel is an M3U selected as a target's persistent source. Channels are
node-level, not duplicated per target.

```text
~/.config/mpv-targets/
├── channels/
├── mpv-targets.toml
├── targets/
│   └── <target>/
│       ├── mpv.conf
│       └── scripts/
└── tls/
```

The shared catalog contains direct `.m3u` and `.m3u8` files. Any target can
select any catalog entry. A target stores only an optional absolute channel
path or URL. External absolute M3U paths and URLs are allowed but are not
copied or imported. A target with no channel still launches mpv idle.

The implemented operations are:

```text
targets show-channels [@node]
targets set-channel <target> <number|name|path-or-url> [--restart]
targets clear-channel <target> [--restart]
targets add <target> [--from TARGET] [--channel VALUE] [--enable] [--start]
```

`node_list_channels` is the only targetless protocol operation.
`RemoteClient::list_channels()` and `TargetClient::set_channel()` expose the
same behavior to all clients.

`show-channels` clears an interactive terminal, then renders:

```text
:: [Channels] ::

1. cameras.m3u8
2. movies.m3u8
```

A successful set receipt includes the selected filename:

```text
"MOVIES"         :: [movies.m3u8] :: [SET]
```

### Live mpv playlist

The currently loaded mpv playlist remains a playlist. It is not a channel
catalog and is not persisted or managed by the service.

```text
targets playlist <target>
targets playlist <target> <one-based-item-number>
```

This displays or jumps within mpv's current playlist. The display prefers mpv
titles, falls back to a local basename, preserves URLs, truncates long lines,
and treats a closed pipe as a clean exit.

Do not add a media browser, recursive scan, metadata reader, playlist editor,
or playlist publisher to mpv-targets. Other Marlovious Dashboard components
may eventually publish M3Us into a channel directory; mpv-targets merely
consumes them.

### Shared cloud channels

The intended multi-node pattern uses a shared cloud/FUSE root. Reusable M3Us
contain paths relative to that cloud root so the same channel works on every
host. Do not mix host-local absolute paths into globally shared M3Us. Each
node only needs the cloud mount available at its configured catalog location.

## Regular mpv and persistent channels

There is no target type or mode. A target with a selected channel loads that
M3U when it starts; a target without one starts as ordinary idle mpv. Clients
may still load and manipulate the live playlist in either case. Channel changes
remain explicit persistence operations, and `--restart` is the explicit way to
apply one immediately to a running target.

## Current `targets` command surface

The implemented operator commands are:

```text
status [@node] [target] [--json]
add [@node] <target> [--from TARGET] [--channel VALUE] [--enable] [--start]
remove [@node] <target> [--yes]
start|stop|restart [@node] <target...|all>
enable [@node] <target...> [--start]
disable [@node] <target...>
rename [@node] <target> <new-target> [--yes]

show-channels [@node]
set-channel [@node] <target> <number|name|path-or-url> [--restart]
clear-channel [@node] <target> [--restart]
playlist [@node] <target> [item]

play|pause|toggle-play [@node] <target...|all>
loadfile [@node] <target> <path-or-url>
append [@node] <target> <path-or-url>
next|previous [@node] <target...>
mute [@node] <target...|all>
unmute|toggle-mute [@node] <target...>
loop|repeat|shuffle|unshuffle [@node] <target...>
fullscreen|cycle-audio|cycle-subtitle|toggle-subtitle [@node] <target>
identify [@node]
mpv [@node] <target> <allowed-native-command> [args...]
```

`toggle-subtitle` replaced `disable-subtitle` because lifecycle already uses
the word disable. It genuinely cycles mpv `sub-visibility` and was verified on
Bipper as `true -> false -> true`. Keep `cycle-audio` and `cycle-subtitle`:
those advance among potentially more than two tracks and are not honest
toggles.

Remote selectors are explicit:

```text
@dr-fez             whole node where supported
@dr-fez cameras     one remote target
@dr-fez all         remote bulk selector
```

The node selector immediately precedes target names:

```text
targets play @dr-fez cameras movies
targets pause @dr-fez all
```

The optional client address book is:

```text
~/.config/mpv-targets/nodes.toml
```

with:

```toml
[[nodes]]
id = "dr-fez"
url = "wss://10.11.12.21:9876"
```

A bare target or `all` always means the local node; there is no hidden default
remote node. `--url` is also supported for direct access.

## Multiple-target operations

Applicable commands accept explicit target lists and process them in order,
stopping at the first error without rollback. `all` is intentionally narrower:
start, stop, restart, play, pause, toggle-play, and mute only. Fullscreen,
track operations, loading, channel mutation, playlist inspection, and raw mpv
commands remain single-target.

## Known findings from remote testing

The local installed client was used against Bipper through `@dr-fez`, not over
SSH command substitution. A disposable target was created, exercised, renamed,
and removed. Real media appeared on the hardware.

Verified remotely:

- node, target, and JSON status;
- shared channel listing, selection, clearing, persistence, and restart;
- bare add and add-from with channel;
- enabled/disabled and stopped/online states;
- enable, disable, start, stop, restart, rename, and remove;
- rejection of duplicate add, running rename, and starting disabled target;
- play, pause, toggle-play, mute, unmute, and toggle-mute;
- local M3U `loadfile`, playlist expansion, and numbered playlist jump;
- next, previous, fullscreen, loop, repeat, shuffle, and unshuffle;
- audio/subtitle cycling and subtitle visibility toggling;
- identify and the native mpv escape hatch;
- direct `--url` and address-book node resolution;
- supported bulk operations.

### Fixed defect: loop/repeat incremental event type

On a second bulk loop or repeat toggle, the client can disconnect with:

```text
invalid server message: invalid value for target change field `loop_file`
invalid server message: invalid value for target change field `loop_playlist`
```

The daemon now normalizes loop values before both storing and broadcasting
them, so `TargetChanged` carries the same canonical `"on"`/`"off"` strings as
snapshots. Regression coverage includes both loop fields and directions.

Multiple-target commands intentionally fail fast. Each completed target has
already printed its correlated receipt; the first failure is reported and
later targets are not attempted. There is no rollback or aggregation layer.

### JSON target filtering inconsistency

Human `status @dr-fez cameras` prints only cameras. The JSON form
`status @dr-fez cameras --json` currently prints the complete node snapshot.
This is an observed consistency issue, not yet fixed. A change must decide
whether JSON target status should be a filtered snapshot or a target record;
do not casually invent a second JSON shape.

## Bipper state

The hardware test node is:

```text
node id: dr-fez
address: 10.11.12.21:9876
service: mpv-targets.service (systemd user service)
```

Configured targets after cleanup:

- `cameras`: enabled;
- `horror`: enabled;
- `movies`: enabled;
- `music`: enabled;
- `gopro`: disabled, stopped, no channel.

The four enabled targets were left online, muted, and with loop/repeat off.
The disposable `remote-smoke`, `remote-renamed`, and `remote-bare` targets were
removed. No legacy per-target `playlists/` directories remain.

The shared catalog contained:

```text
cams.m3u8
outdoor.m3u
supercams.m3u8
test-cloud-films.m3u8
test-local-audio.m3u8
test-local-video.m3u8
video.films.m3u8
```

The latest `targets` binary was installed both locally in `~/.cargo/bin` and on
Bipper in `~/.local/bin`. Bipper's daemon and deployer were previously built
from this canonical codebase. The service was verified to reattach four live
mpv processes without duplication. Expected systemd warnings about leftover
mpv processes occur because `KillMode=process` intentionally leaves them alive
for reattachment.

The old migration rollback under `/tmp/mpv-targets-pre-channels-20260915` was
explicitly deleted after validation.

## Deployment contract

The answer file is deliberately small and richly commented:

- node id and listen address;
- generic or future deployment-owned hardware profile;
- generated self-signed TLS or supplied certificate/key;
- whether to install/start the systemd user service;
- initial targets, disabled state, and generated baseline mpv startup values.

It does not own selected channels, channel files, imported `mpv.conf` files,
remote nodes, credentials, or client UI choices. Deployment creates one empty
shared `channels/` directory and target-local `mpv.conf` plus `scripts/`.

Deployment is non-interactive and non-destructive by default. An existing
configuration causes an error. Redeployment requires `--overwrite` and a real
terminal confirmation; unattended overwrite is intentionally unsupported.

`targets add --from` copies trusted target-local `mpv.conf`, scripts, and the
selected channel reference. It does not copy lifecycle state or channel files.
Arbitrary
remote `mpv.conf` content and remote filesystem path instructions were removed
from the public protocol.

## PaneBot and ecosystem ambition

PaneBot should retain its existing visible behavior while replacing its old
typed command plumbing with the public `mpv_targets` library. Its setup and
target-management screens already exist; they should be cleaned into reusable
UI modules rather than redesigned gratuitously.

PaneBot will:

- include a working mpv-targets distribution in its installation;
- present user-friendly target setup;
- manage desktop geometry, layouts, and pane lifecycle at the product layer;
- start/stop the local daemon with the macOS application by default;
- use the exact same public API as every remote client.

Future dashboard components may publish dead-simple M3U channels for PaneBot
or signage nodes to consume. Examples include a Tidal downloader,
podcast/YouTube injector, or M3U channel builder. Those publishers do not
belong inside mpv-targets.

For Raspberry Pi or stationary signage, host deployment may install the same
Hyprland-based environment through a hardware profile. mpv-targets itself does
not know it is a Pi, NUC, Mac, or desktop.

## Verification baseline

Before the latest handoff, the workspace passed:

```text
cargo fmt --all
git diff --check
cargo test --offline
cargo clippy --offline --all-targets -- -D warnings
```

There were 31 tests after the `toggle-subtitle` change. Hardware tests covered
the command surface much more broadly than unit tests. Keep both levels:
focused unit/contract tests locally and shell-level operation through the real
`targets` client on Bipper.

When testing slow cloud/FUSE M3Us, allow realistic settling time. Do not fire
ten commands in ten seconds at a playlist that needs several seconds to expand.
Malformed, slow, or dead streams are useful dirty-environment evidence, but do
not patch specifically around one hostile test playlist. Separate internet,
stream-provider, mpv, mount, and mpv-targets failures.

## Collaboration discipline

The user cares more about a small correct product than speculative robustness.

- Do not invent features, failure scenarios, policy, or abstraction layers.
- Do not change code when the user says “chat only,” is thinking aloud, or is
  merely asking how something works.
- If a statement is ambiguous, ask what it means instead of interpreting it
  into a design.
- Keep explanations concise unless a detailed analysis is explicitly wanted.
- Distinguish an implemented fact, a confirmed bug, a recommendation, and a
  conceptual idea.
- Do not call targets panes unless discussing PaneBot UI history; the canonical
  service noun is target.
- Do not infer current hardware configuration into the generic contract.
- Avoid checks, gates, retries, state, or cleanup code unless each one has a
  concrete contract-level reason to exist.
- Prefer removing questionable command surface over building infrastructure to
  support implausible use cases.
- Make small changes, test them, show concrete output, and commit/push completed
  work.
- Never claim that a local commit is remotely published. State local commit and
  push status separately.

The tone can be informal, but the implementation must remain ruthless:

**JUST THE STREAMS.**
