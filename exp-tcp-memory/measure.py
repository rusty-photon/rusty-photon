"""Throwaway proof (exp/ branch only): the loopback connection's stalls and the kernel's counters around the macOS build.

  measure.py before                 print the TCP settings, save `netstat -s`, zero TCP's memory peak
  measure.py sample <file>          append "epoch mbuf-denials tcp-allocated" to <file> every second, forever
  measure.py after <file> <pcap>    print every `netstat -s` counter the build changed, the client-server
                                    connection's waits for an ACK (from the stall probe's capture), the mbuf
                                    denials during each long wait, and TCP's memory peak

Run as root (sudo): zeroing the peak writes a sysctl.
"""
import ctypes
import ctypes.util
import json
import re
import struct
import subprocess
import sys
import time

STATE = '/tmp/exp-tcp-memory.json'
# <sys/mem_acct_private.h>, xnu-12377.121.6
MEM_ACCT_PEAK = 1
MEM_ACCT_ALL_STATISTICS = 7
STATS = struct.Struct('<QqQQ16s')  # peak, allocated, softlimit, hardlimit, name
LONG_WAIT = 2.0
BUDGET = 18.0

libc = ctypes.CDLL(ctypes.util.find_library('c'), use_errno=True)


def mib(name):
    arr = (ctypes.c_int * 12)()
    n = ctypes.c_size_t(12)
    if libc.sysctlnametomib(name.encode(), arr, ctypes.byref(n)) != 0:
        raise OSError(ctypes.get_errno(), f'sysctlnametomib {name}')
    return list(arr[:n.value])


def sysctl(m, size, new=None):
    arr = (ctypes.c_int * len(m))(*m)
    buf = ctypes.create_string_buffer(size)
    oldlen = ctypes.c_size_t(size)
    newbuf = ctypes.create_string_buffer(new, len(new)) if new is not None else None
    newlen = ctypes.c_size_t(len(new) if new is not None else 0)
    if libc.sysctl(arr, len(m), buf, ctypes.byref(oldlen), newbuf, newlen) != 0:
        raise OSError(ctypes.get_errno(), f'sysctl {m}')
    return buf.raw[:oldlen.value]


def tcp_account():
    raw = sysctl(mib('kern.memacct') + [MEM_ACCT_ALL_STATISTICS, 0], STATS.size * 8)
    for i in range(len(raw) // STATS.size):
        peak, allocated, soft, hard, name = STATS.unpack_from(raw, i * STATS.size)
        if name.split(b'\0')[0] == b'TCP':
            return {'index': i, 'peak': peak, 'allocated': allocated, 'soft': soft, 'hard': hard}
    raise LookupError('no TCP memory account')


def netstat_s():
    """Every counter line of `netstat -s`, keyed by protocol and the line's text with its number taken out."""
    text = subprocess.run(['netstat', '-s'], capture_output=True, text=True).stdout
    out, proto = {}, '?'
    for line in text.splitlines():
        if line and not line[0].isspace():
            proto = line.rstrip(':')
            continue
        m = re.match(r'\s*(\d+)\s+(.*)', line)
        if m:
            key = f'{proto}: {m[2].strip()}'
            out[key] = out.get(key, 0) + int(m[1])
    return out


def mbuf_denials():
    text = subprocess.run(['netstat', '-m'], capture_output=True, text=True).stdout
    m = re.search(r'(\d+) requests for memory denied', text)
    return int(m[1]) if m else -1


def en0_bytes():
    """en0's input bytes, from `netstat -ibn -I en0`'s link-level row, or -1."""
    text = subprocess.run(['netstat', '-ibn', '-I', 'en0'], capture_output=True, text=True).stdout
    lines = text.splitlines()
    if not lines:
        return -1
    header = lines[0].split()
    for line in lines[1:]:
        fields = line.split()
        if len(fields) >= len(header) and fields[2].startswith('<Link'):
            return int(fields[header.index('Ibytes')])
    return -1


def mib_text(n):
    return f'{n / 1048576:.1f} MiB'


def hms(ts):
    return time.strftime('%H:%M:%S', time.gmtime(ts)) + f'.{int(ts % 1 * 1000):03d}'


def before():
    for name in ['hw.memsize', 'net.inet.tcp.rexmt_slop', 'net.inet.tcp.rtt_min', 'net.inet.tcp.autorcvbufmax']:
        print(subprocess.run(['sysctl', name], capture_output=True, text=True).stdout.strip())
    t = tcp_account()
    sysctl(mib('kern.memacct') + [MEM_ACCT_PEAK, t['index']], 8, struct.pack('<Q', 0))
    print(f"TCP memory: soft {mib_text(t['soft'])}, hard {mib_text(t['hard'])}; peak zeroed")
    print(f'mbuf requests for memory denied: {mbuf_denials()}')
    with open(STATE, 'w') as f:
        json.dump({'start': time.time(), 'netstat': netstat_s(), 'en0': en0_bytes()}, f)


def sample(path):
    with open(path, 'a', buffering=1) as f:
        while True:
            f.write(f"{time.time():.3f} {mbuf_denials()} {tcp_account()['allocated']}\n")
            time.sleep(1)


PACKET = re.compile(r'^(\d+\.\d+) IP6? (\S+)\.(\d+) > (\S+)\.(\d+): Flags \[([^\]]*)\](?:, seq (\d+)(?::(\d+))?)?(?:, ack (\d+))?.*?length (\d+)')


def waits(pcap):
    """The longest-lived connection in the capture: its server-to-client waits for an ACK, retransmissions, and end."""
    out = subprocess.run(['tcpdump', '-n', '-tt', '-S', '-r', pcap], capture_output=True, text=True)
    return analyze(out.stdout), out.stderr.strip()[-300:]


def analyze(text):
    conns = {}
    for line in text.splitlines():
        m = PACKET.match(line)
        if not m:
            continue
        ts, sp, dp, flags = float(m[1]), int(m[3]), int(m[5]), m[6]
        key = tuple(sorted((sp, dp)))
        c = conns.setdefault(key, {'packets': [], 'bytes': {}})
        length = int(m[10])
        c['bytes'][sp] = c['bytes'].get(sp, 0) + length
        c['packets'].append((ts, sp, flags, int(m[7]) if m[7] else None, int(m[8]) if m[8] else None,
                             int(m[9]) if m[9] else None, length))
    if not conns:
        return None
    key, c = max(conns.items(), key=lambda kv: len(kv[1]['packets']))
    server = max(c['bytes'], key=c['bytes'].get)
    sent = []          # (start, end, first-sent time) of each byte range on its first transmission
    snd_max = una = None
    rexmits = 0
    result = {'ports': key, 'server': server, 'packets': len(c['packets']), 'waits': [], 'reset': None, 'unacked_at_end': None}
    for ts, sp, flags, start, end, ack, length in c['packets']:
        if sp == server:
            if 'R' in flags:
                result['reset'] = ts
            if start is not None and end is not None and end > start:
                if snd_max is None:
                    snd_max = start
                if start < snd_max:
                    rexmits += 1
                if end > snd_max:
                    sent.append((max(start, snd_max), end, ts))
                    snd_max = end
        elif ack is not None:
            if una is None or ack > una:
                if una is not None:
                    first = next((t for s, e, t in sent if s <= una < e), None)
                    if first is not None:
                        result['waits'].append((first, ts, ts - first))
                una = ack
    if snd_max is not None and una is not None and snd_max > una:
        first = next((t for s, e, t in sent if s <= una < e), None)
        result['unacked_at_end'] = (first, snd_max - una)
    result['rexmits'] = rexmits
    return result


def after(path, pcap):
    with open(STATE) as f:
        state = json.load(f)
    now = netstat_s()
    received = en0_bytes() - state.get('en0', -1)
    print(f'en0 received during the step: {mib_text(received)}')
    print('netstat -s counters the step changed:')
    for k in sorted(now):
        d = now[k] - state['netstat'].get(k, 0)
        if d:
            print(f'  {d:>10}  {k}')
    rows = []
    try:
        with open(path) as f:
            for line in f:
                parts = line.split()
                if len(parts) == 3:
                    rows.append((float(parts[0]), int(parts[1]), int(parts[2])))
    except FileNotFoundError:
        pass
    t = tcp_account()
    print(f"TCP memory peak {mib_text(t['peak'])} (soft {mib_text(t['soft'])}, hard {mib_text(t['hard'])}); "
          f'{len(rows)} samples')
    if rows:
        print(f'mbuf denials over the samples: {rows[0][1]} -> {rows[-1][1]} (+{rows[-1][1] - rows[0][1]})')

    def denied(a, b):
        inside = [r[1] for r in rows if a <= r[0] <= b + 1]
        return (max(inside) - min(inside)) if inside else None

    result, err = waits(pcap)
    if result is None:
        print(f'no capture to read: {err}')
        return
    w = result['waits']
    long = [x for x in w if x[2] > LONG_WAIT]
    print(f"client-server connection {result['ports']} (server port {result['server']}): {result['packets']} packets, "
          f"{result['rexmits']} retransmitted segments, {len(w)} ACK advances; longest wait {max((x[2] for x in w), default=0):.2f} s; "
          f'waits over {LONG_WAIT:.0f} s: {len(long)}; over {BUDGET:.0f} s: {len([x for x in w if x[2] > BUDGET])}')
    for first, acked, d in long:
        print(f'  {hms(first)} to {hms(acked)}: {d:5.2f} s, mbuf denials during it: {denied(first, acked)}')
    if result['unacked_at_end']:
        first, n = result['unacked_at_end']
        print(f'  UNACKED at the end: {n} bytes, first sent {hms(first) if first else "?"}')
    if result['reset']:
        print(f"  server reset at {hms(result['reset'])}")


if __name__ == '__main__':
    mode = sys.argv[1]
    if mode == 'before':
        before()
    elif mode == 'sample':
        sample(sys.argv[2])
    elif mode == 'after':
        after(sys.argv[2], sys.argv[3])
    else:
        raise SystemExit(f'unknown mode {mode}')
