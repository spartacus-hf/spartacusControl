# Contributing to S-CTRL

These rules apply to human and automated contributors. Scripts and agents that only drive
the radio start from [AGENTS.md](AGENTS.md).

## Changes

- Run `scripts/check.sh` before every commit: `cargo fmt --check`, `clippy -D warnings`
  and all tests. Do not pipe clippy into `head`: that hides its exit status.
- One understandable purpose per commit. The tree builds and the tests pass at every
  commit.
- A behaviour change comes with a test that fails without it. The safety arbiter
  (`crates/core/src/safety.rs`) and the TX checklist (`crates/core/src/tx.rs`) are pure
  functions so that every rule has one.
- Contributions are licensed under the GNU GPL v3.0 or later, as the rest of S-CTRL
  (`LICENSE`).

## The protocol

- `docs/link-protocol.md` is the LINK as the radio's firmware implements it, and the
  firmware keeps an identical copy. `tests/fixtures/link_vectors.txt` comes from the
  firmware's host test suite. Change either only together with the firmware.
- `crates/proto` does no I/O: codecs, the frame model and the screen and spectrum models,
  tested against those vectors.

## Radio safety

S-CTRL keys a transmitter. On the bench:

- The radio's RF output goes into a **50 Ω dummy load**.
- Key-downs from scripts and tests: **at most 1 s each** (`sctrl tx policy --bench on`),
  never above 5 s.
- Never provoke risky cases on real hardware: unplugging during TX, a supply drop,
  flashing during TX. Those belong in the firmware's host test suites.
- S-CTRL never opens the radio's CAT port (`...-if03`, WSJT-X's). Its LINK port is
  `...-if05`.
- The console goes silent while the radio is realtime-critical (keyed CW, TX streams).
  That is expected, not a hang.

## Source comments

Comments explain a contract that is not obvious from the code: units and ownership,
invariants, the radio's behaviour, safety constraints, the reason for a non-obvious choice.
Keep them concise and in the present tense. Derivations, measurements and defect histories
belong in the commit message. Never commit session notes, temporary tracing or
commented-out experiments.
