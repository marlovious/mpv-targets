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

## Current unresolved product direction: regular and channeled mpv

This discussion is conceptual only. No type/mode field or behavior has been
approved or implemented yet.

The channel metaphor exposed a real split between two usage lines:

1. **Regular mpv / playback use**
   - starts as ordinary idle mpv;
   - a client owns the live session;
   - clients load files and manipulate the live playlist;
   - persistent channel assignment is irrelevant or unwanted.

2. **Channeled mpv / signage use**
   - has a persistent M3U channel;
   - starting loads that channel;
   - changing the channel should likely apply immediately;
   - it behaves like a persistent signage appliance.

The promising idea is one small source-management condition, not two daemon
implementations and not a deep type system. Supervision, lifecycle, protocol,
and state remain shared. The condition would only clarify source/startup
behavior.

Why this matters: normal channel switching currently exposes too much of the
lifecycle assembly. For a disabled target, an operator must currently do:

```text
targets set-channel @node/target 3
targets enable @node/target --start
```

`set-channel --start` is not implemented. `set-channel` persists only;
`--restart` applies it to an already-running target. This is technically clear
but feels awkward for signage. A usage posture may collapse that complexity
into a defined assumption rather than adding more flags.

The concise concept agreed in conversation was:

- channel-managed: persistent channel; start loads it; changing it applies
  immediately;
- session-managed: starts idle; clients own the live playlist; no persistent
  channel is expected.

Questions still to decide before touching the spec or code:

- Is this distinction actually encoded, or only a client/deployment default?
- What is the smallest honest name for it?
- Does a channel change ensure a target is running, or merely switch an
  already-running target?
- Does signage posture imply enabled/autostart, or must administrative
  `disabled` always remain an absolute override?
- Which exact moments differ: creation, daemon startup, explicit start,
  channel change, restart, and status?

Do not implement a target type until these behavioral differences are written
plainly and shown to justify the extra configuration branch.

## Current `targets` command surface

The implemented operator commands are:

```text
status [target] [--json]
add <target> [--from TARGET] [--channel VALUE] [--enable] [--start]
remove <target> [--yes]
start|stop <target>
restart <target|all>
enable <target> [--start]
disable <target> [--stop]
rename <target> <new-target> [--yes]

show-channels [@node]
set-channel <target> <number|name|path-or-url> [--restart]
clear-channel <target> [--restart]
playlist <target> [item]

play|pause|toggle-play <target|all>
loadfile <target> <path-or-url>
next|previous <target|all>
mute|unmute|toggle-mute <target|all>
fullscreen|loop|repeat|shuffle|unshuffle <target|all>
cycle-audio|cycle-subtitle|toggle-subtitle <target>
identify [@node]
mpv <target> <allowed-native-command> [args...]
```

`toggle-subtitle` replaced `disable-subtitle` because lifecycle already uses
the word disable. It genuinely cycles mpv `sub-visibility` and was verified on
Bipper as `true -> false -> true`. Keep `cycle-audio` and `cycle-subtitle`:
those advance among potentially more than two tracks and are not honest
toggles.

Remote selectors are explicit:

```text
@dr-fez             whole node where supported
@dr-fez/cameras     one remote target
@dr-fez/all         remote bulk selector
```

`targets play all @dr-fez` is invalid. The correct form is:

```text
targets play @dr-fez/all
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

## Bulk-operation discussion

The current CLI permits `all` for playback-wide commands documented in
`SPEC.md`, including next/previous, loop/repeat, and shuffle/unshuffle.

During remote testing, the user questioned whether bulk next, previous, loop,
repeat, shuffle, or unshuffle have a real operator use. The likely useful bulk
surface is only:

- play, pause, toggle-play;
- mute, unmute, toggle-mute;
- fullscreen.

This narrowing was discussed but not approved as a specification amendment or
implemented. Do not silently remove commands. Revisit it explicitly.

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

### Confirmed defect: loop/repeat incremental event type

On a second bulk loop or repeat toggle, the client can disconnect with:

```text
invalid server message: invalid value for target change field `loop_file`
invalid server message: invalid value for target change field `loop_playlist`
```

The root cause was inspected but not fixed. In
`crates/mpv-targetsd/src/supervisor.rs`, `update_observed` converts raw mpv loop values
to canonical `"on"`/`"off"` strings for stored state but emits the original raw
value in `TargetChanged`. The public client correctly expects the protocol's
string form. Fix the daemon to broadcast the same canonical value it stores,
and add regression tests for both fields and both directions. This matters to
single-target clients too, even if bulk loop/repeat is removed.

### Bulk failure behavior needs a ruling

`next @dr-fez/all` successfully advanced `cameras`, then stopped when another
target's mpv correctly rejected an impossible next operation. The remaining
targets were not attempted. Decide whether bulk operator commands are
intentionally fail-fast or should continue and report per-target failures.
Do not add aggregation machinery if the questionable bulk commands are removed
instead.

### JSON target filtering inconsistency

Human `status @dr-fez/cameras` prints only cameras. The JSON form
`status @dr-fez/cameras --json` currently prints the complete node snapshot.
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

`targets add --from` copies only trusted target-local `mpv.conf` and scripts.
It does not copy lifecycle state, selected channel, or channel files. Arbitrary
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
