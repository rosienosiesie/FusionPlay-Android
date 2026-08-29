#!/usr/bin/env python3
"""Generate ALAC golden-vector fixtures using macOS afconvert (Apple's reference encoder).

Pipeline:  synth PCM -> WAV -> afconvert -d alac -f caff -> parse CAF -> fixture.

ALAC is lossless, so the *expected* decoded PCM is exactly the PCM we feed the
encoder. We commit (cookie + compressed frames + expected PCM); the decoder under
test must reproduce the PCM bit-for-bit.

Fixture format (all multi-byte little-endian):
    magic   "ALACGV01"            (8 bytes)
    u32     sample_rate
    u8      channels
    u8      bit_depth
    u16     reserved (0)
    u32     cookie_len ; cookie bytes      (the 48-byte block set_info() wants)
    u32     n_frames
    repeat: u32 frame_len ; frame bytes
    u32     pcm_len ; pcm bytes (interleaved signed 16-bit LE)
"""
import math
import os
import struct
import subprocess
import sys
import tempfile
import wave

SR = 44100
OUT_DIR = sys.argv[1] if len(sys.argv) > 1 else "."


def clamp16(x):
    return max(-32768, min(32767, int(round(x * 32767.0))))


def synth_stereo():
    N = 3 * 4096 + 1500  # not a multiple of 4096 -> partial final frame
    T = N / SR
    f0, f1 = 300.0, 6000.0
    L = []
    for n in range(N):
        t = n / SR
        phase = 2 * math.pi * (f0 * t + (f1 - f0) / (2 * T) * t * t)
        env = 0.20 + 0.15 * math.sin(2 * math.pi * 1.5 * t)
        L.append(env * math.sin(phase))
    inter = []
    for n in range(N):
        t = n / SR
        l = L[n]
        r = 0.6 * L[n - 5] + 0.10 * math.sin(2 * math.pi * 440.0 * t) if n >= 5 else l
        inter.append(clamp16(l))
        inter.append(clamp16(r))
    return 2, inter


def synth_mono():
    N = 2 * 4096 + 777  # partial final frame
    inter = []
    for n in range(N):
        t = n / SR
        env = 0.25 + 0.10 * math.sin(2 * math.pi * 2.0 * t)
        s = env * (math.sin(2 * math.pi * 440.0 * t) + 0.5 * math.sin(2 * math.pi * 1567.0 * t))
        inter.append(clamp16(s))
    return 1, inter


def write_wav(path, channels, samples_i16):
    with wave.open(path, "wb") as w:
        w.setnchannels(channels)
        w.setsampwidth(2)
        w.setframerate(SR)
        w.writeframes(struct.pack("<%dh" % len(samples_i16), *samples_i16))


def read_caf_chunks(data):
    assert data[:4] == b"caff", "not a CAF file"
    off = 8  # caff + version(2) + flags(2)
    chunks = []
    while off + 12 <= len(data):
        ctype = data[off:off + 4]
        size = struct.unpack(">q", data[off + 4:off + 12])[0]
        off += 12
        if size < 0:  # data chunk can have -1 (to EOF)
            size = len(data) - off
        chunks.append((ctype, data[off:off + size]))
        off += size
    return chunks


def parse_pakt(payload):
    # mNumberPackets(s64), mNumberValidFrames(s64), mPrimingFrames(s32), mRemainderFrames(s32)
    n_pkts, n_valid = struct.unpack(">qq", payload[:16])
    priming, remainder = struct.unpack(">ii", payload[16:24])
    off = 24
    sizes = []
    for _ in range(n_pkts):
        val = 0
        while True:
            b = payload[off]; off += 1
            val = (val << 7) | (b & 0x7F)
            if not (b & 0x80):
                break
        sizes.append(val)
    return n_pkts, n_valid, priming, remainder, sizes


def normalize_cookie(kuki):
    """Return the 48-byte block set_info() expects: frma-atom(12)+alac-atom-hdr(12)+config(24)."""
    # Locate the 24-byte ALACSpecificConfig regardless of wrapper variant.
    if len(kuki) >= 24 and kuki[4:8] == b"frma":
        cfg = kuki[24:48]          # frma(12) + alac hdr(12) + config(24)
    elif len(kuki) >= 12 and kuki[4:8] == b"alac":
        cfg = kuki[12:36]          # alac hdr(12) + config(24)
    else:
        cfg = kuki[:24]            # bare ALACSpecificConfig
    assert len(cfg) == 24, "could not locate 24-byte ALACSpecificConfig (kuki len=%d)" % len(kuki)
    block = struct.pack(">I", 12) + b"frma" + b"alac"
    block += struct.pack(">I", 36) + b"alac" + struct.pack(">I", 0)
    block += cfg
    assert len(block) == 48
    return block


def gen(name, channels, samples):
    tmp = tempfile.mkdtemp()
    wav = os.path.join(tmp, "in.wav")
    caf = os.path.join(tmp, "out.caf")
    write_wav(wav, channels, samples)
    subprocess.run(["afconvert", "-d", "alac", "-f", "caff", wav, caf], check=True)

    chunks = dict()
    raw = open(caf, "rb").read()
    for ctype, payload in read_caf_chunks(raw):
        chunks[ctype] = payload

    kuki = normalize_cookie(chunks[b"kuki"])
    n_pkts, n_valid, priming, remainder, sizes = parse_pakt(chunks[b"pakt"])
    data = chunks[b"data"][4:]  # skip mEditCount(u32)

    frames = []
    off = 0
    for sz in sizes:
        frames.append(data[off:off + sz]); off += sz

    print(f"[{name}] channels={channels} pcm_frames={len(samples)//channels} "
          f"packets={n_pkts} valid={n_valid} priming={priming} remainder={remainder} "
          f"kuki_in={len(chunks[b'kuki'])} cookie_out={len(kuki)} frame_sizes={sizes}")

    pcm = struct.pack("<%dh" % len(samples), *samples)
    out = bytearray()
    out += b"ALACGV01"
    out += struct.pack("<IBBH", SR, channels, 16, 0)
    out += struct.pack("<I", len(kuki)) + kuki
    out += struct.pack("<I", len(frames))
    for f in frames:
        out += struct.pack("<I", len(f)) + f
    out += struct.pack("<I", len(pcm)) + pcm

    path = os.path.join(OUT_DIR, name + ".alac")
    open(path, "wb").write(out)
    print(f"[{name}] wrote {path} ({len(out)} bytes)")


os.makedirs(OUT_DIR, exist_ok=True)
gen("stereo", *synth_stereo())
gen("mono", *synth_mono())
