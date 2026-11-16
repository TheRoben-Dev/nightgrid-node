#!/usr/bin/env python3
"""Sends crafted DHCP frames on one end of a veth pair, for the install test.

Usage (root): dhcp-frames.py IFACE first|rogue

The frames arrive on the other end of the pair as if a DHCP server and a
client were talking on the LAN. Addresses are from 10.99.0.0/24 and MACs
are locally administered, so nothing real is touched.
"""
import socket
import struct
import sys

SERVER_MAC = bytes.fromhex("02005e100001")
ROGUE_MAC = bytes.fromhex("02005e100066")
CLIENT_MAC = bytes.fromhex("02005e100002")


def ip(a):
    return socket.inet_aton(a)


def frame(src_mac, op, yiaddr, opts):
    bootp = bytearray(240)
    bootp[0] = op
    bootp[1] = 1
    bootp[2] = 6
    bootp[16:20] = ip(yiaddr)
    bootp[28:34] = CLIENT_MAC
    bootp[236:240] = bytes([99, 130, 83, 99])
    bootp += bytes(opts) + b"\xff"
    sport, dport = (67, 68) if op == 2 else (68, 67)
    udp = struct.pack("!HHHH", sport, dport, 8 + len(bootp), 0) + bootp
    src = ip("10.99.0.1") if op == 2 else ip("0.0.0.0")
    iph = struct.pack("!BBHHHBBH4s4s", 0x45, 0, 20 + len(udp), 0, 0, 64, 17, 0, src, ip("255.255.255.255"))
    return b"\xff" * 6 + src_mac + b"\x08\x00" + iph + udp


def opt(code, data):
    return [code, len(data)] + list(data)


def offer(server, mac, msg=2):
    o = opt(53, [msg]) + opt(54, ip(server)) + opt(1, ip("255.255.255.0"))
    o += opt(3, ip(server)) + opt(6, ip(server)) + opt(51, struct.pack("!I", 3600))
    return frame(mac, 2, "10.99.0.50", o)


def main():
    iface, which = sys.argv[1], sys.argv[2]
    s = socket.socket(socket.AF_PACKET, socket.SOCK_RAW)
    s.bind((iface, 0))
    if which == "first":
        frames = [
            offer("10.99.0.1", SERVER_MAC),
            frame(CLIENT_MAC, 1, "0.0.0.0", opt(53, [3]) + opt(54, ip("10.99.0.1")) + opt(12, b"ngn-phone")),
            offer("10.99.0.1", SERVER_MAC, msg=5),
        ]
    else:
        frames = [offer("10.99.0.66", ROGUE_MAC)]
    for f in frames:
        s.send(f)


if __name__ == "__main__":
    main()
