#!/usr/bin/env python3
"""Write probe of the Gemini Power & Data Hubs Advanced 3 for the design
doc's hardware validation items 1, 2 and 4.

Every set goes out in ONE buffer with a `>G#` behind it, as the driver
sends it, and every byte is logged with a timestamp; each complete `*G`
frame is parsed and its fields 0 (DC), 4 (DEW6 enabled), 5 (DEW6 mode),
8 (DEW6 duty) and 9 (DEW7 duty) printed. DEW7 is never changed, DC3 is
the one output toggled, and the end state is the power-up state: every
output on, DEW6 Manual, enabled, 100 %.
"""
import fcntl
import os
import re
import select
import struct
import sys
import termios
import time

PORT = sys.argv[1] if len(sys.argv) > 1 else "/dev/ttyUSB0"
TIOCMBIS = 0x5416
TIOCM_DTR = 0x002
TIOCM_RTS = 0x004
T0 = time.monotonic()
FRAME = re.compile(
    r"\*G(\d{4})D(\d{6})U(\d)A(\d)T(\d)A(\d)M(\d)B(\d)M\s*(\d+)C\s*(\d+)C"
    r"\s*([^S]*)S\s*([^T]*)T\s*([^H]*)H\s*([^D]*)D\s*([^V]*)V\s*([^C]*)C\s*([^P]*)P#"
)
NAMES = ["DC", "USB", "AHT20", "DS18B20", "DEW6en", "DEW6mode", "DEW7en", "DEW7mode",
         "DEW6duty", "DEW7duty", "lens", "ambient", "hum", "dewpt", "V", "A", "W"]


def ts():
    return f"{time.monotonic() - T0:8.3f}"


def log(msg):
    print(f"[{ts()}] {msg}", flush=True)


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
        self.buf = ""

    def listen(self, seconds, label):
        end = time.monotonic() + seconds
        while True:
            remaining = end - time.monotonic()
            if remaining <= 0:
                break
            r, _, _ = select.select([self.fd], [], [], min(remaining, 0.25))
            if not r:
                continue
            try:
                chunk = os.read(self.fd, 512)
            except BlockingIOError:
                continue
            if not chunk:
                continue
            log(f"{label} rx {chunk!r}")
            self.buf += chunk.decode("ascii", "replace")
            self.drain_frames(label)

    def drain_frames(self, label):
        while True:
            m = FRAME.search(self.buf)
            if not m:
                # keep only the tail after the last '*' so a cut frame cannot grow forever
                last = self.buf.rfind("*")
                if last > 0:
                    self.buf = self.buf[last:]
                return
            fields = [g.strip() for g in m.groups()]
            summary = " ".join(f"{n}={v}" for n, v in zip(NAMES, fields) if n in (
                "DC", "USB", "DEW6en", "DEW6mode", "DEW7en", "DEW7mode", "DEW6duty", "DEW7duty"))
            log(f"{label} FRAME {summary} | lens={fields[10]} amb={fields[11]} {fields[14]}V")
            self.buf = self.buf[m.end():]


def send(fd, text):
    data = text.encode("ascii")
    os.write(fd, data)
    log(f"tx {data!r}")


def step(reader, title, text, wait):
    print(f"\n----- {title}", flush=True)
    send(reader.fd, text)
    reader.listen(wait, title.split()[0])


fd = open_port()
rd = Reader(fd)
try:
    termios.tcflush(fd, termios.TCIOFLUSH)
    rd.listen(1.0, "silence")
    step(rd, "baseline >G#", ">G#\n", 2.5)

    # Item 1: two commands in one buffer, the set changing a value.
    step(rd, "item1 DC3 off in one buffer", ">C3#\n>G#\n", 2.5)
    step(rd, "item1 DC3 on in one buffer", ">O3#\n>G#\n", 2.5)

    # Item 2 (Manual): does field 8 follow >X at once?
    step(rd, "item2 Manual duty 50", ">X50#\n>G#\n", 2.5)
    step(rd, "item2 Manual duty 100", ">X100#\n>G#\n", 2.5)

    # Item 4: modes and the enable flag.
    step(rd, "item4 DEW6 Switch mode", ">M12#\n>G#\n", 2.5)
    step(rd, "item4 Switch: disable", ">Z10#\n>G#\n", 2.5)
    step(rd, "item4 Switch: enable", ">Z11#\n>G#\n", 2.5)
    step(rd, "item4 DEW6 Auto mode", ">M10#\n>G#\n", 2.5)
    print("\n----- stream in Auto (what does field 8 show?)", flush=True)
    rd.listen(9.5, "auto-stream")
    step(rd, "item4 Auto: disable", ">Z10#\n>G#\n", 2.5)
    rd.listen(3.5, "auto-disabled")
    step(rd, "item4 Auto: enable", ">Z11#\n>G#\n", 2.5)
    # Item 2 (Auto): what does a >X do while the loop runs?
    step(rd, "item2 Auto: duty 30", ">X30#\n>G#\n", 2.5)
    rd.listen(3.5, "auto-after-X30")
    step(rd, "item4 DEW6 Manual mode", ">M11#\n>G#\n", 2.5)
    step(rd, "item4 Manual: disable (what happens?)", ">Z10#\n>G#\n", 2.5)
    rd.listen(3.5, "manual-disabled")
    step(rd, "item4 Manual: enable", ">Z11#\n>G#\n", 2.5)
    step(rd, "restore duty 100", ">X100#\n>G#\n", 2.5)
    step(rd, "final >G#", ">G#\n", 2.5)
finally:
    os.close(fd)
    log("closed")
print("\nprobe complete", flush=True)
