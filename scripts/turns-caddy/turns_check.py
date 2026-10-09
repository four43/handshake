# TURN over TLS through Caddy into Handshake: Allocate (401 dance), CreatePermission, Send -> UDP echo peer -> Data.
# Run by scripts/turns-caddy/check.sh inside its Docker network; prints the relayed and mapped addresses.
import socket, ssl, struct, os, hmac, hashlib, binascii, base64, time, sys
MAGIC = 0x2112A442
def attr(t, v): return struct.pack("!HH", t, len(v)) + v + b"\0" * (-len(v) % 4)
def msg(mtype, attrs, key=None):
    tid = os.urandom(12); body = b"".join(attrs)
    if key:
        hdr = struct.pack("!HHI", mtype, len(body) + 24, MAGIC) + tid
        body += attr(0x0008, hmac.new(key, hdr + body, hashlib.sha1).digest())
    hdr = struct.pack("!HHI", mtype, len(body) + 8, MAGIC) + tid
    body += attr(0x8028, struct.pack("!I", (binascii.crc32(hdr + body) ^ 0x5354554E) & 0xFFFFFFFF))
    return struct.pack("!HHI", mtype, len(body), MAGIC) + tid + body
def recv(s):
    h = b""
    while len(h) < 20: h += s.recv(20 - len(h))
    t, ln, _ = struct.unpack("!HHI", h[:8]); b = b""
    while len(b) < ln: b += s.recv(ln - len(b))
    a, i = {}, 0
    while i < ln:
        at, l = struct.unpack("!HH", b[i:i+4]); a.setdefault(at, b[i+4:i+4+l]); i += 4 + l + (-l % 4)
    return t, a
def xa(v): return f"{socket.inet_ntoa(struct.pack('!I', struct.unpack('!I', v[4:8])[0] ^ MAGIC))}:{struct.unpack('!H', v[2:4])[0] ^ 0x2112}"
def xenc(ip, port): return struct.pack("!BBH", 0, 1, port ^ 0x2112) + struct.pack("!I", struct.unpack("!I", socket.inet_aton(ip))[0] ^ MAGIC)
ctx = ssl.create_default_context(); ctx.check_hostname = False; ctx.verify_mode = ssl.CERT_NONE
s = ctx.wrap_socket(socket.create_connection(("caddy", 443)), server_hostname="turn.test")
user = f"{int(time.time()) + 600}:game".encode()
pw = base64.b64encode(hmac.new(b"caddy-check-turn-secret", user, hashlib.sha1).digest())
tr = attr(0x0019, bytes([17, 0, 0, 0]))
s.sendall(msg(0x0003, [tr])); t, a = recv(s); assert t == 0x0113, hex(t)
realm, nonce = a[0x0014], a[0x0015]
key = hashlib.md5(user + b":" + realm + b":" + pw).digest()
auth = [attr(0x0006, user), attr(0x0014, realm), attr(0x0015, nonce)]
s.sendall(msg(0x0003, [tr] + auth, key)); t, a = recv(s); assert t == 0x0103, hex(t)
me = socket.gethostbyname(socket.gethostname())
print("allocated over TLS: relayed", xa(a[0x0016]), "| mapped (from PROXY header)", xa(a[0x0020]), "| my address", me)
assert xa(a[0x0020]).split(":")[0] == me, "Handshake should see this client's address, not Caddy's"
echo = socket.gethostbyname("echo")
s.sendall(msg(0x0008, [attr(0x0012, xenc(echo, 9000))] + auth, key)); t, a = recv(s); assert t == 0x0108, hex(t)
s.sendall(msg(0x0016, [attr(0x0012, xenc(echo, 9000)), attr(0x0013, b"hello through caddy")]))
t, a = recv(s); assert t == 0x0017, hex(t)
assert a[0x0013] == b"hello through caddy"
print("data indication from", xa(a[0x0012]), ":", a[0x0013])
