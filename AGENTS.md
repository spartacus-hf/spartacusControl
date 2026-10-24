# Driving the radio with `sctrl` (for agents and scripts)

`sctrl <command>` does one thing and exits. Add `--json` to any command for one JSON object
per line on stdout. For working on S-CTRL's own code, see
[CONTRIBUTING.md](CONTRIBUTING.md).

## How a command reaches the radio

- With the S-CTRL window (or `sctrl daemon`) running, a command goes through its local
  socket (`$XDG_RUNTIME_DIR/sctrl/sctrl.sock`) and shares its connection. The PC speaker
  and `watch` need this.
- Without one, the command opens the radio's LINK port itself and closes it when done.
- The running S-CTRL and `sctrl` must be the same build: after a rebuild, restart the
  window, or the command is refused with "another build ... restart it". A window started
  from the menu is the installed `~/.local/bin/sctrl` (`scripts/install.sh`): use that one.
- `--serial <SERIAL>` picks one of several radios; `--timeout <s>` (default 10).
- `--port <tty>` names the radio's LINK port instead of finding the radio on USB: a
  pseudo-terminal (an emulated radio's, socat) or a SPARTACUS's own `if05` port.
  A command with `--port` goes only to an S-CTRL started with the same `--port` (it serves
  its own socket), never to the one holding a radio on USB, and the reverse. A
  pseudo-terminal's radio has no sound card: the PC speaker and `audio` leave it alone.
  Without `--port` nothing changes.

## Results

| Exit | `--json` `error` | Meaning |
|---|---|---|
| 0 | (`"ok": true`) | done |
| 2 | (usage text) | bad arguments |
| 3 | `not_connected` | no radio, no LINK, or the window/daemon is needed |
| 4 | `refused` | a safety rule said no; the message names the rule and why |
| 5 | `timeout` | the radio did not answer in time |
| 6 | `error` | the radio or the PC reported an error |

On failure `--json` prints `{"ok": false, "error": <kind>, "message": <text>}`.

## Recipes

```sh
sctrl ls                           # radios and related USB devices, ports, radio programs running
sctrl status --json                # dial_hz, mode, page, state, faults, vin, armed, CAT program ...
sctrl diagnose                     # radio + host checks, problems first
sctrl shot -o screen.png           # the radio's screen as a PNG
sctrl usb audio                    # radio in DIAG (console) -> sound card + CAT + LINK ports

sctrl tune 14.074M                 # also 14074000 or 7074k
sctrl mode cw                      # usb lsb cw am fm digi digi-lsb
sctrl goto waterfall               # home freq diag tx-test trainer sd pd waterfall
sctrl knob 3                       # detents; negative turns counter-clockwise
sctrl press sel                    # click hold up dn sel func
sctrl get <setting>                # by id or name
sctrl set <setting> <value>
sctrl cat 'FA;'                    # one TS-480/QMX CAT command over the LINK
sctrl console status               # one console command, its reply

sctrl watch status --count 5 --json  # STATUS as it changes
sctrl watch events                   # pipeline events
sctrl watch cw                       # the CW decoder's text

sctrl sd ls LOGS                   # the SD card, read only (about 2.7 KB/s)
sctrl sd get README.TXT -o readme.txt

sctrl audio status                 # radio audio devices, what plays into the radio, PC speaker
sctrl audio pc on --volume 0.8     # PC speaker; --digi/--no-digi, --output <sink|default>
sctrl audio pc off                 # (each choice is kept for the next start)
```

## Rules S-CTRL enforces (and agents should expect)

- It never opens the radio's CAT port (WSJT-X's). While a CAT program is active there,
  frequency, mode and band are locked unless `--take-control` is given, and never while it
  transmits.
- Nothing changes frequency, mode, band, audio paths, storage or the USB personality while
  the radio transmits, and nothing keys during a mode or band change.
- The console goes silent while the radio is realtime-critical (keyed CW, TX streams).
  That is expected, not a hang.

## TX

TX is off until armed, and every key-down passes a checklist first. Keep key-downs short
(about a second) and say truthfully what the radio transmits into.

```sh
sctrl tx check key --load dummy    # the checklist only; nothing is keyed
sctrl tx arm --load dummy          # ARM; the radio disarms itself after the TTL without TX
sctrl tx key --ms 500              # DIT held 500 ms (--dah, --both); on TX TEST the carrier
sctrl tx tune --ms 800             # TUNE carrier on the TX TEST page
sctrl tx mem 'CQ TEST'             # a CW memory through the keyer, at most 48 characters
sctrl tx stop                      # ends everything S-CTRL keyed and disarms; never refused
sctrl tx policy                    # limits; e.g. --bench on caps every key-down at 1 s
sctrl tx log                       # the audit log (JSON lines)
```

The checklist covers the LINK, firmware, safety rules, radio state, faults, supply voltage,
band plan (IARU region), mode, other TX owners, SWR, TX MUTE, the declared load, duty cycle
and ARM. A failed item refuses with exit 4 and says how to fix it. The radio keeps its own
guards behind these (SWR and supply trips, its CW power-up arm).
