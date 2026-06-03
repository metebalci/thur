#!/bin/sh
# Thur container entrypoint.
#
# Bare invocation launches the product daemon selected at image build time
# (THUR_PRODUCT = thurvtl | thurvsa), reading its config from
# /etc/<product>/<product>.yaml unless THUR_CONFIG overrides it. Any
# argument is treated as a CLI invocation instead, so
# `podman run --rm <image> config defaults` works without a mounted config.
set -eu

PRODUCT="${THUR_PRODUCT:?THUR_PRODUCT not set — rebuild with --build-arg PRODUCT=thurvtl|thurvsa}"
CONFIG="${THUR_CONFIG:-/etc/${PRODUCT}/${PRODUCT}.yaml}"

# Admin socket lives at /run/<product>/admin.sock. systemd uses
# RuntimeDirectory=; in a container we ensure the dir exists each start
# (it may be a tmpfs that doesn't survive a restart).
mkdir -p "/run/${PRODUCT}" 2>/dev/null || true

# Argument present -> run the CLI (no config-file requirement, e.g.
# `config defaults`, `config completion bash`). Bare -> launch the daemon.
if [ "$#" -gt 0 ]; then
    exec "${PRODUCT}" "$@"
fi

if [ ! -f "${CONFIG}" ]; then
    echo "thur: no config at ${CONFIG}" >&2
    echo "thur: mount your ${PRODUCT}.yaml there, or set THUR_CONFIG." >&2
    echo "thur: print a fully-annotated starter with:" >&2
    echo "      podman run --rm <image> config defaults" >&2
    exit 1
fi

exec "${PRODUCT}d" --config "${CONFIG}"
