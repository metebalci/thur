#!/usr/bin/env python3
# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0

"""Provision an AES-256 KEK on the local PyKMIP server.

Connects with the client cert minted by `pykmip-server.py`, creates a
fresh AES-256 symmetric key with Encrypt+Decrypt usage, activates it,
prints its Unique Identifier on stdout. Everything else (logs, hints)
goes to stderr so the UID can be captured with command substitution:

    KEK_UID=$(~/kmip/bin/python vsa/scripts/pykmip-create-kek.py)

The matching `keystore-backends.json` entry then looks like:

    {
      "type": "kmip",
      "endpoint": "127.0.0.1:5696",
      "kek_uid": "<KEK_UID>",
      "server_name": "localhost",
      "ca_bundle": { "type": "path", "path": "/tmp/thurvsa-kmip/ca.crt" },
      "mtls": {
        "type": "client_cert",
        "cert_path": "/tmp/thurvsa-kmip/client.crt",
        "key_path":  "/tmp/thurvsa-kmip/client.key"
      }
    }
"""

from __future__ import annotations

import ssl
import sys
from pathlib import Path

# Same Python-3.14 `ssl.wrap_socket` compat shim as pykmip-server.py.
# Both the client and the server in PyKMIP 0.10 call the deprecated
# helper; drop this once the upstream release adds SSLContext support.
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

from kmip.core.enums import CryptographicAlgorithm, CryptographicUsageMask
from kmip.pie.client import ProxyKmipClient

CERT_DIR = Path("/tmp/thurvsa-kmip")
HOSTNAME = "127.0.0.1"
PORT = 5696


def main() -> int:
    if not (CERT_DIR / "client.crt").exists():
        print(
            f"[pykmip-create-kek] error: client cert not found at {CERT_DIR}/client.crt — "
            f"start pykmip-server.py first to generate certs",
            file=sys.stderr,
        )
        return 1

    print(f"[pykmip-create-kek] connecting to {HOSTNAME}:{PORT}", file=sys.stderr)
    with ProxyKmipClient(
        hostname=HOSTNAME,
        port=PORT,
        cert=str(CERT_DIR / "client.crt"),
        key=str(CERT_DIR / "client.key"),
        ca=str(CERT_DIR / "ca.crt"),
        # Match the rustls SNI hostname our backend derives from the
        # endpoint host portion. "localhost" is in the server cert's
        # SAN so cert verification passes either way.
        config_file=None,
    ) as client:
        uid = client.create(
            CryptographicAlgorithm.AES,
            256,
            cryptographic_usage_mask=[
                CryptographicUsageMask.ENCRYPT,
                CryptographicUsageMask.DECRYPT,
            ],
        )
        # KMIP managed objects start in `Pre-Active`. Encrypt/Decrypt
        # require `Active` state; activate explicitly.
        client.activate(uid)
        print(f"[pykmip-create-kek] created + activated AES-256 KEK uid={uid}", file=sys.stderr)
        # UID on stdout — captureable with `$(...)`.
        print(uid)
    return 0


if __name__ == "__main__":
    sys.exit(main())
