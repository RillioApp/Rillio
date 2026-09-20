"""RMS (dBFS) per second over a span of a 48 kHz stereo s16 WAV timeline.
   python wav-rms.py <wav> <from_s> <to_s>"""
import struct, sys
path, a, b = sys.argv[1], float(sys.argv[2]), float(sys.argv[3])
rate, ch = 48000, 2
with open(path, 'rb') as f:
    f.seek(44 + int(a * rate) * ch * 2)
    for s in range(int(a), int(b)):
        raw = f.read(rate * ch * 2)
        n = len(raw) // 2
        vals = struct.unpack(f'<{n}h', raw)
        rms = (sum(v * v for v in vals) / max(n, 1)) ** 0.5
        db = 20 * __import__('math').log10(rms / 32768) if rms > 0 else -120
        print(f'{s:5d}s {db:7.1f} dBFS')
