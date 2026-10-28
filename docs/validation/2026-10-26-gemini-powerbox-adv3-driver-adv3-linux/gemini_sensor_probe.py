#!/usr/bin/env python3
"""Sensor-presence probe of the Gemini Power & Data Hubs Advanced 3 for the
design doc's hardware validation item 3: what the frame carries while the
AHT20, the DS18B20, or both are unplugged.

Opens the port as the driver does (19200 8N1, DTR and RTS raised, HUPCL
cleared), sends a bare `>G#` every two seconds and logs every received
chunk with a timestamp. Every `*G…#` span is printed raw, then split: the
fixed prefix (fields 0–9) by a strict pattern, and the remainder — the four
sensor fields and the three electrical ones — shown exactly as the hub
wrote it, plus whether the driver-shaped pattern (each sensor field ending
at its own tag letter) still matches. Nothing is written to the hub.

The operator unplugs and replugs the sensors while it runs; a `label` file
in the log directory names the phase on every line, and a `stop` file ends
the run (or 25 minutes do).
"""
import fcntl
import os
import pathlib
import re
import select
import struct
import sys
import termios
import time

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyUSB0"
LOGDIR = pathlib.Path(sys.argv[2] if len(sys.argv) > 2 else ".")
TIOCMBIS = 0x5416
TIOCM_DTR = 0x002
TIOCM_RTS = 0x004
T0 = time.monotonic()
PREFIX = re.compile(r"\*G(\d{4})D(\d{6})U(\d)A(\d)T(\d)A(\d)M(\d)B(\d)M\s*(\d+)C\s*(\d+)C")
DRIVER_SHAPE = re.compile(
    r"\*G(\d{4})D(\d{6})U(\d)A(\d)T(\d)A(\d)M(\d)B(\d)M\s*(\d+)C\s*(\d+)C"
    r"([^S]*)S([^T]*)T([^H]*)H([^D]*)D\s*([^V]*)V\s*([^C]*)C\s*([^P]*)P#"
)


def ts():
    return f"{time.monotonic() - T0:8.3f}"


def label():
    try:
        return (LOGDIR / "label").read_text().strip() or "-"
    except OSError:
        return "-"


def log(msg):
    print(f"[{ts()}] [{label()}] {msg}", flush=True)


def open_port():
    fd = os.open(PORT, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK)
    iflag, oflag, cflag, lflag, _, _, cc = termios.tcgetattr(fd)
    iflag &= ~(termios.IGNBRK | termios.BRKINT | termios.PARMRK | termios.ISTRIP
               | termios.INLCR | termios.IGNCR | termios.ICRNL | termios.IXON
               | termios.IXOFF | termios.IXANY)
    oflag &= ~termios.OPOST
    lflag &= ~(termios.ECHO | termios.ECHONL | termios.ICANON | termios.ISIG | termios.IEXTEN)
    cflag &= ~(termios.CSIZE | termios.PARENB | termios.CSTOPB | termios.CRTSCTS | termios.HUPCL)
    cflag |= termios.CS8 | termios.CREAD | termios.CLOCAL
    cc[termios.VMIN] = 0
    cc[termios.VTIME] = 0
    termios.tcsetattr(fd, termios.TCSANOW,
                      [iflag, oflag, cflag, lflag, termios.B19200, termios.B19200, cc])
    fcntl.ioctl(fd, TIOCMBIS, struct.pack("I", TIOCM_DTR | TIOCM_RTS))
    log(f"opened {PORT}")
    return fd


class Reader:
    def __init__(self, fd):
        self.fd = fd
        self.buf = b""
        self.last = None

    def pump(self, seconds):
        end = time.monotonic() + seconds
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                return
            r, _, _ = select.select([self.fd], [], [], min(remaining, 0.25))
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 512)
            except BlockingIOError:
                continue
            if not chunk:
                continue
            log(f"rx {chunk!r}")
            self.buf += chunk
            self.drain()

    def drain(self):
        while True:
            start = self.buf.find(b"*")
            if start < 0:
                self.buf = b""
                return
            end = self.buf.find(b"#", start)
            if end < 0:
                self.buf = self.buf[start:]
                return
            span = self.buf[start:end + 1]
            self.buf = self.buf[end + 1:]
            self.frame(span)

    def frame(self, span):
        text = span.decode("ascii", "backslashreplace")
        if text == self.last:
            log(f"FRAME (unchanged) {text}")
            return
        self.last = text
        log(f"FRAME {text}")
        m = PREFIX.match(text)
        if not m:
            log("  prefix: DOES NOT MATCH the fixed fields 0-9")
            return
        f = m.groups()
        rest = text[m.end():]
        log(f"  DC={f[0]} USB={f[1]} AHT20={f[2]} DS18B20={f[3]} DEW6en={f[4]} mode={f[5]} "
            f"DEW7en={f[6]} mode={f[7]} duty6={f[8]} duty7={f[9]}")
        log(f"  sensor+electrical remainder: {rest!r}")
        d = DRIVER_SHAPE.match(text)
        if d:
            g = d.groups()
            log(f"  driver shape OK: lens={g[10]!r} ambient={g[11]!r} humidity={g[12]!r} "
                f"dewpoint={g[13]!r} V={g[14]!r} A={g[15]!r} W={g[16]!r}")
        else:
            log("  driver shape: DOES NOT MATCH (a sensor field holds a tag letter or the frame is cut)")


fd = open_port()
rd = Reader(fd)
deadline = time.monotonic() + 25 * 60
try:
    termios.tcflush(fd, termios.TCIOFLUSH)
    while time.monotonic() < deadline and not (LOGDIR / "stop").exists():
        os.write(fd, b">G#\n")
        rd.pump(2.0)
finally:
    os.close(fd)
    log("closed")
print("probe complete", flush=True)
