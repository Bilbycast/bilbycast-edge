#!/usr/bin/env python3
# Generator for eac3_5_1_plus_dependent_48k.ec3, the E-AC-3 7.1 fixture of
# the AU-cutter / splitter tests (engine::audio_au, engine::audio_decode).
#
# Seven 32 ms time slots, each a 5.1 E-AC-3 independent frame (substream 0,
# 768 bytes) followed by a 2/0 E-AC-3 dependent substream 0 frame (128 bytes,
# ETSI TS 102 366 E.1.2) that extends it with the Lw/Rw pair: an 8-channel
# programme, FL FR FC LFE SL SR WL WR. The core comes from FFmpeg's eac3
# encoder (a 300 / 400 / 500 / 60 / 600 / 700 Hz tone per channel):
#
#   ffmpeg -f lavfi -i "aevalsrc=0.3*sin(2*PI*300*t)|0.3*sin(2*PI*400*t)|\
#     0.3*sin(2*PI*500*t)|0.1*sin(2*PI*60*t)|0.3*sin(2*PI*600*t)|\
#     0.3*sin(2*PI*700*t):s=48000:c=5.1(side):d=0.2" \
#     -c:a eac3 -b:a 192k -f eac3 core.ec3
#
# FFmpeg's encoder writes no dependent substreams, so each dependent frame
# is built here bit by bit: D45 exponents near the top of their range and
# the SNR offsets at their floor, so every bit allocation is 0 and the frame
# carries no mantissas; its two channels decode to dithered silence.
#
#   python3 mk_eac3_7_1.py core.ec3 eac3_5_1_plus_dependent_48k.ec3
import sys

def frames(b):
    i = 0
    out = []
    while i + 4 <= len(b):
        assert b[i] == 0x0B and b[i + 1] == 0x77
        n = ((((b[i + 2] & 7) << 8) | b[i + 3]) + 1) * 2
        out.append(b[i:i + n])
        i += n
    return out

def to_bytes(bits):
    return bytes(int(bits[i:i + 8], 2) for i in range(0, len(bits), 8))

def dependent(words=64):
    b = '0000101101110111'           # syncword
    b += '01' + '000' + '0' * 11     # strmtyp 1 (dependent), substreamid 0, frmsiz (below)
    b += '00' + '11' + '010' + '0'   # 48 kHz, 6 blocks, acmod 2/0, no LFE
    b += '10000' + '11111' + '0'     # bsid 16, dialnorm -31, compre 0
    b += '1' + f'{1 << (15 - 10):016b}'  # chanmape; chanmap: location 10, the Lw/Rw pair
    b += '0' + '0' + '0'             # mixmdate, infomdate, addbsie
    b += '1' + '0'                   # expstre (per-block strategies), ahte
    b += '00' + '0' * 8              # snroffststr 0; transproce .. spxattene off
    b += '0' + '0' * 5               # coupling: blk0 cplinu 0, blk1..5 cplstre 0
    for blk in range(6):             # D45 in block 0, reuse after
        b += ('11' if blk == 0 else '00') * 2
    b += '000000' + '0000' + '0'     # csnroffst 0, fsnroffst 0, blkstrtinfoe 0
    for blk in range(6):
        b += '0' + '0'               # dynrnge; spx in use / strategy
        if blk == 0:
            b += '0000'              # rematrixing flags (4 bands)
            b += '000000' * 2        # bandwidth codes: end frequency 73
            for _ in range(2):       # exponents: 15, +6, +3, then flat at 24
                b += '1111'
                for g in [124, 93, 62, 62, 62, 62]:
                    b += f'{g:07b}'
                b += '00'            # gainrng
        else:
            b += '0'                 # rematstr
    b += '0' * ((-len(b)) % 16)
    assert len(b) // 16 <= words
    b += '0' * (words * 16 - len(b))
    return to_bytes(b[:21] + f'{words - 1:011b}' + b[32:])

core = frames(open(sys.argv[1], 'rb').read())
dep = dependent()
open(sys.argv[2], 'wb').write(b''.join(c + dep for c in core))
