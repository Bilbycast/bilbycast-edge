#!/usr/bin/env python3
"""Dual-destination RTP sender for testing SMPTE 2022-7 hitless switching.

Sends identical RTP packets (same seq/ts) to two UDP destinations simultaneously,
simulating a redundant source feeding two SRT paths.
"""

import socket
import struct
import time
import sys

PACKETS_PER_SEC = 30
PAYLOAD_SIZE = 1316  # 7 x 188 bytes (MPEG-TS packets)
DURATION_SECS = 60
SSRC = 0x12345678
CLOCK_RATE = 90000

def build_rtp_packet(seq, timestamp, ssrc, payload):
    header = struct.pack(
        "!BBHII",
        0x80,       # V=2, P=0, X=0, CC=0
        96,         # M=0, PT=96
        seq & 0xFFFF,
        timestamp & 0xFFFFFFFF,
        ssrc,
    )
    return header + payload

def main():
    dest1_port = int(sys.argv[1]) if len(sys.argv) > 1 else 6000
    dest2_port = int(sys.argv[2]) if len(sys.argv) > 2 else 6001
    duration = int(sys.argv[3]) if len(sys.argv) > 3 else DURATION_SECS
    dest_ip = "127.0.0.1"

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    payload = bytes(range(256)) * (PAYLOAD_SIZE // 256) + bytes(range(PAYLOAD_SIZE % 256))

    seq = 0
    ts = 0
    ts_increment = CLOCK_RATE // PACKETS_PER_SEC
    interval = 1.0 / PACKETS_PER_SEC

    total_packets = duration * PACKETS_PER_SEC
    print(f"Sending {total_packets} RTP packets to {dest_ip}:{dest1_port} AND "
          f"{dest_ip}:{dest2_port} at {PACKETS_PER_SEC} pps for {duration}s...")

    start = time.monotonic()
    for i in range(total_packets):
        pkt = build_rtp_packet(seq, ts, SSRC, payload)
        sock.sendto(pkt, (dest_ip, dest1_port))
        sock.sendto(pkt, (dest_ip, dest2_port))
        seq = (seq + 1) & 0xFFFF
        ts += ts_increment

        target_time = start + (i + 1) * interval
        sleep_time = target_time - time.monotonic()
        if sleep_time > 0:
            time.sleep(sleep_time)

        if (i + 1) % (PACKETS_PER_SEC * 5) == 0:
            elapsed = time.monotonic() - start
            print(f"  Sent {i + 1}/{total_packets} packets ({elapsed:.1f}s elapsed)")

    elapsed = time.monotonic() - start
    print(f"Done: sent {total_packets} packets in {elapsed:.1f}s "
          f"({total_packets / elapsed:.1f} pps)")
    sock.close()

if __name__ == "__main__":
    main()
