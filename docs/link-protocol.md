# SCTRL LINK protocol, version 1

The LINK is the framed channel between the SPARTACUS REV1 (alt2 firmware) and S-CTRL.
It runs on the radio's existing composite USB: no new interface, endpoint or packet
memory. The firmware copy of this file is `alt2/docs/SCTRL_LINK.md`; the two are kept
identical. Both sides test against the same vectors (`tests/fixtures/link_vectors.txt`,
exported by the firmware host suite `sctrl_link_logic`).

## 1. Transport

| Personality | Port | Notes |
|---|---|---|
| AUDIO (0483:5740) | CDC port 1, `...-if05` | S-CTRL's port. Port 0 (`if03`) stays with WSJT-X/hamlib CAT and S-CTRL never opens it. |
| DIAG (0483:5742) | the DIAG CDC port, `...-if00` | Same protocol. No audio. TX is muted in DIAG, so every TX request is refused. |

A CAT port becomes the console on the first line ending (`cat.c`). The LINK starts from
that console:

1. The host opens the port and sends `link 1\n`.
2. The radio answers on the text console: `link=1 caps=<hex> crc=<hex>` then `OK`.
   (`ERR ...` or an unknown command: the radio has no LINK, so the host uses basic mode.)
3. Once that reply has left, the port speaks frames only. The first radio frame is `HELLO`.

The LINK ends, and the port is a fresh CAT port again, when any of these happens:
- DTR drops (the host closes the port);
- USB reset, suspend or a personality switch;
- the host sends `LINK_OFF`.

The radio sends `BYE` when it can. A heartbeat timeout does **not** end the LINK. It
releases everything LINK-owned (section 6) and streaming carries on.

## 2. Framing

SLIP byte stuffing (RFC 1055): `END = 0xC0`, `ESC = 0xDB`, `ESC_END = 0xDC`,
`ESC_ESC = 0xDD`. On the wire a frame is `END, stuffed(body), END`. Empty frames
(`END END`) are ignored.

```
body = type:u8 | seq:u8 | payload:n | crc:u16le
crc  = CRC-16/CCITT-FALSE (poly 0x1021, init 0xFFFF, no reflection, xorout 0) over type..payload
```

- `seq` counts frames per direction (mod 256), so a receiver sees what it lost. The radio
  does not resend.
- Body size limits:

  | Direction | Max body |
  |---|---|
  | radio → host | 1024 B |
  | host → radio | 56 B for buffered frames; a `CMD` line up to 120 B (streamed into the console's own line, so it needs no LINK buffer) |

- A frame with a bad CRC, an oversize body or a body shorter than 4 bytes is dropped and
  counted.
- All integers are little-endian. `f32` is IEEE-754 binary32, little-endian.
- Frame types: 0x01-0x3F radio → host, 0x40-0x7F host → radio.
- A receiver ignores unknown types. Payloads may grow in later versions: readers ignore
  trailing bytes they don't know, and writers never reorder fields.

Time-critical host frames (`HEARTBEAT`, `PADDLE`, `INPUT`, `TX_STOP`) fit one 8-byte CDC OUT
packet unless stuffing lengthens them.

## 3. Radio → host frames

### 0x01 HELLO (once, first frame)

| Off | Type | Field |
|---|---|---|
| 0 | u8 | protocol version (1) |
| 1 | u8 | firmware major |
| 2 | u8 | firmware minor |
| 3 | u8 | personality: 0 AUDIO, 1 DIAG |
| 4 | u32 | caps (below) |
| 8 | u32 | image CRC (as `id` reports it) |
| 12 | u16 | framebuffer width (320) |
| 14 | u16 | framebuffer height (240) |
| 16 | u8 | pages (30) |
| 17 | u8 | tile columns (40) |
| 18 | u16 | max radio → host body (1024) |
| 20 | u8 | port: CAT port index, 0xFF for DIAG |
| 21 | u8 | packets per frame now (section 7) |

Caps bits:

| Bit | Feature |
|---|---|
| 0 | SCREEN |
| 1 | SPECTRUM |
| 2 | CWDEC |
| 3 | EVENTS |
| 4 | SETVAL |
| 5 | CAT tunnel |
| 6 | TX (ARM, PADDLE, MEMSEND, TUNE) |
| 7 | MONITOR (monitor mix into USB capture) |
| 8 | INPUT |
| 9 | BAND |

### 0x02 STATUS

Sent at the subscribed rate (default 10 Hz, up to 20 Hz), also during TX and keyed CW, when
nothing else is.

| Off | Type | Field |
|---|---|---|
| 0 | u32 | uptime ms |
| 4 | u32 | dial Hz (as the panel shows it) |
| 8 | u8 | radio state: 0 SETUP, 1 RECEIVE, 2 TX_PREPARE, 3 TRANSMIT, 4 FAULT, 5 OFF |
| 9 | u8 | mode: 0 USB, 1 LSB, 2 CW, 3 AM, 4 FM, 5 DIGI |
| 10 | u8 | page (`ui_page_t`) |
| 11 | u8 | ui: bits 0-1 home level, 2-3 zone, 4-5 menu level, 6 editing, 7 boot log held |
| 12 | u8 | flags0 (below) |
| 13 | u8 | flags1 (below) |
| 14 | u8 | TX owner: 0 none, 1 CAT port 0, 2 CAT port 1, 3 LINK paddle/TUNE, 4 LINK memory, 5 radio key/paddle, 6 microphone/VOX, 0xFF other |
| 15 | u8 | receive + render load % (`RADIO_AudioLoadPct`) |
| 16 | u16 | radio fault bits (`RADIO_FAULT_*`) |
| 18 | u16 | VIN mV |
| 20 | u16 | PEP in centiwatts, last meter reading ≤ 3 s old (0 = none) |
| 22 | u16 | SWR x 100 (0 = no valid reading) |
| 24 | i16 | receive level dBFS x 10 |
| 26 | u8 | CW WPM |
| 27 | u8 | CAT: bits 0-1 ports open, bits 4-7 console port (0xF none) |
| 28 | u16 | time since the last CAT command on port 0, 0.1 s (saturates at 65535) |
| 30 | u16 | same for port 1 |
| 32 | u32 | settings hash (changes when any setting value changes) |
| 36 | u16 | USB-PD contract mV (0 none) |
| 38 | u16 | receive overruns (low 16 bits, wraps) |
| 40 | u16 | USB capture gaps the host heard in the last 10 minutes, as the DIGI page's GAPS counts them (a loss counts once the host has pulled 20 ms past it; the overflow a stopping host leaves behind is not a gap) |
| 42 | u8 | reference discipline: 0 idle, 1 acquiring, 2 locked |
| 43 | u8 | link: bit 0 heartbeat ok, bit 1 host subscribed to SCREEN |
| 44 | i16 | reference trim ppm x 100 |
| 46 | u8 | band index (0xFF none) |
| 47 | u8 | ARM seconds left (0 = disarmed) |

flags0 bits:

| Bit | Meaning |
|---|---|
| 0 | TX MUTE |
| 1 | USB RAW IQ (capture is I/Q) |
| 2 | USB VOX |
| 3 | sideband is LSB |
| 4 | audio realtime-critical (console paused) |
| 5 | background I/O blocked (storage, SD) |
| 6 | monitor mix on |
| 7 | radio phones off for the PC monitor |

flags1 bits:

| Bit | Meaning |
|---|---|
| 0 | LINK TX armed |
| 1 | CAT PTT on |
| 2 | memory sending |
| 3 | mode or band change pending |
| 4 | pipeline lockout |
| 5 | DIAG personality |
| 6 | keyer cannot radiate (practice, trainer, TX MUTE) |
| 7 | reserved |

### 0x03 SCREEN

| Off | Type | Field |
|---|---|---|
| 0 | u8 | page 0..29 (8 rows each) |
| 1 | u8 | flags: bit 0 band page (no data: the colour band covers it, see BAND/SPECTRUM); bit 1 last page of a full refresh |
| 2 | ... | RLE of 360 bytes |

The 360 bytes are 320 bitmap bytes followed by 40 attribute bytes:
- Bitmap: byte `x` is column `x`; its bit `n` is row `page*8 + n` (1 = foreground).
- Attributes: byte `c` is the 8x8 tile at column `c`; high nibble foreground colour, low
  nibble background colour, both indexes into the 16-colour palette (section 8).

RLE ("PackBits", unsigned):

| Control byte `c` | Meaning |
|---|---|
| `c < 0x80` | `c + 1` literal bytes follow (1..128) |
| `c >= 0x80` | the next byte repeats `c - 0x80 + 2` times (2..129) |

Decoding must produce exactly 360 bytes, otherwise the frame is dropped. Pages are sent
latest-wins: a page that changes again before it is sent goes out once, with its newest
content.

### 0x04 BAND

| Off | Type | Field |
|---|---|---|
| 0 | u8 | first band page |
| 1 | u8 | number of band pages (0 = no band) |
| 2 | u8 | kind: 1 waterfall/spectrum |

Sent on every change and with a full refresh.

### 0x05 SPECTRUM

One per waterfall change, only while the WATERFALL page is open (the engine runs only
then). A 6-byte prefix, the engine's own `spec_view_t` exactly as the firmware holds it
(32 bytes, natural alignment; the firmware pins the layout with static asserts), then the
data.

| Off | Type | Field |
|---|---|---|
| 0 | u16 | line sequence: advances when a new history line exists (the host appends a line only then; otherwise only the trace, axis or scale changed) |
| 2 | u8 | flags: bit 0 trace follows, bit 1 peak follows |
| 3 | u8 | palette (0 CLASSIC, 1 GRAY, 2 HEAT, 3 OCEAN) |
| 4 | u16 | width `w` in pixels (304); the centre pixel is `w / 2` |
| 6 | u32 | centre Hz (at the centre pixel) |
| 10 | f32 | Hz per pixel |
| 14 | u32 | dial Hz |
| 18 | i16 | dial x (pixels, may lie off screen) |
| 20 | i16 | pass-band low x |
| 22 | i16 | pass-band high x |
| 24 | u8 | axis valid |
| 25 | u8 | receiving |
| 26 | u8 | scanning |
| 27 | u8 | sweep % |
| 28 | u8 | history lines filled |
| 29 | u8 | receive load % |
| 30 | u8 | waterfall load % |
| 31 | u8 | TX guard holding |
| 32 | i16 | colour scale bottom, dBFS |
| 34 | i16 | colour scale top, dBFS |
| 36 | u16 | lines per second x 10 |
| 38 | u8[w/2] | the newest history line: 4-bit levels; pixel `x` is the low nibble of byte `x/2` when `x` is even, the high nibble when odd |
| ... | u8[w] | trace, if flag bit 0: dBFS + 160, clamped to 1..255; 0 = no data |
| ... | u8[w] | peak hold, if flag bit 1: dBFS + 160; 0 = empty |

### 0x06 EVENT

One per new pipeline event.

| Off | Type | Field |
|---|---|---|
| 0 | u16 | age ms when sent |
| 2 | u8 | stage |
| 3 | u8 | reason |
| 4 | u8 | severity |
| 5 | u8 | reserved |
| 6 | u32 | value |

Names come from the SD pack (`SCTRL.BIN`), not from flash.

### 0x07 CWDEC

| Off | Type | Field |
|---|---|---|
| 0 | u32 | decoder character count after this frame (0 again after a clear) |
| 4 | ... | the new characters, ASCII |

### 0x08 CONSOLE

Raw console output: the same CRLF lines, `OK` / `ERR ...` terminators and `# ` commentary as
on the text console.

### 0x09 CATREPLY

The reply to one `CAT` frame, e.g. `FA00014074000;`. An empty payload means the command has
no reply. `?;` means refused.

### 0x0A SETVAL

Repeated 5-byte records `{id:u8, value:f32}` (setting ids as `set list`). Sent for a
`GETVAL`, and with `SETVAL` push subscribed, for the settings whose value changed (found by
the settings hash scan, section 7).

### 0x0B ACK

| Off | Type | Field |
|---|---|---|
| 0 | u8 | request type |
| 1 | u8 | request seq |
| 2 | u8 | code |
| 3 | u8 | detail |

| Code | Name | Meaning |
|---|---|---|
| 0 | OK | done / accepted |
| 1 | BUSY_TX | refused: the radio transmits |
| 2 | BUSY_OWNER | refused: another TX owner (CAT PTT, the radio's own key) |
| 3 | BUSY_RT | refused now: audio realtime-critical; retry in receive |
| 4 | NOT_ARMED | TX request without ARM |
| 5 | FAULT | a fault or lockout is latched |
| 6 | BAD | malformed request |
| 7 | UNSUPPORTED | not in this firmware |
| 8 | BUSY | the previous console command has not finished |
| 9 | CAPPED | TX released by a cap (TXEVENT also) |
| 10 | MUTED | TX MUTE on: sidetone only, no RF |
| 11 | RANGE | value out of range |
| 12 | DIAG | not in the DIAG personality |
| 13 | PENDING | a mode or band change is in flight |

Every host request with an effect gets exactly one ACK, except `PADDLE` and `HEARTBEAT`
(ACKed only when refused) and `CMD`/`CAT`/`GETVAL`, whose replies are the answer
(ACKed only when refused).

### 0x0C BYE

| Off | Type | Field |
|---|---|---|
| 0 | u8 | reason: 0 host asked, 1 DTR dropped, 2 USB reset/suspend, 3 personality switch |

### 0x0D TXEVENT

| Off | Type | Field |
|---|---|---|
| 0 | u8 | kind: 1 armed, 2 disarmed, 3 key-down cap released, 4 heartbeat lost: released, 5 stopped (TX_STOP), 6 memory finished, 7 lever not refreshed: released, 8 memory cap released, 9 another owner took TX |
| 1 | u8 | reserved |
| 2 | u16 | detail (ms or count) |

## 4. Host → radio frames

| Type | Name | Payload | Notes |
|---|---|---|---|
| 0x40 | HEARTBEAT | optional u16 host ms | every 250-500 ms. No heartbeat for 1500 ms = release (section 6). |
| 0x41 | SUB | u16 mask, u8 packets/frame (1-4, default 2), u8 STATUS Hz (1-20, default 10) | mask bits: 0 STATUS, 1 SCREEN, 2 SPECTRUM, 3 SPECTRUM trace+peak, 4 EVENTS, 5 CWDEC, 6 SETVAL push. ACK. |
| 0x42 | REFRESH | u8 what | bit 0 all screen pages, 1 BAND, 2 all SETVAL, 3 CW decoder text, 4 STATUS now. ACK. |
| 0x43 | INPUT | u8 code, i8 value | panel parity, codes below. ACK (BUSY_RT while realtime-critical). |
| 0x44 | PADDLE | u8 levers | bit 0 DIT, bit 1 DAH. Repeat every ≤ 100 ms while non-zero; released 250 ms after the last one. Needs ARM unless the keyer cannot radiate. |
| 0x45 | TX_ARM | u8 on, u8 key-down max (0.1 s, 1-255), u8 memory max (s, 1-255), u8 ARM TTL (s, 5-255) | ACK. Refused in DIAG, with a fault or lockout, or while another owner transmits. |
| 0x46 | TX_STOP | none | Always accepted: levers released, memory stopped, TUNE ended, LINK-owned CAT PTT dropped, disarmed. ACK + TXEVENT 5. |
| 0x47 | MEMSEND | ASCII ≤ 48 | Through `MOD_CwMemorySend` (the keyer's timing, holds and break-in). Needs ARM unless the keyer cannot radiate. ACK. |
| 0x48 | CMD | one console line, no newline | Output arrives as CONSOLE frames up to `OK`/`ERR`. One command at a time (BUSY otherwise). Not run while realtime-critical; it waits, as on the text console. |
| 0x49 | CAT | one CAT command, e.g. `FA14074000;` | Reply as CATREPLY. `TX`/`RX`/`TQ1` via the tunnel are refused (UNSUPPORTED): LINK TX uses section 6. |
| 0x4A | GETVAL | u8 ids (none = all) | Reply as SETVAL frames. |
| 0x4B | MONITOR | u8 flags, u8 gain | bit 0 monitor mix into USB capture, bit 1 radio phones off while the monitor is on. Gain 0-255 = 0..2x. ACK (UNSUPPORTED before caps bit 7). |
| 0x4C | LINK_OFF | none | BYE 0, then the port is a CAT port again. |
| 0x4D | TUNE | u8 on | Carrier at TUNE % on the TX TEST page. Needs ARM. The key-down cap applies. ACK. |

INPUT codes:

| Code | Event | Value |
|---|---|---|
| 1 | knob turn | detents |
| 2 | knob turn with shaft held | detents |
| 3 | knob click | — |
| 4 | knob hold | — |
| 5 / 6 / 7 | UP / DN / SEL press | — |
| 8 / 9 / 10 | UP / DN / SEL long | — |
| 11 | FUNC press | — |
| 12 | FUNC long | — |
| 13 | knob turn with FUNC held | detents |

## 5. What runs when

| Radio condition | LINK behaviour |
|---|---|
| Receiving, idle | everything subscribed |
| `MOD_AudioRealtimeCritical` (keyed CW, TX streams) | STATUS, ACK, TXEVENT, BYE; HEARTBEAT, PADDLE and TX_STOP are taken. SCREEN, SPECTRUM, CONSOLE, CAT, GETVAL and INPUT wait (INPUT: BUSY_RT). |
| Storage busy (save, SD) | as receiving, console commands that need storage answer `ERR storage busy` as today |
| TX from any source | as realtime-critical; the mirror freezes like the panel |

## 6. TX safety (firmware side)

These hold whatever the host does:

1. **ARM.**
   - `PADDLE` (when it could radiate), `MEMSEND` and `TUNE` need ARM.
   - ARM expires after its TTL without TX activity. It also ends on: heartbeat loss, LINK
     end, `TX_STOP`, a mode or band change, a fault or lockout, and another owner keying.
2. **Heartbeat.** 1500 ms without one releases levers, memory, TUNE and LINK-owned PTT,
   disarms, and turns the monitor mix and phones-off off (TXEVENT 4).
3. **Lever refresh.** A lever not refreshed within 250 ms is released (TXEVENT 7).
4. **Key-down cap.** One continuous key-down (lever or TUNE) longer than the ARM's key-down
   max is released (TXEVENT 3). Firmware ceiling: 25.5 s.
5. **Memory cap.** A memory still sending after its max is stopped (TXEVENT 8).
6. **One owner.** While CAT PTT or the radio's own key transmits, LINK TX requests get
   BUSY_OWNER. If another owner keys during LINK TX, LINK TX is released (TXEVENT 9).
7. **Never in DIAG.** TX is muted there; requests get DIAG.
8. **Existing protections are untouched:** DTR/suspend/reset release, CAT timeout,
   VIN/SWR/stray-RF trips, HRTIM halt kill.

The host adds its own stricter limits on top. S-CTRL's defaults: TUNE 5 s, stuck lever 3 s,
memory 60 s, duty 50 % over 5 min; bench 1 s.

## 7. Firmware scheduling, pacing and cost

- **Ring.** One 256-byte TX ring. The main loop streams frames into it. A frame longer
  than the ring (SPECTRUM, a screen page) is produced across several passes from its source
  memory. No other frame can start until it ends, which costs at most ~13 ms of latency at
  64 KB/s.
- **Priority** when a new frame is chosen:
  1. ACK / TXEVENT / BYE
  2. STATUS
  3. CATREPLY
  4. CONSOLE
  5. EVENT / CWDEC / SETVAL
  6. SPECTRUM
  7. BAND / SCREEN
- **SOF pacing.**
  - The USB SOF callback arms one IN packet.
  - The IN-complete callback re-arms only while fewer than N packets have gone out in this
    frame and less than ~600 µs has passed since the SOF.
  - Throughput is 32 x N bytes per ms (N = 2: 64 KB/s). USB IRQs are bounded to N + 1 per
    ms and kept clear of the next SOF, which the reference discipline time-stamps.
  - The last packet of a burst is never a full 32 bytes (Linux cdc-acm would hold it).
- **Screen.** It uses the panel flush's changed-page mask (no extra hashing). At most one
  page is encoded per main-loop pass, and only while non-critical UI work is allowed. A page
  whose hash changes while it is sent is sent again.
- **Settings hash.** Each STATUS tick folds a slice of the setting getters into a running
  hash: the full table about once a second, no hooks in the setters.
- **Idle cost.** With no LINK the service is one flag test per pass.

## 8. Palettes (host constants, checked by golden tests)

GFX palette (RGB565, index 0-15), computed from `ui_gfx.c`'s `RGB()` triples:

| Index | Name | RGB565 | Index | Name | RGB565 |
|---|---|---|---|---|---|
| 0 | BG | `0x0000` | 8 | SELECT | `0x02CB` |
| 1 | PANEL | `0x00E2` | 9 | FREQ | `0x4FEF` |
| 2 | PANEL_HI | `0x01A4` | 10 | OK | `0x378C` |
| 3 | LINE | `0x0347` | 11 | WARN | `0xFE20` |
| 4 | TEXT | `0xE7FD` | 12 | ALERT | `0xFA06` |
| 5 | DIM | `0x6712` | 13 | ALERT_BG | `0x8000` |
| 6 | MUTED | `0x2D0C` | 14 | OK_BG | `0x0284` |
| 7 | ACCENT | `0x075C` | 15 | WHITE | `0xFFFF` |

The host's golden test checks them against `ui_preview.py` frames.

Waterfall palettes: `ui_waterfall.c` `palettes[4][16]`, copied into the host with the
same check.

## 9. SD pack `SCTRL.BIN` (names stay off flash)

Generated by `alt2/tools/gen_sctrl_pack.py` from the firmware sources and installed on the
card with the other card files (`flash_rev1.py`, `card`). It holds:
- event stage, reason and severity names;
- fault and pipeline bit names;
- page names;
- setting metadata (id, name, style, limits, step, decimals, list labels, help id);
- INPUT code names;
- the palettes.

The host reads it once per image CRC (`sd get SCTRL.BIN`) and caches it in
`~/.cache/sctrl/<crc>/`. Format: section 10 (defined with the generator in M2).
