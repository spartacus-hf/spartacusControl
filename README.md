# S-CTRL — Spartacus Control

Desktop control for the Chireix SPARTACUS REV1 transceiver (alt2 firmware), on Linux.

- A small floating window (qmmp-style) that finds the radio on USB and shows its live screen.
- Every radio setting, mode and band, and the panel controls (knob, buttons, FUNC).
- **PC speaker mode:** the radio's audio, sidetone and CW trainer on the laptop's output,
  through PipeWire with low latency.
- Every diagnostic the radio has over USB, plus the host side (PipeWire, ports, conflicts).
- Interlocks against unsafe combinations, e.g. WSJT-X on the CAT port, TX during a mode
  change, stray audio into the radio's TX input.
- `sctrl`, a CLI for scripts and agents: one command for what used to take many.

Status: under construction. See `docs/` for the design:
- `docs/link-protocol.md`: the USB LINK protocol shared with the firmware.

## Build

Needs a working Rust toolchain (rustup stable), PipeWire headers, libudev and clang (for
bindgen).

```
cargo build --release
```

## Layout

| Path | What |
|---|---|
| `crates/proto` | LINK codec: SLIP, CRC, frames, screen/spectrum decoding. No I/O. |
| `crates/core` | Engine: discovery, transport, radio model, safety arbiter, PipeWire, watchers, bench. |
| `crates/sctrl` | The `sctrl` binary: GUI (no arguments), `sctrl daemon`, CLI commands. |
| `docs/` | Protocol, architecture, safety rules. |
| `packaging/` | .desktop file, WirePlumber rule, i3 snippet. |
