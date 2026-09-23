# Hardware test bug ledger

Confirmed during shell-level testing on the Bipper. Keep entries here until
the behavior is fixed and retested.

- [fixed] Intentional `stop` left stale observed playback fields in status.
  The daemon now clears them; live stop/disable and restart regression checks
  pass.
- [fixed] `targets disable <target>` now always disables and stops the target,
  clearing its observed playback fields. The redundant `--stop` form was
  removed.
- [investigate: CLI contract] `targets status <target> --json` currently emits
  the full node snapshot even when a target was selected. Decide whether to
  return the same snapshot envelope filtered to that target, while preserving
  a meaningful health count.
- [investigate: CLI contract] `targets set-channel <disabled-target> ...
  --restart` currently persists the channel and quietly leaves the target
  stopped. It should reject the restart request before mutating the channel.
- [fixed] `targets enable <target> --start` now enables and starts the target.
- [closed: not reproduced] One early `remove --yes` run printed a
  connection-refused diagnostic before its successful removal receipt. Later
  tests, including five consecutive add/remove cycles on the live Bipper,
  completed cleanly and returned to four targets/four mpv processes.
- [expected media boundary] `previous all` returned one mpv rejection after
  `next all` because `movies` was at item 1; per-target previous succeeded for
  the other targets.
- [closed: expected channel loading] Direct Premiumize `toons.m3u8` briefly
  reported the playlist file itself after restart; once loaded, it exposed all
  797 relative entries correctly.
- [fixed] Long `targets playlist <target>` output now treats a closed pipe as
  a clean exit instead of panicking.
- [closed: not reproduced] In the first torture cycle, camera `previous`
  rejected after `next` with a 2-second wait. It passed the next three cycles
  and five later timed cycles with 4-second settling intervals; command
  receipts in the timed run took 0.11–0.20 seconds. Keep treating the original
  rejection as stream/mpv timing unless it recurs.
- [fixed] After an unexpected mpv exit, a target now reports `offline` with
  cleared observed playback fields until recovery. A hard kill of the cloud
  `movies` mpv produced `offline / - / -` immediately, recovered in 2 seconds,
  and returned to four mpv processes with no duplicate.
- [closed: expected systemd consequence] `KillMode=process` deliberately leaves
  healthy mpv children alive across daemon restart, as required by SPEC. That
  makes systemd log “left-over process” warnings when the replacement daemon
  reattaches. Repeated live checks remained at four targets and four mpv
  processes with no duplicate; changing this would change the lifecycle
  contract rather than fix a failure.
- [environment] The Bipper logs PipeWire `pw.conf: can't load config
  client.conf` when mpv starts. Playback still works; this is recorded as a
  host/environment warning, not yet attributed to mpv-targets.
- [investigate: resource] With the 797-entry Premiumize playlist loaded,
  current mpv RSS was roughly 584 MiB for `movies`, 349 MiB for `horror`, 271
  MiB for `cameras`, and 200 MiB for `music`; the daemon itself was about 8
  MiB. The service journal also reports a 6.3 GiB cgroup memory peak during
  restart/load. Playback and reattachment remain correct; attribution needs a
  separate resource investigation.
- [fixed] Successful native mpv commands that return `null` now print concise
  target-correlated CLI receipts. Live Bipper checks passed for single-target
  play/pause, `all`, playlist jump, identify, and the raw native escape hatch.
- [fixed] Human status now includes the required `LOOP` and `REPEAT` columns.
  The daemon normalizes mpv's native `false`/count/`inf` values to observable
  `off`/`on` state, and title truncation keeps rows within the fixed separator.
  Live loop and repeat toggles each passed on then off.
- [fixed] `loop` and `repeat` now set explicit mpv `no`/`inf` values instead of
  incrementing mpv's numeric loop count with `cycle`.
- [fixed] `toggle-play all` makes one node-wide decision from the cached
  snapshot. Mixed playback became all paused and all paused became all playing
  in live Bipper checks. The later command-surface cleanup removed
  `toggle-mute all`; node-wide audio exposes only the deterministic `mute all`.
