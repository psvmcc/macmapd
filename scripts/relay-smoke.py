"""Exercise the real server through a relay socket on an isolated container network."""
import socket
import sys

server = socket.gethostbyname(sys.argv[1])
with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as probe:
    probe.connect((server, 67))
    relay = probe.getsockname()[0]

with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as sock:
    sock.bind(("0.0.0.0", 67))
    sock.settimeout(5)
    for stage in ("uefi", "ipxe", "os"):
        packet = bytearray(240)
        packet[:3] = bytes([1, 1, 6])
        packet[4:8] = bytes([1, 2, 3, 4])
        packet[24:28] = socket.inet_aton(relay)
        packet[28:34] = bytes.fromhex("aabbccddeeff")
        packet[236:240] = bytes([99, 130, 83, 99])
        packet.extend(bytes([53, 1, 1, 55, 1, 121]))
        if stage != "os":
            packet.extend(bytes([93, 2, 0, 9]))
        if stage == "ipxe":
            packet.extend(bytes([77, 4]) + b"iPXE")
        packet.append(255)
        sock.sendto(packet, (server, 67))
        response, _ = sock.recvfrom(2048)
        assert response[16:20] == socket.inet_aton("10.20.0.10")
        assert response[4:8] == packet[4:8]
        options = {}
        pos = 240
        while response[pos] != 255:
            code = response[pos]
            pos += 1
            if code == 0:
                continue
            length = response[pos]
            pos += 1
            options[code] = options.get(code, b"") + response[pos:pos + length]
            pos += length
        assert options[53] == b"\x02"
        assert options[54] == socket.inet_aton(server), options[54]
        if stage == "uefi":
            assert 3 in options and 121 not in options
            assert options[67] == b"ipxe.efi"
        else:
            assert 121 in options and 3 not in options
            assert options[121] == bytes([8, 10, 10, 20, 0, 1])
            assert (67 in options) == (stage == "ipxe")
print("PASS: Linux wildcard destination metadata, relay replies, UEFI/iPXE/OS routes")
