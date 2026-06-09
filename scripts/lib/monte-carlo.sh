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
# "pick fresh". Sets globals MC_SEED, MC_OP_INDEX, MC_OP_LOG, MC_OP_STATS.
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
    mc_op_stats_init
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

# Validate that a list of `weight:name` pairs sums to 100. Call once at
# startup with the exact same list the harness will pass to
# mc_pick_weighted. Fails fast with a clear message on drift — this is
# the guard that catches a freshly-added op handler that the picker
# forgot to weight (the bug class that left VTL's filemark ops as dead
# code for months).
mc_assert_weights() {
    local tag="$1"; shift
    local sum=0 pair
    for pair in "$@"; do
        sum=$(( sum + ${pair%%:*} ))
    done
    if (( sum != 100 )); then
        echo "mc_assert_weights[$tag]: weights sum to $sum, expected 100" >&2
        echo "  pairs: $*" >&2
        exit 1
    fi
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

# Keyed AES-256-CTR keystream truncated to `size`. The cheapest way to
# get high-entropy bytes from a keyed seed (~1 GB/s on any modern CPU).
# Size-independent by construction (the key never depends on size), so
# content(N) is always a prefix of content(M>N) — the property
# append / truncate / truncate_extend rely on.
_mc_keystream_to() {
    local key_hex="$1" size="$2" out="$3"
    openssl enc -aes-256-ctr -K "$key_hex" -iv 00000000000000000000000000000000 \
        -in /dev/zero 2>/dev/null | head -c "$size" > "$out"
}

# blake3 (or sha256 fallback) of the piped tag, first 64 hex chars =
# one AES-256 key.
_mc_key64() {
    local tag="$1" h
    h=$(printf '%s' "$tag" | b3sum --no-names 2>/dev/null \
        || printf '%s' "$tag" | sha256sum | awk '{print $1}')
    echo "${h:0:64}"
}

# --- content classes (each a size-independent stream truncated to size) ---

# Unique, high-entropy, incompressible. Never dedupes (key folds in
# path/version). This is the original behavior and the bulk of writes.
_mc_content_random() {
    local key="$1" version="$2" size="$3" out="$4"
    _mc_keystream_to "$(_mc_key64 "$MC_SEED|$key|$version|content")" "$size" "$out"
}

# Compressible: a keyed 512-byte tile repeated (by doubling) to >= size
# then truncated. Period-512 so the compressor actually shrinks it;
# unique per (key,version) so it doesn't also dedupe across files. The
# tile is size-independent, so the prefix property still holds.
_mc_content_compressible() {
    local key="$1" version="$2" size="$3" out="$4"
    local tile="$out.tile"
    _mc_keystream_to "$(_mc_key64 "$MC_SEED|$key|$version|tile")" 512 "$tile"
    cp "$tile" "$out"
    local cur=512
    while (( cur < size )); do
        cat "$out" "$out" > "$out.dbl" && mv "$out.dbl" "$out"
        cur=$(( cur * 2 ))
    done
    head -c "$size" "$out" > "$out.cut" && mv "$out.cut" "$out"
    rm -f "$tile"
}

# Dedup-friendly: drawn from a small shared corpus keyed only by a
# bucket (NOT the file key), so distinct files / cartridges that land on
# the same bucket contain identical chunks that fold in the
# content-addressed pool. High-entropy, so the win is dedup, not
# compression. Size-independent keystream => prefix property holds.
_mc_content_dup() {
    local bucket="$1" size="$2" out="$3"
    _mc_keystream_to "$(_mc_key64 "$MC_SEED|dupcorpus|$bucket")" "$size" "$out"
}

# Write `size` bytes of deterministic content to `out_path`, keyed by
# `(MC_SEED, key, version)`. Same key + version + size always yields the
# same bytes, so a reader verifies by regenerating into a tmp file and
# `cmp`'ing.
#
# The content *class* is a deterministic function of (seed, key,
# version) too, so the verify side reproduces byte-identical content
# with zero model state. The mix exercises three storage-layer behaviors
# a single all-random stream never reaches (a unique high-entropy stream
# defeats both dedup and compression by construction):
#   ~62% random        unique + incompressible (no dedup, no compress)
#   ~20% compressible   the compressor measurably shrinks it
#   ~18% dup-corpus     identical chunks fold in the dedup pool
# All three keep the prefix property, so append / truncate are unaffected.
mc_content_to() {
    local key="$1" version="$2" size="$3" out_path="$4"
    local ch
    ch=$(_mc_key64 "$MC_SEED|$key|$version|class")
    local roll=$(( 16#${ch:0:2} ))           # 0..255
    if (( roll < 158 )); then
        _mc_content_random "$key" "$version" "$size" "$out_path"
    elif (( roll < 210 )); then
        _mc_content_compressible "$key" "$version" "$size" "$out_path"
    else
        local bucket=$(( 16#${ch:2:2} % 6 ))
        _mc_content_dup "$bucket" "$size" "$out_path"
    fi
}

# Per-op + per-status counter, keyed "op|status". `status` defaults to
# "ok" when mc_log_op isn't passed an explicit `status=...` field.
# Initialized by mc_seed_init; bumped by mc_log_op; dumped at run end
# and inside mc_dump_failure so failed runs surface the same coverage
# stats as successful ones.
declare -A MC_OP_STATS

mc_op_stats_init() {
    MC_OP_STATS=()
}

mc_op_stats_incr() {
    local op="$1" status="${2:-ok}"
    local key="${op}|${status}"
    MC_OP_STATS[$key]=$(( ${MC_OP_STATS[$key]:-0} + 1 ))
}

# Print one line per (op, status) bucket, sorted. Output:
#   Op statistics:
#     changer_move|ok = 87
#     read_verify|no_records = 12
#     read_verify|ok = 138
#     ...
# Redirected to caller-supplied stream (default stdout).
mc_op_stats_dump() {
    local stream="${1:-/dev/stdout}"
    {
        echo "Op statistics (op|status = count):"
        local key
        for key in $(printf '%s\n' "${!MC_OP_STATS[@]}" | sort); do
            printf '  %s = %d\n' "$key" "${MC_OP_STATS[$key]}"
        done
    } > "$stream"
}

# Append one structured log line. Caller-supplied free-form fields
# follow op-name; we don't enforce a schema so each script can record
# what's natural (path / size / position / etc). If a `status=` field
# appears in the kv pairs, the counter is bumped under that status;
# otherwise the bucket is "ok".
#
# Format:  [N] op=OPNAME k1=v1 k2=v2 ...
mc_log_op() {
    local op="$1"; shift
    local status="ok"
    local kv
    for kv in "$@"; do
        if [[ "$kv" == status=* ]]; then
            status="${kv#status=}"
        fi
    done
    mc_op_stats_incr "$op" "$status"

    printf '[%d] op=%s' "$MC_OP_INDEX" "$op" >> "$MC_OP_LOG"
    for kv in "$@"; do
        printf ' %s' "$kv" >> "$MC_OP_LOG"
    done
    printf '\n' >> "$MC_OP_LOG"
}

# Assert the daemon is still healthy after a run. Two signals:
#
#   - process liveness: if a PID is passed and it isn't running, the
#     daemon died on us (a crash the data path may not have surfaced
#     because the next I/O simply errored out and got handled).
#   - thread panics in the daemon log: a panicked Tokio task (eviction
#     / upload worker, reachability ticker) can leave the data path
#     intact yet still be a real bug — exactly the kind a long random
#     run is meant to flush out. A panic is never legitimate, so it
#     fails the run.
#
# ERROR-level log lines are *surfaced but not fatal*: this harness
# deliberately induces ENOSPC, abrupt logout, and daemon restarts, any
# of which the daemon may legitimately log at ERROR. We print a count +
# sample for human review rather than flaking the suite on expected
# noise. (Contrast test-smoke.sh, a happy-path test, which treats any
# ERROR as failure.)
#
# Args: daemon_log_path [daemon_pid]. Returns 1 on a fatal signal.
mc_assert_daemon_healthy() {
    local log="$1" pid="${2:-}"
    local rc=0
    if [[ -n "$pid" ]] && ! kill -0 "$pid" 2>/dev/null; then
        log_error "daemon health: process (PID $pid) is not running"
        rc=1
    fi
    if [[ -r "$log" ]] && grep -qE 'panicked|panic occurred' "$log"; then
        log_error "daemon health: thread panic in $log:"
        grep -nE 'panicked|panic occurred' "$log" | tail -10 >&2
        rc=1
    fi
    if [[ -r "$log" ]]; then
        local errs
        errs=$(grep -cE ' ERROR ' "$log" 2>/dev/null || true)
        if (( errs > 0 )); then
            log_warn "daemon health: $errs ERROR line(s) in daemon log (non-fatal; induced failures log at ERROR):"
            grep -nE ' ERROR ' "$log" | tail -5 >&2
        fi
    fi
    return $rc
}

# Dump the last N op-log lines + the seed banner + per-op stats to
# stderr. Called from the harness on any verification failure; the goal
# is that the user can copy-paste a reproducer command line and see
# coverage skew from the failure output without scrolling.
mc_dump_failure() {
    local tail_n="${1:-50}"
    echo "" >&2
    echo "================================================================" >&2
    echo "Monte Carlo FAILURE  —  seed=$MC_SEED  op_index=$MC_OP_INDEX" >&2
    echo "Reproduce with: --seed $MC_SEED" >&2
    echo "Op log: $MC_OP_LOG" >&2
    echo "Last $tail_n ops:" >&2
    tail -n "$tail_n" "$MC_OP_LOG" | sed 's/^/  /' >&2
    echo "" >&2
    mc_op_stats_dump /dev/stderr
    echo "================================================================" >&2
}
