"""Throwaway proof (exp/ branch only): TCP's memory account and drop counters around the macOS build.

  measure.py before          print the limits and counters, zero TCP's peak, save the counters
  measure.py sample <file>   append "epoch allocated" for TCP's account to <file> every 250 ms, forever
  measure.py after <file>    print the counters' change, TCP's peak, the time spent over each limit,
                             and the kernel's own "memacct" log lines since `before`

Run as root (sudo): zeroing the peak writes a sysctl, and `log show` may need it.
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
COUNTERS = {
    'memdrop': re.compile(r'(\d+) received packets? dropped due to low memory'),
    'timeoutdrop': re.compile(r'(\d+) connections? dropped by rexmit timeout'),
    'rstchallenge': re.compile(r'(\d+) challenge ACKs? sent due to unexpected RST'),
    'rexmttimeo': re.compile(r'(\d+) retransmit timeouts?'),
    'badrst': re.compile(r'(\d+) bad resets?'),
}

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


def accounts():
    raw = sysctl(mib('kern.memacct') + [MEM_ACCT_ALL_STATISTICS, 0], STATS.size * 8)
    out = []
    for i in range(len(raw) // STATS.size):
        peak, allocated, soft, hard, name = STATS.unpack_from(raw, i * STATS.size)
        out.append({'index': i, 'name': name.split(b'\0')[0].decode(), 'peak': peak,
                    'allocated': allocated, 'soft': soft, 'hard': hard})
    return out


def tcp():
    return next(a for a in accounts() if a['name'] == 'TCP')


def counters():
    text = subprocess.run(['netstat', '-s', '-p', 'tcp'], capture_output=True, text=True, check=True).stdout
    found = {}
    for key, pattern in COUNTERS.items():
        m = pattern.search(text)
        found[key] = int(m[1]) if m else None
    return found


def mib_text(n):
    return f'{n / 1048576:.1f} MiB'


def show_accounts():
    for a in accounts():
        print(f"  {a['name']:<8} allocated {mib_text(a['allocated'])}, peak {mib_text(a['peak'])}, "
              f"soft {mib_text(a['soft'])}, hard {mib_text(a['hard'])}")


def before():
    for name in ['hw.memsize', 'net.inet.tcp.autorcvbufmax', 'net.inet.tcp.autosndbufmax', 'kern.ipc.maxsockbuf']:
        print(subprocess.run(['sysctl', name], capture_output=True, text=True).stdout.strip())
    print('memory accounts before:')
    show_accounts()
    t = tcp()
    sysctl(mib('kern.memacct') + [MEM_ACCT_PEAK, t['index']], 8, struct.pack('<Q', 0))
    print(f"TCP peak zeroed; now {mib_text(tcp()['peak'])}")
    c = counters()
    print('tcp counters before:', c)
    with open(STATE, 'w') as f:
        json.dump({'start': time.time(), 'counters': c}, f)


def sample(path):
    with open(path, 'a', buffering=1) as f:
        while True:
            f.write(f"{time.time():.3f} {tcp()['allocated']}\n")
            time.sleep(0.25)


def spans(rows, limit):
    out, start = [], None
    for ts, v in rows:
        if v > limit and start is None:
            start = ts
        elif v <= limit and start is not None:
            out.append((start, ts))
            start = None
    if start is not None:
        out.append((start, rows[-1][0]))
    return out


def hms(ts):
    return time.strftime('%H:%M:%S', time.gmtime(ts)) + f'.{int(ts % 1 * 1000):03d}'


def after(path):
    with open(STATE) as f:
        state = json.load(f)
    c = counters()
    print('tcp counters after:', c)
    print('change over the build:', {k: (c[k] - state['counters'][k]) if c[k] is not None and state['counters'][k] is not None else None for k in c})
    print('memory accounts after:')
    show_accounts()
    t = tcp()
    rows = []
    try:
        with open(path) as f:
            for line in f:
                parts = line.split()
                if len(parts) == 2:
                    rows.append((float(parts[0]), int(parts[1])))
    except FileNotFoundError:
        pass
    print(f'{len(rows)} samples of TCP allocated; max {mib_text(max((v for _, v in rows), default=0))}; '
          f"peak since before {mib_text(t['peak'])}; soft {mib_text(t['soft'])}; hard {mib_text(t['hard'])}")
    for label, limit in [('soft', t['soft']), ('hard', t['hard'])]:
        s = spans(rows, limit)
        total = sum(b - a for a, b in s)
        print(f'over the {label} limit: {len(s)} spans, {total:.1f} s in all')
        for a, b in s[:60]:
            print(f'  {hms(a)} to {hms(b)} ({b - a:.2f} s)')
    start = time.strftime('%Y-%m-%d %H:%M:%S', time.localtime(state['start']))
    log = subprocess.run(['log', 'show', '--style', 'compact', '--start', start,
                          '--predicate', 'eventMessage CONTAINS "memacct"'], capture_output=True, text=True)
    lines = [l for l in log.stdout.splitlines() if 'memacct' in l]
    print(f'kernel memacct log lines since before: {len(lines)}')
    for l in lines[:200]:
        print(' ', l)
    if log.returncode != 0:
        print('log show failed:', log.stderr.strip()[-500:])


if __name__ == '__main__':
    mode = sys.argv[1]
    if mode == 'before':
        before()
    elif mode == 'sample':
        sample(sys.argv[2])
    elif mode == 'after':
        after(sys.argv[2])
    else:
        raise SystemExit(f'unknown mode {mode}')
