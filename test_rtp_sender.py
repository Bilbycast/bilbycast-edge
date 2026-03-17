#!/usr/bin/env python3
"""Simple RTP packet sender for testing srtedge traffic flow.

Sends valid RTP packets (V=2, PT=96, incrementing seq/ts) to a UDP destination
at ~30 packets/second. Each packet has a 1316-byte payload (standard MPEG-TS
over RTP payload size of 7 TS packets).
"""

import socket
import struct
import time
import sys

DEST_IP = "127.0.0.1"
DEST_PORT = 5000
PACKETS_PER_SEC = 30
PAYLOAD_SIZE = 1316  # 7 x 188 bytes (MPEG-TS packets)
DURATION_SECS = 30
SSRC = 0x12345678
CLOCK_RATE = 90000  # 90kHz RTP clock for video

def build_rtp_packet(seq, timestamp, ssrc, payload):
    """Build a minimal RTP packet (RFC 3550)."""
    # V=2, P=0, X=0, CC=0 -> 0x80
    # M=0, PT=96 -> 0x60
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
    dest_ip = sys.argv[1] if len(sys.argv) > 1 else DEST_IP
    dest_port = int(sys.argv[2]) if len(sys.argv) > 2 else DEST_PORT
    duration = int(sys.argv[3]) if len(sys.argv) > 3 else DURATION_SECS

    sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    payload = bytes(range(256)) * (PAYLOAD_SIZE // 256) + bytes(range(PAYLOAD_SIZE % 256))

    seq = 0
    ts = 0
    ts_increment = CLOCK_RATE // PACKETS_PER_SEC  # 3000 per packet at 30fps
    interval = 1.0 / PACKETS_PER_SEC

    total_packets = duration * PACKETS_PER_SEC
    print(f"Sending {total_packets} RTP packets to {dest_ip}:{dest_port} "
          f"at {PACKETS_PER_SEC} pps for {duration}s...")

    start = time.monotonic()
    for i in range(total_packets):
        pkt = build_rtp_packet(seq, ts, SSRC, payload)
        sock.sendto(pkt, (dest_ip, dest_port))
        seq = (seq + 1) & 0xFFFF
        ts += ts_increment

        # Pace to target rate
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
