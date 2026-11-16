"""Linux 容器里用的 TLS 终止记录代理（LS → 云端的兜底抓包方式）。

背景：LS 的 GetTab 不走 --api_server_url / --inference_api_server_url，而是用从 unleash 特性开关里
拿到的推理地址（实测 inference-us-west-lb.codeium.com），unleash 本身也是直连。macOS 上 Go 用系统钥匙串
验证证书，不改钥匙串就没法中间人；Linux 上 Go 认 SSL_CERT_FILE，所以在容器里跑同版本的 Linux LS：

  1. 生成自签 CA（.lsrun/mitm/），把系统 CA + 自签 CA 合成一个 bundle，通过 SSL_CERT_FILE 交给 LS
  2. 在容器的 /etc/hosts 里把这些主机指到 127.0.0.1（用 sudo，退出时恢复）
  3. 本进程在 127.0.0.1:443 上终止 TLS（按 SNI 签发证书，ALPN 只给 http/1.1），记录后用真实 IP 转发

只在容器里用；不改宿主机任何东西。
"""
import datetime
import os
import socket
import ssl
import subprocess
import threading

import common

MITM_DIR = os.path.join(common.SPIKE_DIR, ".lsrun", "mitm")
HOSTS_MARK = "# zed-fork-spike"
DEFAULT_HOSTS = [
    "server.codeium.com",
    "inference.codeium.com",
    "unleash.codeium.com",
    "inference-us-west-lb.codeium.com",
    "api.codeium.com",
]


def _ca():
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    os.makedirs(MITM_DIR, exist_ok=True)
    kp, cp = os.path.join(MITM_DIR, "ca.key"), os.path.join(MITM_DIR, "ca.pem")
    if os.path.exists(kp) and os.path.exists(cp):
        key = serialization.load_pem_private_key(open(kp, "rb").read(), None)
        cert = x509.load_pem_x509_certificate(open(cp, "rb").read())
        return key, cert
    key = ec.generate_private_key(ec.SECP256R1())
    name = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, "zed-fork-spike local CA")])
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(name).issuer_name(name).public_key(key.public_key())
            .serial_number(x509.random_serial_number()).not_valid_before(now - datetime.timedelta(days=1))
            .not_valid_after(now + datetime.timedelta(days=365))
            .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
            .add_extension(x509.KeyUsage(digital_signature=True, key_cert_sign=True, crl_sign=True, content_commitment=False,
                                         key_encipherment=False, data_encipherment=False, key_agreement=False,
                                         encipher_only=False, decipher_only=False), critical=True)
            .sign(key, hashes.SHA256()))
    fd = os.open(kp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as fh:
        fh.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    with open(cp, "wb") as fh:
        fh.write(cert.public_bytes(serialization.Encoding.PEM))
    return key, cert


def _leaf(host, ca_key, ca_cert):
    from cryptography import x509
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.x509.oid import NameOID

    cp = os.path.join(MITM_DIR, "%s.pem" % host)
    kp = os.path.join(MITM_DIR, "%s.key" % host)
    if os.path.exists(cp) and os.path.exists(kp):
        return cp, kp
    key = ec.generate_private_key(ec.SECP256R1())
    now = datetime.datetime.now(datetime.timezone.utc)
    cert = (x509.CertificateBuilder().subject_name(x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, host)]))
            .issuer_name(ca_cert.subject).public_key(key.public_key()).serial_number(x509.random_serial_number())
            .not_valid_before(now - datetime.timedelta(days=1)).not_valid_after(now + datetime.timedelta(days=90))
            .add_extension(x509.SubjectAlternativeName([x509.DNSName(host)]), critical=False)
            .add_extension(x509.ExtendedKeyUsage([x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]), critical=False)
            .sign(ca_key, hashes.SHA256()))
    with open(cp, "wb") as fh:
        fh.write(cert.public_bytes(serialization.Encoding.PEM))
    fd = os.open(kp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "wb") as fh:
        fh.write(key.private_bytes(serialization.Encoding.PEM, serialization.PrivateFormat.PKCS8, serialization.NoEncryption()))
    return cp, kp


def bundle_path():
    _, ca_cert = _ca()
    out = os.path.join(MITM_DIR, "bundle.pem")
    sys_bundle = open("/etc/ssl/certs/ca-certificates.crt", "rb").read()
    with open(out, "wb") as fh:
        fh.write(sys_bundle + b"\n" + open(os.path.join(MITM_DIR, "ca.pem"), "rb").read())
    return out


def tls_context(hosts):
    ca_key, ca_cert = _ca()
    contexts = {}
    for h in hosts:
        cp, kp = _leaf(h, ca_key, ca_cert)
        c = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        c.load_cert_chain(cp, kp)
        c.set_alpn_protocols(["http/1.1"])
        contexts[h] = c
    base = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    first = hosts[0]
    base.load_cert_chain(*_leaf(first, ca_key, ca_cert))
    base.set_alpn_protocols(["http/1.1"])

    def sni(sock, name, _ctx):
        if name in contexts:
            sock.context = contexts[name]

    base.sni_callback = sni
    return base


# ---- DNS：本进程用真实 IP 转发，LS 通过 /etc/hosts 指到本机 ----

_real = {}
_orig_getaddrinfo = socket.getaddrinfo


def pin_dns(hosts):
    for h in hosts:
        if h in _real:
            continue
        try:
            _real[h] = _orig_getaddrinfo(h, 443, proto=socket.IPPROTO_TCP)[0][4][0]
        except Exception:
            pass

    def gai(host, port, *a, **kw):
        if host in _real:
            host = _real[host]
        return _orig_getaddrinfo(host, port, *a, **kw)

    socket.getaddrinfo = gai
    return dict(_real)


def hosts_add(hosts):
    hosts_restore()
    lines = "".join("127.0.0.1 %s %s\n" % (h, HOSTS_MARK) for h in hosts)
    subprocess.run(["sudo", "sh", "-c", "printf '%%s' '%s' >> /etc/hosts" % lines], check=True)


def hosts_restore():
    cur = open("/etc/hosts").read()
    if HOSTS_MARK not in cur:
        return
    kept = "".join(l for l in cur.splitlines(True) if HOSTS_MARK not in l)
    # /etc/hosts 在容器里常是 bind mount，不能 mv 替换，只能原地写
    p = subprocess.run(["sudo", "tee", "/etc/hosts"], input=kept.encode(), stdout=subprocess.DEVNULL)
    p.check_returncode()


class Mitm:
    def __init__(self, rec, hosts=None):
        from recorder import CloudProxy
        self.hosts = list(hosts or DEFAULT_HOSTS)
        self.real = pin_dns(self.hosts)
        ctx = tls_context(self.hosts)
        self.proxy = CloudProxy(rec, None, "mitm", port=443, tls_ctx=ctx)
        hosts_add(self.hosts)
        rec.write(seg="mitm", dir="start", hosts=self.hosts, real=self.real)

    def stop(self):
        hosts_restore()
