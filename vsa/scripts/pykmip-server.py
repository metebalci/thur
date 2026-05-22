#!/usr/bin/env python3
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0

"""Local PyKMIP server for smoke-testing the Thur VSA `kmip` keystore backend.

Generates a fresh CA + server cert + client cert under /tmp/thurvsa-kmip/
(only on first run; subsequent runs reuse them), writes a minimal PyKMIP
server config, and starts the server bound to 127.0.0.1:5696 with mTLS
required. Companion script `pykmip-create-kek.py` provisions an AES-256
KEK and prints its KMIP Unique Identifier — paste that into the
keystore-backends.json entry the daemon (or the kmip_pykmip.rs integration
test) reads.

Run with the user's PyKMIP venv:

    ~/kmip/bin/python vsa/scripts/pykmip-server.py

Server cert SANs cover both `localhost` (DNS) and `127.0.0.1` (IP) so a
rustls client can use either form as the SNI hostname without trust
errors.
"""

from __future__ import annotations

import datetime
import ipaddress
import ssl
import sys
from pathlib import Path

# Compat shim: PyKMIP 0.10 calls the deprecated `ssl.wrap_socket`
# helper, which Python 3.12 deprecated and 3.14 removed entirely
# (https://docs.python.org/3.14/whatsnew/3.14.html#ssl). Substitute
# an SSLContext-based equivalent before importing kmip so the server
# starts on modern Pythons. Drop this shim once PyKMIP cuts a release
# with the upstream fix.
if not hasattr(ssl, "wrap_socket"):
    def _wrap_socket_compat(
        sock,
        keyfile=None,
        certfile=None,
        server_side=False,
        cert_reqs=ssl.CERT_NONE,
        ssl_version=None,
        ca_certs=None,
        do_handshake_on_connect=True,
        suppress_ragged_eofs=True,
        ciphers=None,
    ):
        proto = ssl.PROTOCOL_TLS_SERVER if server_side else ssl.PROTOCOL_TLS_CLIENT
        ctx = ssl.SSLContext(proto)
        # Disable SNI hostname verification before touching
        # verify_mode — the legacy `ssl.wrap_socket` API never did
        # hostname checks; modern SSLContext defaults `check_hostname`
        # to True for client contexts which conflicts with our
        # server_hostname=None wrap call. CA-chain verification still
        # happens via load_verify_locations.
        if not server_side:
            ctx.check_hostname = False
        if certfile:
            ctx.load_cert_chain(certfile, keyfile)
        if ca_certs:
            ctx.load_verify_locations(ca_certs)
        ctx.verify_mode = cert_reqs
        if ciphers:
            ctx.set_ciphers(ciphers)
        return ctx.wrap_socket(
            sock,
            server_side=server_side,
            do_handshake_on_connect=do_handshake_on_connect,
            suppress_ragged_eofs=suppress_ragged_eofs,
        )

    ssl.wrap_socket = _wrap_socket_compat  # type: ignore[attr-defined]

from cryptography import x509
from cryptography.hazmat.primitives import hashes, serialization
from cryptography.hazmat.primitives.asymmetric import rsa
from cryptography.x509.oid import NameOID

from kmip.services.server import KmipServer

CERT_DIR = Path("/tmp/thurvsa-kmip")
HOSTNAME = "127.0.0.1"
PORT = 5696


def _now() -> datetime.datetime:
    # cryptography 42+ wants timezone-aware naive-UTC datetimes for cert
    # validity windows; older releases accept either.
    return datetime.datetime.now(datetime.timezone.utc)


def _gen_keypair() -> rsa.RSAPrivateKey:
    return rsa.generate_private_key(public_exponent=65537, key_size=2048)


def _write_key(key: rsa.RSAPrivateKey, path: Path) -> None:
    path.write_bytes(
        key.private_bytes(
            encoding=serialization.Encoding.PEM,
            format=serialization.PrivateFormat.TraditionalOpenSSL,
            encryption_algorithm=serialization.NoEncryption(),
        )
    )
    path.chmod(0o600)


def _write_cert(cert: x509.Certificate, path: Path) -> None:
    path.write_bytes(cert.public_bytes(serialization.Encoding.PEM))


def _gen_ca() -> tuple[x509.Certificate, rsa.RSAPrivateKey]:
    key = _gen_keypair()
    subject = issuer = x509.Name(
        [x509.NameAttribute(NameOID.COMMON_NAME, "thurvsa-kmip-test-ca")]
    )
    cert = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(issuer)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(_now())
        .not_valid_after(_now() + datetime.timedelta(days=365))
        .add_extension(x509.BasicConstraints(ca=True, path_length=None), critical=True)
        .add_extension(
            x509.KeyUsage(
                digital_signature=True,
                content_commitment=False,
                key_encipherment=False,
                data_encipherment=False,
                key_agreement=False,
                key_cert_sign=True,
                crl_sign=True,
                encipher_only=False,
                decipher_only=False,
            ),
            critical=True,
        )
        .sign(key, hashes.SHA256())
    )
    return cert, key


def _gen_leaf(
    cn: str,
    ca_cert: x509.Certificate,
    ca_key: rsa.RSAPrivateKey,
    san_dns: list[str] | None = None,
    san_ips: list[str] | None = None,
    client_auth: bool = False,
) -> tuple[x509.Certificate, rsa.RSAPrivateKey]:
    key = _gen_keypair()
    subject = x509.Name([x509.NameAttribute(NameOID.COMMON_NAME, cn)])

    sans = []
    for name in san_dns or []:
        sans.append(x509.DNSName(name))
    for ip in san_ips or []:
        sans.append(x509.IPAddress(ipaddress.ip_address(ip)))

    eku = x509.ExtendedKeyUsage(
        [x509.oid.ExtendedKeyUsageOID.CLIENT_AUTH]
        if client_auth
        else [x509.oid.ExtendedKeyUsageOID.SERVER_AUTH]
    )

    builder = (
        x509.CertificateBuilder()
        .subject_name(subject)
        .issuer_name(ca_cert.subject)
        .public_key(key.public_key())
        .serial_number(x509.random_serial_number())
        .not_valid_before(_now())
        .not_valid_after(_now() + datetime.timedelta(days=365))
        .add_extension(x509.BasicConstraints(ca=False, path_length=None), critical=True)
        .add_extension(eku, critical=False)
    )
    if sans:
        builder = builder.add_extension(x509.SubjectAlternativeName(sans), critical=False)
    cert = builder.sign(ca_key, hashes.SHA256())
    return cert, key


def gen_certs_if_missing() -> None:
    CERT_DIR.mkdir(parents=True, exist_ok=True)
    ca_crt = CERT_DIR / "ca.crt"
    if ca_crt.exists():
        print(f"[pykmip-server] reusing existing certs at {CERT_DIR}/", flush=True)
        return

    print(f"[pykmip-server] generating fresh CA + server + client certs at {CERT_DIR}/", flush=True)
    ca_cert, ca_key = _gen_ca()
    _write_cert(ca_cert, ca_crt)
    _write_key(ca_key, CERT_DIR / "ca.key")

    server_cert, server_key = _gen_leaf(
        cn="thurvsa-kmip-test-server",
        ca_cert=ca_cert,
        ca_key=ca_key,
        san_dns=["localhost"],
        san_ips=[HOSTNAME],
        client_auth=False,
    )
    _write_cert(server_cert, CERT_DIR / "server.crt")
    _write_key(server_key, CERT_DIR / "server.key")

    client_cert, client_key = _gen_leaf(
        cn="thurvsa-kmip-test-client",
        ca_cert=ca_cert,
        ca_key=ca_key,
        client_auth=True,
    )
    _write_cert(client_cert, CERT_DIR / "client.crt")
    _write_key(client_key, CERT_DIR / "client.key")


def write_config() -> Path:
    # PyKMIP 0.10's `KmipServer` driver reads the [server] section of an
    # INI file. Documented at:
    #   https://pykmip.readthedocs.io/en/latest/server.html#configuration
    # The auth_suite and tls_cipher_suites keys are intentionally absent
    # — defaults (TLS 1.2, broad cipher list) work for our smoke test.
    config_path = CERT_DIR / "server.conf"
    config_path.write_text(
        f"""[server]
hostname={HOSTNAME}
port={PORT}
certificate_path={CERT_DIR}/server.crt
key_path={CERT_DIR}/server.key
ca_path={CERT_DIR}/ca.crt
auth_suite=TLS1.2
enable_tls_client_auth=True
logging_level=INFO
database_path={CERT_DIR}/pykmip.db
"""
    )
    return config_path


def main() -> int:
    gen_certs_if_missing()
    config_path = write_config()

    print(
        f"[pykmip-server] starting on {HOSTNAME}:{PORT} (mTLS required, "
        f"certs at {CERT_DIR}/)",
        flush=True,
    )

    # `log_path` is a *constructor* arg (not read from the config
    # file) — PyKMIP defaults it to /var/log/pykmip/server.log which
    # needs root. Point it at our cert dir so the smoke runs as the
    # invoking user.
    log_dir = CERT_DIR / "log"
    log_dir.mkdir(parents=True, exist_ok=True)
    server = KmipServer(
        config_path=str(config_path),
        log_path=str(log_dir / "server.log"),
    )
    server.start()
    print(f"[pykmip-server] ready — Ctrl-C to stop", flush=True)
    try:
        server.serve()
    except KeyboardInterrupt:
        print("\n[pykmip-server] shutting down", flush=True)
    finally:
        server.stop()
    return 0


if __name__ == "__main__":
    sys.exit(main())
