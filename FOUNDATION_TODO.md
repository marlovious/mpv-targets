# mpv-targets foundation hardening

Status: current working backlog after building the first `Targets::Studio`
client. This file records agreed follow-up work; it does not amend `SPEC.md` or
authorize a protocol change by itself.

The Studio exercise validated the product boundary: nearly all substantive
client behavior was already supported through the public `mpv_targets` library
and protocol. The work below is ordinary correctness, deployment, and
client-library hardening—not a CMS-sized daemon expansion.

## 1. Correctness

### Serialize configuration mutations

`target_add`, `target_remove`, and `target_rename` currently release the
supervisor lock while preparing filesystem/configuration changes. Concurrent
topology requests can derive candidates from the same old configuration and
commit conflicting results.

- Add one narrow serialization gate for persistent topology/configuration
  mutations.
- Preserve the existing validation, filesystem rollback, and replacement
  snapshot behavior.
- Add concurrent add/add, add/remove, and rename/collision tests.
- Do not add a general transaction framework.

### Clear observed mpv state correctly

Observed property updates currently ignore `null`, negative playlist
positions, and nonnumeric audio/subtitle values, allowing stale state to remain
authoritative in client snapshots.

- Treat mpv `null` as clearing the corresponding optional field.
- Represent valid mpv track values such as numeric IDs, `no`, and `auto`
  faithfully.
- Clear playlist position when mpv reports no current item.
- Test file transitions, idle state, disabled subtitles, and source removal.

### Recover after an explicit launch failure

An explicit `start` marks a target expected-online before launch. If mpv never
creates its IPC socket, the command fails but no recovery task is scheduled.

- Preserve the correlated command failure.
- Clean up the failed child as today.
- Enter the same recovery path used by an enabled target lost unexpectedly.
- Test that no duplicate mpv process can result.

### Decide recovery behavior after the current retry limit

The current recovery task stops after five attempts and leaves the target
expected-online but permanently offline until another operator action.

- Decide between finite attempts and indefinite retry with a capped interval.
- Prefer the smallest behavior consistent with the specification's “bounded
  backoff” language.
- Record the decision in `SPEC.md` if externally observable semantics change.

## 2. Filesystem and deployment

### Put logs in the specified XDG state directory

The specification declares:

```text
~/.local/state/mpv-targets/logs/
```

The daemon and deployer currently use `~/.config/mpv-targets/logs/`.

- Derive the state root through XDG rules with the documented fallback.
- Update daemon launch logging, deployer creation/receipts, and tests together.
- Do not delete or migrate existing logs automatically without a separate
  explicit decision.

### Make deployment reproduce the working Fez boot setup

The installed Fez unit works because it was manually given a user-session/
Hyprland readiness gate. The generic deployer would overwrite it with an early
`default.target` unit that cannot reliably launch visible mpv targets.

- Implement the deployment-profile boundary already specified in `SPEC.md`.
- Keep the daemon generic; session readiness belongs to profile assets and
  service integration.
- Make a clean redeployment reproduce the working boot behavior without manual
  edits.
- Test an actual reboot and confirm target attachment without duplicate mpv
  processes.

### Finish the profile contract

`host_profile` is accepted by the answer-file model, but deployment currently
rejects every value except `generic`.

- Define the first real profile needed by the hardware deployment.
- Keep generated daemon configuration and public protocol identical across
  profiles.
- Document exactly which packages, session integration, and `mpv.conf`
  defaults the profile owns.

## 3. Public client-library foundation

These additions should preserve the existing raw protocol surface while
removing repeated parsing and state interpretation from `targets`,
`Targets::Studio`, PaneBot, and later clients.

### Typed successful results

Add typed conveniences for stable response shapes, including:

- channel catalog entries;
- playlist items;
- lifecycle results;
- add/remove/rename results;
- channel mutation results.

Keep a low-level raw request/result escape hatch. Do not add new wire
operations merely to type existing response data.

### Authoritative snapshot watch

The library already applies incremental revisions to its internal snapshot.
Expose a watch-style stream of the resulting authoritative snapshots so a GUI
does not have to receive a raw event and immediately clone the cache itself.

- Retain raw event subscription for activity/history presentation.
- A slow UI should receive the latest snapshot rather than fail because it
  missed intermediate presentation updates.
- Revision gaps must still terminate the connection and require a fresh
  snapshot.

### Awaitable disconnect notification

Expose an awaitable/watchable disconnect result so long-running clients do not
poll `disconnected()` and `disconnect_error()`.

- Preserve explicit `close()` behavior.
- Distinguish an intentional close from a transport/protocol failure.

### Connection and request timing

- Replace initial-snapshot polling with direct notification.
- Put an explicit deadline around the complete TCP/TLS/WebSocket/initial-
  snapshot connection sequence.
- Allow clients to configure connection and request timeouts while retaining
  conservative defaults.

### Shared derived target status

Provide one small helper that derives the common client state precedence:

```text
disabled -> stopped -> offline -> idle -> paused -> playing
```

This remains a view of authoritative fields, not another daemon lifecycle
state.

### Shared bulk-selection helpers

Keep all bulk execution client-side as required by `SPEC.md`, but consolidate
the pure selection/decision rules that otherwise drift between `targets`,
Studio, and PaneBot.

- No server-side `all` operation.
- No rollback or transaction layer.
- Preserve written-order execution and first-error behavior where applicable.

## 4. Deliberate protocol discussion

### Media end/error observation

A client currently cannot reliably distinguish ordinary end-of-file, a dead
stream, an mpv media error, an operator stop, or source replacement while mpv
itself remains online.

mpv emits `end-file` reasons that may provide the narrow observation a signage
client needs.

Before implementation:

- decide whether this belongs in the v1 observable state/event contract;
- define the smallest stable representation;
- keep command acceptance separate from later media outcome;
- do not expose remote logs or add a generic monitoring system as a substitute.

This item requires an explicit `SPEC.md` amendment before code changes.

## 5. Verification order

1. Add failing focused tests for concurrency, null state, track values, and
   explicit launch failure.
2. Fix those correctness defects without changing public vocabulary.
3. Correct XDG log ownership and deployment/profile behavior.
4. Add the client-library conveniences without removing the raw API.
5. Update `targets` and `Targets::Studio` to consume the shared helpers.
6. Run the full Rust test and lint suite.
7. Exercise both clients against the real Fez node.
8. Reboot Fez and verify service/session startup, reattachment, recovery, and
   no duplicate target processes.
9. Discuss and separately approve media end/error observation.

## 6. Explicit non-goals for this pass

- No CMS, scheduler, media library, downloader, NVR, or content discovery in
  the daemon.
- No server-side bulk operations.
- No remote filesystem editor.
- No authentication or PKI expansion.
- No generic monitoring or rollback framework.
- No Studio visual redesign in this repository.
