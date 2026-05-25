# Copyright (c) 2026 Mete Balci
# SPDX-License-Identifier: Apache-2.0
#
# Shared helpers for the Monte Carlo random-op test harnesses:
#   - vsa/scripts/test-monte-carlo.sh
#   - vtl/scripts/test-monte-carlo.sh
#
# Sourced — not executed. Caller is expected to have already sourced
# scripts/lib/test-helpers.sh (we reuse log_info / log_error from there).
#
# Reproducibility contract: every random choice flows from $MC_SEED via
# the per-op counter $MC_OP_INDEX. Re-running with the same --seed gives
# byte-identical op sequence and content. Print the seed at start AND on
# failure so the user can re-run.

# Initialize MC_SEED from --seed N or by drawing from /dev/urandom.
# Caller passes through whatever was on the command line; empty means
# "pick fresh". Sets globals MC_SEED, MC_OP_INDEX, MC_OP_LOG.
mc_seed_init() {
    local cli_seed="$1"
    local log_path="$2"
    if [[ -n "$cli_seed" ]]; then
        MC_SEED="$cli_seed"
    else
        # 64-bit unsigned, plenty of entropy without dragging in awk.
        MC_SEED=$(od -An -N8 -tu8 /dev/urandom | tr -d ' \n')
    fi
    MC_OP_INDEX=0
    MC_OP_LOG="$log_path"
    : > "$MC_OP_LOG"
    echo ""
    echo "========================================"
    echo "Monte Carlo seed: $MC_SEED"
    echo "Re-run with: --seed $MC_SEED"
    echo "========================================"
    echo ""
}

# Per-op deterministic RNG: blake3(seed || index || tag) -> 64 hex chars.
# Different `tag` values give independent streams for op-pick vs size-pick
# vs content-seed without state-sharing between them.
mc_rng_hex() {
    local tag="$1"
    printf '%s|%s|%s' "$MC_SEED" "$MC_OP_INDEX" "$tag" \
        | b3sum --no-names 2>/dev/null \
        || printf '%s|%s|%s' "$MC_SEED" "$MC_OP_INDEX" "$tag" | sha256sum | awk '{print $1}'
}

# Convert the leading 8 hex chars of mc_rng_hex into a uint32 in [0, mod).
# 8 chars = 32 bits is plenty for picker domains (weights sum to 100,
# size buckets max ~32 MiB).
mc_rng_u32() {
    local tag="$1" mod="$2"
    local hex
    hex=$(mc_rng_hex "$tag")
    # printf %d on a 0x-prefixed 8-hex is a 32-bit int; bash arithmetic
    # is 64-bit signed so the modulo is safe.
    local n=$((16#${hex:0:8}))
    echo $(( n % mod ))
}

# Weighted picker. Caller passes pairs as `weight:name weight:name ...`.
# Weights must sum to 100. Returns the picked name on stdout.
#
# Usage:
#   op=$(mc_pick_weighted op \
#       "22:write_new" "14:overwrite" "14:append" "24:read_verify" \
#       "8:delete" "4:truncate" "4:sync" "6:umount_cycle" "4:logout_cycle")
mc_pick_weighted() {
    local tag="$1"; shift
    local roll
    roll=$(mc_rng_u32 "$tag" 100)
    local cum=0 weight name
    for pair in "$@"; do
        weight="${pair%%:*}"
        name="${pair#*:}"
        cum=$(( cum + weight ))
        if (( roll < cum )); then
            echo "$name"
            return 0
        fi
    done
    # Defensive: weights must sum to 100. If they don't, return the last
    # name so the caller still gets *something* and we surface the bug
    # via a warn rather than wedging.
    echo "$name"
    log_warn "mc_pick_weighted: weights summed to $cum (expected 100) — last bucket leaked"
}

# Boundary-biased size picker (bytes). Buckets target the VSA page-cache
# + chunk-pool boundary cases that handcrafted tests miss:
#
#   18 sub-sector       1 B .. 4 KiB-1
#   12 exact boundaries 4096 / 65535 / 65536 / 65537 / 131072
#   20 sub-page         4 KiB .. 64 KiB-1
#   25 1-4 pages        64 KiB .. 256 KiB
#   20 many chunks      256 KiB .. 4 MiB
#    5 big              4 MiB .. 32 MiB
#
# Sums to 100. Returns the size on stdout.
mc_pick_size_boundary_biased() {
    local tag="$1"
    local bucket
    bucket=$(mc_pick_weighted "${tag}-bucket" \
        "18:sub_sector" "12:exact" "20:sub_page" \
        "25:few_pages" "20:many_chunks" "5:big")
    case "$bucket" in
        sub_sector)
            # 1..4095
            echo $(( $(mc_rng_u32 "${tag}-size" 4095) + 1 ))
            ;;
        exact)
            local choices=(4096 65535 65536 65537 131072)
            local idx
            idx=$(mc_rng_u32 "${tag}-exact" 5)
            echo "${choices[$idx]}"
            ;;
        sub_page)
            # 4096..65535
            echo $(( $(mc_rng_u32 "${tag}-size" 61440) + 4096 ))
            ;;
        few_pages)
            # 65536..262143
            echo $(( $(mc_rng_u32 "${tag}-size" 196608) + 65536 ))
            ;;
        many_chunks)
            # 262144..4194303
            echo $(( $(mc_rng_u32 "${tag}-size" 3932160) + 262144 ))
            ;;
        big)
            # 4194304..33554431
            echo $(( $(mc_rng_u32 "${tag}-size" 29360128) + 4194304 ))
            ;;
    esac
}

# Write `size` bytes of deterministic-but-random-looking content to
# `out_path`. Content is keyed by `(MC_SEED, key, version)`; same key +
# version always yields the same bytes, so callers can verify a read by
# regenerating into a tmp file and `cmp`'ing.
#
# AES-256-CTR over /dev/zero is the cheapest way to get high-entropy
# bytes from a keyed seed without needing a per-byte hash. Throughput is
# ~1 GB/s on any modern CPU.
mc_content_to() {
    local key="$1" version="$2" size="$3" out_path="$4"
    local key_hex
    key_hex=$(printf '%s|%s|%s|content' "$MC_SEED" "$key" "$version" \
        | b3sum --no-names 2>/dev/null \
        || printf '%s|%s|%s|content' "$MC_SEED" "$key" "$version" | sha256sum | awk '{print $1}')
    key_hex="${key_hex:0:64}"
    # IV is fixed at zeros — we re-key per (path, version), so reusing
    # the IV is fine (the keystream never repeats across versions or
    # across paths). Saves an extra RNG draw.
    openssl enc -aes-256-ctr -K "$key_hex" -iv 00000000000000000000000000000000 \
        -in /dev/zero 2>/dev/null \
        | head -c "$size" > "$out_path"
}

# Append one structured log line. Caller-supplied free-form fields
# follow op-name; we don't enforce a schema so each script can record
# what's natural (path / size / position / etc).
#
# Format:  [N] op=OPNAME k1=v1 k2=v2 ...
mc_log_op() {
    local op="$1"; shift
    printf '[%d] op=%s' "$MC_OP_INDEX" "$op" >> "$MC_OP_LOG"
    for kv in "$@"; do
        printf ' %s' "$kv" >> "$MC_OP_LOG"
    done
    printf '\n' >> "$MC_OP_LOG"
}

# Dump the last N op-log lines + the seed banner to stderr. Called from
# the harness on any verification failure; the goal is that the user can
# copy-paste a reproducer command line from the failure output without
# scrolling.
mc_dump_failure() {
    local tail_n="${1:-50}"
    echo "" >&2
    echo "================================================================" >&2
    echo "Monte Carlo FAILURE  —  seed=$MC_SEED  op_index=$MC_OP_INDEX" >&2
    echo "Reproduce with: --seed $MC_SEED" >&2
    echo "Op log: $MC_OP_LOG" >&2
    echo "Last $tail_n ops:" >&2
    tail -n "$tail_n" "$MC_OP_LOG" | sed 's/^/  /' >&2
    echo "================================================================" >&2
}
