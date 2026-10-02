# ensemble-reaper

REAPER OSC bridge for [Ensemble](https://github.com/samgwise/ensemble) —
exposes REAPER's transport, tracks and FX parameters as Ensemble actions, and
forwards Ensemble actions to REAPER as OSC commands.

```text
   Ensemble hub                  ensemble-reaper                      REAPER
┌─────────────────┐   TCP    ┌──────────────────────┐   UDP    ┌──────────────────┐
│ voices, clock,  │<────────>│ /reaper/** OSC       │<────────>│ OSC device       │
│ routing, replay │          │ translation+registry │          │ (pattern config) │
└─────────────────┘          └──────────────────────┘          └──────────────────┘
```

## Building

```bash
cargo build --release
```

Released builds resolve the Ensemble crates from crates.io. For local
development, check out the
[`ensemble`](https://github.com/samgwise/ensemble) workspace as a sibling
directory (`../ensemble`) — the path dependencies pick it up automatically.

## Running

```bash
# Defaults: hub discovery, commands to REAPER on UDP 8000,
# feedback listener on UDP 9000.
ensemble-reaper

# Custom configuration.
ensemble-reaper --name reaper --hub 7331 --reaper-host 127.0.0.1 \
    --osc-send-port 8000 --osc-listen-port 9000
```

| Option | Default | Description |
|--------|---------|-------------|
| `--name` | `reaper-bridge` | Voice name shown in the hub |
| `--reaper-host` | `127.0.0.1` | Host running REAPER |
| `--osc-send-port` | `8000` | Port to send commands to (REAPER's OSC listen port) |
| `--osc-listen-port` | `9000` | Local UDP port for REAPER feedback |
| `--hub` | (discovery) | Explicit hub port, bypassing discovery |

## REAPER setup

1. Copy `reaper-osc/Ensemble.ReaperOSC` into `%APPDATA%\REAPER\OSC\`
   (use the "open config directory" button in the OSC device dialog).
2. In REAPER: **Options → Preferences → Control/OSC/web → Add**,
   *Control surface mode* = **OSC (Open Sound Control)**.
3. Set *Pattern config* to `Ensemble.ReaperOSC`.
4. Set the device's **local listen port** to the bridge's send port (8000 by
   default) and point the **target** at the bridge's listen port (9000 by
   default).

The template uses track-scoped patterns so every track reports (not just the
device-selected one), and omits flood-prone feedback (VU, samples, frames).
Bank sizes are at the top of the file if you need more than 64 tracks, 16 FX
slots or 64 parameters per FX.

## Address schema

State values are Ensemble **Params** (retained by the hub and replayed to
late subscribers), the beat position is a **Stream** (best-effort), and
transport/action triggers are **Events**.

| Address | Signal | Maps to (REAPER) |
|---------|--------|------------------|
| `/reaper/transport/play\|stop\|pause\|rewind\|record` | Event | `t/play`, `t/stop`, … |
| `/reaper/transport/playing` | Param (bool) | `t/play` feedback |
| `/reaper/transport/recording` | Param (bool) | `t/record` feedback |
| `/reaper/transport/bpm` | Param (float) | `f/tempo/raw` |
| `/reaper/transport/position/seconds` | Param (float) | `TIME f/position/seconds` |
| `/reaper/transport/position/beat` | Stream (float) | `BEAT f/position/beat` |
| `/reaper/master/volume` | Param (float 0–1) | `n/master/volume` |
| `/reaper/track/{n}/volume` | Param (float 0–1) | `n/track/@/volume` |
| `/reaper/track/{n}/pan` | Param (float 0–1, 0.5 centred) | `n/track/@/pan` |
| `/reaper/track/{n}/mute` / `/solo` / `/recarm` | Param (bool) | `b/track/@/…` |
| `/reaper/track/{n}/name` | Param (string) | `s/track/@/name` |
| `/reaper/track/{t}/fx/{f}/param/{p}/value` | Param (float 0–1) | `n/track/@/fx/@/fxparam/@/value` |
| `/reaper/track/{t}/fx/{f}/info` | Param (map) | synthesised from `FX_NAME`/`FX_PARAM_NAME` |
| `/reaper/action/{id}` | Event | `i/action` / `t/action/@` |

FX parameter values are normalised 0–1 (REAPER's OSC representation);
track, FX and parameter indices are passed through to REAPER verbatim.

## Plugin registry

REAPER's OSC interface has no FX add/remove events, so the bridge maintains a
registry by diffing `FX_NAME`/`FX_PARAM_NAME` feedback:

- a name arriving at a new slot publishes `/reaper/track/{t}/fx/{f}/info`
  (a Map Param: plugin name, observed parameter names);
- a changed name republishes the slot;
- an emptied slot unsets its `info` and parameter value Params.

Because registry entries are Params, any voice joining the hub later receives
the full FX topology via replay — no REAPER connection needed to discover it.
Note that `param_count` in `info` is the highest observed parameter index + 1;
REAPER's OSC interface does not expose a plugin's true parameter count.

## Limitations

- No MIDI into instrument plugins — use `ensemble-bridge-midi`.
- No FX chain manipulation (add/remove/reorder plugins) — ReaScript territory.
- No installed-plugin inventory — only loaded FX are reported.
- Track/FX indices shift when the chain changes; the registry republishes on
  name changes, but consumers addressing slots by index should re-read `info`
  after chain edits.
- OSC bundles are ignored (as in the generic `ensemble-bridge-osc`).
