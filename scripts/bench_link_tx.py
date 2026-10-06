#!/usr/bin/env python3
"""Bench check of the radio's own LINK TX guard (firmware side), with a host
that misbehaves on purpose: it never releases, never refreshes, leaves TX
TEST under a TUNE, stops its heartbeat. Each case expects the radio to end
the key-down by itself, and reports when it did.

Safety: run only into a 50 ohm dummy load, with no S-CTRL holding the port
(`pgrep -x sctrl`). Every key-down has this script's own TX_STOP at 0.9 s as
a backstop, and the radio's ARM key-down max is set below that.

    scripts/bench_link_tx.py [--port /dev/serial/by-id/...-if05] [CASE ...]
cases: refuse heartbeat cap stale busy dtr
"""
import argparse
import glob
import os
import select
import struct
import sys
import termios
import time

END, ESC, ESC_END, ESC_ESC = 0xC0, 0xDB, 0xDC, 0xDD
T_STATUS, T_ACK, T_TXEVENT, T_HELLO = 0x02, 0x0B, 0x0D, 0x01
H_HEARTBEAT, H_SUB, H_INPUT, H_PADDLE, H_ARM, H_STOP, H_TUNE = 0x40, 0x41, 0x43, 0x44, 0x45, 0x46, 0x4D
PAGE_HOME, PAGE_TX_TEST = 0, 6
BACKSTOP_S = 0.9
TXE = {1: "armed", 2: "disarmed", 3: "key-down cap", 4: "heartbeat lost", 5: "stopped", 6: "memory done",
       7: "lever stale", 8: "memory cap", 9: "other owner", 10: "TUNE left TX TEST"}


def crc16(b, c=0xFFFF):
    for x in b:
        c ^= x << 8
        for _ in range(8):
            c = ((c << 1) ^ 0x1021) if c & 0x8000 else (c << 1)
            c &= 0xFFFF
    return c


def frame(ty, seq, payload=b""):
    body = bytes([ty, seq]) + payload
    body += struct.pack("<H", crc16(body))
    out = bytearray([END])
    for x in body:
        if x == END:
            out += bytes([ESC, ESC_END])
        elif x == ESC:
            out += bytes([ESC, ESC_ESC])
        else:
            out.append(x)
    out.append(END)
    return bytes(out)


class Link:
    def __init__(self, port):
        self.fd = os.open(port, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
        t = termios.tcgetattr(self.fd)
        t[0] = t[1] = t[3] = 0
        t[2] |= termios.CLOCAL | termios.CREAD | termios.HUPCL
        t[6][termios.VMIN] = 1
        t[6][termios.VTIME] = 0
        termios.tcsetattr(self.fd, termios.TCSANOW, t)
        termios.tcflush(self.fd, termios.TCIFLUSH)
        self.seq = 0
        self.buf = bytearray()
        self.esc = False
        self.status = None
        self.status_t = 0.0
        self.acks = []
        self.txevents = []
        self.hb_on = True
        self.hb_at = 0.0
        self.hello = False
        os.write(self.fd, b"link 1\n")
        text = b""
        end = time.time() + 3
        while b"OK\r\n" not in text:
            if time.time() > end:
                raise SystemExit(f"no OK to `link 1`: {text!r}")
            r, _, _ = select.select([self.fd], [], [], 0.1)
            if r:
                text += os.read(self.fd, 512)
        rest = text.split(b"OK\r\n", 1)[1]
        self.feed(rest)
        # Nothing goes out before HELLO: until the radio has switched the port,
        # bytes reach its text console and are lost.
        self.hb_on = False
        self.wait(lambda: self.hello, 3, "HELLO")
        self.hb_on = True
        self.send(H_HEARTBEAT, struct.pack("<H", 0))
        self.send(H_SUB, struct.pack("<HBB", 0x0001, 2, 20))  # STATUS only, 20 Hz
        self.wait(lambda: self.status is not None, 3, "STATUS")

    def close(self):
        os.close(self.fd)

    def send(self, ty, payload=b""):
        os.write(self.fd, frame(ty, self.seq & 0x7F, payload))
        self.seq += 1

    def feed(self, data):
        for x in data:
            if x == END:
                if len(self.buf) >= 4 and crc16(self.buf[:-2]) == struct.unpack("<H", self.buf[-2:])[0]:
                    self.handle(self.buf[0], bytes(self.buf[2:-2]))
                self.buf = bytearray()
                self.esc = False
            elif x == ESC:
                self.esc = True
            else:
                if self.esc:
                    x = END if x == ESC_END else ESC
                    self.esc = False
                self.buf.append(x)

    def handle(self, ty, p):
        now = time.time()
        if ty == T_HELLO:
            self.hello = True
        elif ty == T_STATUS and len(p) >= 48:
            s = struct.unpack_from("<IIBBBBBBBBHHHHhBBHBBIHHHBBhBB", p)
            self.status = {"state": s[2], "page": s[4], "flags1": s[7], "owner": s[8], "pep": s[12] / 100,
                           "swr": s[13] / 100, "vin": s[11] / 1000, "faults": s[10]}
            self.status_t = now
        elif ty == T_ACK:
            self.acks.append((now, p[0], p[1], p[2]))
        elif ty == T_TXEVENT:
            self.txevents.append((now, p[0]))

    def pump(self, dt=0.01):
        if self.hb_on and time.time() - self.hb_at > 0.3:
            self.hb_at = time.time()
            self.send(H_HEARTBEAT, struct.pack("<H", 0))
        r, _, _ = select.select([self.fd], [], [], dt)
        if r:
            self.feed(os.read(self.fd, 4096))

    def wait(self, cond, timeout, what):
        end = time.time() + timeout
        while not cond():
            if time.time() > end:
                raise SystemExit(f"timeout waiting for {what}")
            self.pump()

    def request(self, ty, payload=b""):
        """Send and wait for its ACK; returns the code."""
        seq = self.seq & 0x7F
        self.send(ty, payload)
        end = time.time() + 2
        while time.time() < end:
            self.pump()
            for a in self.acks:
                if a[1] == ty and a[2] == seq:
                    self.acks.remove(a)
                    return a[3]
        raise SystemExit(f"no ACK for 0x{ty:02x}")

    def transmitting(self):
        return self.status and self.status["state"] in (2, 3)


def arm(l, key_max_ds):
    code = l.request(H_ARM, bytes([1, key_max_ds, 5, 30]))
    assert code == 0, f"ARM refused: {code}"
    l.wait(lambda: l.status["flags1"] & 1, 1, "ARM in STATUS")


def watch(l, start, until_s):
    """Pump until `until_s` after start; returns (first TX seen, TX end) times and
    the backstop's use."""
    tx_on = tx_off = None
    backstop = False
    while time.time() - start < until_s:
        l.pump()
        if l.transmitting():
            tx_on = tx_on or time.time()
        elif tx_on and not tx_off:
            tx_off = time.time()
        if time.time() - start > BACKSTOP_S and l.transmitting() and not backstop:
            backstop = True
            l.send(H_STOP)
    return tx_on, tx_off, backstop


def report(name, start, tx_on, tx_off, backstop, l, expect_event):
    evs = [(round(t - start, 3), TXE.get(k, k)) for t, k in l.txevents if t >= start]
    held = (tx_off - tx_on) if (tx_on and tx_off) else None
    ok = (not backstop) and (expect_event is None or any(e == expect_event for _, e in evs))
    print(f"{'PASS' if ok else 'FAIL'} {name}: TX {'%.0f ms' % (held * 1000) if held else 'none' if not tx_on else 'unended'}"
          f" (from {'%.3f' % (tx_on - start) if tx_on else '-'} s), events {evs}{', BACKSTOP USED' if backstop else ''}")
    return ok


def need_page(l, page):
    if l.status["page"] != page:
        code = l.request(H_INPUT, bytes([14, page]))
        assert code == 0, f"open page {page}: ACK {code}"
        l.wait(lambda: l.status["page"] == page, 2, f"page {page}")
        time.sleep(0.3)


def case_cap(l):
    """TUNE never released: the radio's key-down cap (0.3 s) ends it."""
    need_page(l, PAGE_TX_TEST)
    arm(l, 3)
    start = time.time()
    code = l.request(H_TUNE, b"\x01")
    assert code == 0, f"TUNE ACK {code}"
    r = report("key-down cap ends a TUNE never released", start, *watch(l, start, 1.2), l, "key-down cap")
    l.request(H_TUNE, b"\x00")
    return r


def case_stale(l):
    """One PADDLE and no refresh on TX TEST: released after 250 ms."""
    need_page(l, PAGE_TX_TEST)
    arm(l, 8)
    start = time.time()
    l.send(H_PADDLE, b"\x01")
    r = report("lever not refreshed is released", start, *watch(l, start, 1.0), l, "lever stale")
    l.send(H_PADDLE, b"\x00")
    return r


def case_busy(l):
    """TUNE, then panel input at 0.2 s (open HOME): refused while transmitting
    (ACK BUSY_RT, no mode change under a key-down); the cap (0.4 s) ends it."""
    need_page(l, PAGE_TX_TEST)
    arm(l, 4)
    start = time.time()
    assert l.request(H_TUNE, b"\x01") == 0
    while time.time() - start < 0.2:
        l.pump()
    code = l.request(H_INPUT, bytes([14, PAGE_HOME]))
    r = report(f"panel input refused while keyed (ACK {code}), cap ends TUNE", start, *watch(l, start, 1.2), l,
               "key-down cap")
    l.request(H_TUNE, b"\x00")
    return r and code == 3 and l.status["page"] == PAGE_TX_TEST


def case_refuse(l):
    """TUNE anywhere but TX TEST is refused (ACK 14), no RF."""
    need_page(l, PAGE_HOME)
    arm(l, 5)
    start = time.time()
    code = l.request(H_TUNE, b"\x01")
    tx_on, tx_off, backstop = watch(l, start, 0.5)
    ok = code == 14 and not tx_on
    print(f"{'PASS' if ok else 'FAIL'} TUNE off TX TEST refused: ACK {code}, TX {'seen' if tx_on else 'none'}")
    return ok


def case_heartbeat(l):
    """Armed, then the heartbeat stops: the radio disarms (no RF here)."""
    need_page(l, PAGE_HOME)
    arm(l, 5)
    start = time.time()
    l.hb_on = False
    end = time.time() + 2.5
    while time.time() < end:
        l.pump()
    l.hb_on = True
    evs = [(round(t - start, 3), TXE.get(k, k)) for t, k in l.txevents if t >= start]
    disarmed = not (l.status["flags1"] & 1)
    ok = disarmed and any(e == "heartbeat lost" for _, e in evs)
    print(f"{'PASS' if ok else 'FAIL'} heartbeat loss disarms: armed now {not disarmed}, events {evs}")
    return ok


def case_dtr(port):
    """TUNE, then the PC's port closes at 0.2 s (DTR drops): TX ends."""
    l = Link(port)
    need_page(l, PAGE_TX_TEST)
    arm(l, 8)
    start = time.time()
    assert l.request(H_TUNE, b"\x01") == 0
    while time.time() - start < 0.2:
        l.pump()
    l.close()
    time.sleep(0.6)
    # A fresh LINK reads the radio's state after the drop.
    l2 = Link(port)
    st = l2.status
    ok = st["state"] == 1 and not (st["flags1"] & 1)
    print(f"{'PASS' if ok else 'FAIL'} closing the port ends TX and disarms: state {st['state']}, armed {bool(st['flags1'] & 1)}")
    l2.request(H_INPUT, bytes([14, PAGE_HOME]))
    l2.close()
    return ok


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--port", default=None)
    ap.add_argument("cases", nargs="*", default=["refuse", "heartbeat", "cap", "stale", "busy", "dtr"])
    a = ap.parse_args()
    port = a.port or (glob.glob("/dev/serial/by-id/usb-Chireix_SPARTACUS_REV1_*-if05") or [None])[0]
    if not port:
        raise SystemExit("no SPARTACUS LINK port")
    results = []
    l = Link(port)
    try:
        s = l.status
        assert s["state"] == 1 and s["faults"] == 0, f"radio not idle: {s}"
        for c in a.cases:
            if c == "dtr":
                continue
            results.append(globals()[f"case_{c}"](l))
            time.sleep(0.5)
    finally:
        l.send(H_STOP)
        l.send(H_INPUT, bytes([14, PAGE_HOME]))
        time.sleep(0.2)
        l.close()
    if "dtr" in a.cases:
        time.sleep(0.5)
        results.append(case_dtr(port))
    print(f"{sum(results)}/{len(results)} passed")
    sys.exit(0 if all(results) else 1)


if __name__ == "__main__":
    main()
