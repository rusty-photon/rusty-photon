#!/usr/bin/env python3
"""Power-cycle probe of the Gemini Power & Data Hubs Advanced 3 for the
design doc's hardware validation item 5: do the outputs and the dew
channels come back as they were, or at the hub's defaults?

Three phases, each a separate run so the operator can cut the power in
between (the hub's CH340 dies with its 12 V, so the port goes away):

  before   read the frame, then set a state no default resembles — DC3 off,
           USB A off, DEW6 duty 50 %, DEW6 mode Switch, DEW6 disabled —
           every set in one buffer with a `>G#` behind it, and read it back
  after    wait for the port to come back, log everything the hub sends on
           its own for twelve seconds (a boot banner, the periodic frame),
           then ask `>H#`, `>V#`, `>G#`
  restore  put the power-up state back: DC3 on, USB A on, DEW6 Manual,
           enabled, 100 %, and read it back

Every chunk is logged with a timestamp; every `*G…#` frame is parsed.
"""
import fcntl
import os
import re
import select
import struct
import sys
import termios
import time

PHASE = sys.argv[1]
PORT = sys.argv[2] if len(sys.argv) > 2 else "/dev/serial/by-id/usb-1a86_USB_Serial-if00-port0"
TIOCMBIS = 0x5416
TIOCM_DTR = 0x002
TIOCM_RTS = 0x004
T0 = time.monotonic()
FRAME = re.compile(
    r"\*G(\d{4})D(\d{6})U(\d)A(\d)T(\d)A(\d)M(\d)B(\d)M\s*(\d+)C\s*(\d+)C"
    r"([^S]*)S([^T]*)T([^H]*)H([^D]*)D\s*([^V]*)V\s*([^C]*)C\s*([^P]*)P#"
)


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
    log(f"opened {PORT} -> {os.path.realpath(PORT)}")
    return fd


class Reader:
    def __init__(self, fd):
        self.fd = fd
        self.buf = b""

    def listen(self, seconds, tag):
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
            log(f"{tag} rx {chunk!r}")
            self.buf += chunk
            self.drain(tag)

    def drain(self, tag):
        while True:
            start = self.buf.find(b"*")
            if start < 0:
                self.buf = b""
                return
            end = self.buf.find(b"#", start)
            if end < 0:
                self.buf = self.buf[start:]
                return
            span = self.buf[start:end + 1].decode("ascii", "backslashreplace")
            self.buf = self.buf[end + 1:]
            m = FRAME.match(span)
            if m:
                g = m.groups()
                log(f"{tag} FRAME DC={g[0]} USB={g[1]} AHT20={g[2]} DS18B20={g[3]} "
                    f"DEW6en={g[4]} mode={g[5]} DEW7en={g[6]} mode={g[7]} duty6={g[8]} duty7={g[9]} "
                    f"lens={g[10].strip()} amb={g[11].strip()} hum={g[12].strip()} dew={g[13].strip()} "
                    f"{g[14].strip()}V {g[15].strip()}A {g[16].strip()}W")
            else:
                log(f"{tag} SPAN {span}")


def send(fd, text):
    os.write(fd, text.encode("ascii"))
    log(f"tx {text!r}")


def step(rd, title, text, wait=2.5):
    print(f"\n----- {title}", flush=True)
    send(rd.fd, text)
    rd.listen(wait, title.split()[0])


def wait_for_port(timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if os.path.exists(PORT):
            log(f"port present: {PORT} -> {os.path.realpath(PORT)}")
            return True
        time.sleep(0.2)
    log(f"port did not appear within {timeout}s")
    return False


def wait_for_absence(timeout):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if not os.path.exists(PORT):
            log("port gone: the hub is off")
            return True
        time.sleep(0.2)
    log(f"port never went away within {timeout}s")
    return False


if PHASE == "after":
    log("waiting for the power cut")
    if not wait_for_absence(600) or not wait_for_port(180):
        sys.exit(1)
    time.sleep(1.0)  # let udev finish with the node

fd = open_port()
rd = Reader(fd)
try:
    if PHASE == "before":
        termios.tcflush(fd, termios.TCIOFLUSH)
        rd.listen(1.0, "silence")
        step(rd, "baseline >G#", ">G#\n")
        step(rd, "set DC3 off", ">C3#\n>G#\n")
        step(rd, "set USB-A off", ">C6#\n>G#\n")
        step(rd, "set DEW6 duty 50", ">X50#\n>G#\n")
        step(rd, "set DEW6 mode Switch", ">M12#\n>G#\n")
        step(rd, "set DEW6 disabled", ">Z10#\n>G#\n")
        step(rd, "state before the cut >G#", ">G#\n")
    elif PHASE == "after":
        print("\n----- the hub's own output after power-up (12 s, nothing sent)", flush=True)
        rd.listen(12.0, "boot")
        step(rd, "identity >H#", ">H#\n")
        step(rd, "firmware >V#", ">V#\n")
        step(rd, "state after the cut >G#", ">G#\n")
        step(rd, "second >G#", ">G#\n")
    elif PHASE == "restore":
        termios.tcflush(fd, termios.TCIOFLUSH)
        step(rd, "before restore >G#", ">G#\n")
        step(rd, "restore DC3 on", ">O3#\n>G#\n")
        step(rd, "restore USB-A on", ">O6#\n>G#\n")
        step(rd, "restore DEW6 mode Manual", ">M11#\n>G#\n")
        step(rd, "restore DEW6 enabled", ">Z11#\n>G#\n")
        step(rd, "restore DEW6 duty 100", ">X100#\n>G#\n")
        step(rd, "final >G#", ">G#\n")
    else:
        sys.exit(f"unknown phase {PHASE}")
finally:
    os.close(fd)
    log("closed")
print(f"\n{PHASE} complete", flush=True)
