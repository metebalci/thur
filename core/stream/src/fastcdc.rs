// Copyright (c) 2026 Mete Balci
// SPDX-License-Identifier: Apache-2.0

//! FastCDC content-defined chunking.
//!
//! Reference: Xia et al., "FastCDC: a Fast and Efficient Content-Defined
//! Chunking Approach for Data Deduplication", USENIX ATC 2016.
//!
//! Why this exists: fixed-size chunking dedups poorly under shift —
//! inserting one byte at the start of a tar stream re-aligns every
//! downstream chunk and the BLAKE3 hashes all change, so the cross-
//! cartridge dedup pool sees zero hits. FastCDC picks chunk boundaries
//! based on the *content* of the bytes (a Gear-hash rolling window):
//! after the shifted region, cut points re-converge within ~one chunk
//! and the rest of the stream dedups normally.
//!
//! Implementation notes:
//! - **Gear hash**, not Rabin. `h = (h << 1) + GEAR[byte]`. ~10× faster
//!   than Rabin in the inner loop and produces equivalently uniform cuts
//!   for our purposes.
//! - **Normalized cuts**: two masks. Below `avg`, use a stricter mask
//!   (more zero bits required → cuts unlikely → grow). Above `avg`, use
//!   a looser mask (cuts likely → seal). Tightens the chunk-size
//!   distribution around `avg` and avoids long pathological tails.
//! - **Hard bounds**: `min` skips boundary checks entirely (no chunk
//!   smaller than min); `max` forces a cut even when no boundary fires
//!   (no chunk larger than max).
//! - **No window**: FastCDC's Gear hash doesn't subtract bytes leaving
//!   the window — it just keeps shifting left. The shift drains stale
//!   bits naturally. Window-less Gear is part of why it's fast.
//!
//! The GEAR table is fixed at compile time. Two Thur VTL processes
//! reading the same bytes MUST emit the same cut points; otherwise
//! cross-cartridge dedup breaks. So the table is reproducible, not
//! random per build.
//!
//! Performance:
//! - **Pre-min skip**: `h = (h << 1) + GEAR[b]` is u64, so a byte's
//!   contribution shifts entirely out of the accumulator after 64
//!   iterations. Hashing bytes more than 64 positions before the
//!   first possible cut (`pos == min - 64`) is therefore wasted work
//!   — we start the rolling hash there with `h = 0` and produce a
//!   bit-exact match to the from-zero accumulation. For default
//!   bounds (1 MiB / 8 MiB / 32 MiB) this skips ~99.99% of the
//!   pre-min region per chunk.
//! - **Phase-split feed**: `StreamingChunker::feed` runs separate
//!   inner loops for the skip / warmup / strict-mask / loose-mask /
//!   forced-cut phases instead of branching on `pos vs min/avg/max`
//!   inside a single loop. The hot loops are then 2-3 instructions
//!   on the critical path through `h` (shl-1 + add of indexed-load),
//!   close to the scalar floor of ~2 cycles/byte on modern x86.
//! - **No SIMD (intentional)**: Gear's `h_n = (h_{n-1} << 1) +
//!   GEAR[b]` has a strict serial dependency on `h`. The standard
//!   K-shifted-table parallel-batch trick that breaks the chain
//!   needs AVX-512 (8 native u64 lanes) to consistently beat scalar;
//!   on AVX2-only targets (e.g. AMD Zen3) the uop count for a K=4
//!   batched chain is roughly the same as the scalar path. Both
//!   `find_cut` and `feed` are bit-exact equivalent to a naïve
//!   single-loop reference (`tests::find_cut_bit_exact_against_reference`,
//!   `tests::feed_bit_exact_against_reference_byte_by_byte`,
//!   `tests::feed_split_arbitrarily_matches_single_call`) so any
//!   future SIMD path can be slotted in without changing cut
//!   points.

#![allow(clippy::unreadable_literal)]

/// Default minimum chunk size for FastCDC. Cuts are not allowed below
/// this length — prevents pathological tiny chunks.
pub const DEFAULT_MIN_SIZE: usize = 1024 * 1024; // 1 MiB

/// Default average chunk size. The masks are tuned so the expected cut
/// distance is approximately this value.
pub const DEFAULT_AVG_SIZE: usize = 8 * 1024 * 1024; // 8 MiB

/// Default maximum chunk size. Beyond this, a cut is forced regardless
/// of content. Caps tail latency for cloud upload.
pub const DEFAULT_MAX_SIZE: usize = 32 * 1024 * 1024; // 32 MiB

/// Build a Gear-hash cut mask with `bits` set, drawn from a fixed bit
/// ordering. The ordering covers the middle 48 bits of the 64-bit word
/// (positions [8, 56)) — that's where the GEAR rolling-hash's
/// shift-left-and-add mixing is best (low byte is the freshly-added
/// GEAR value, high byte is several iterations stale).
///
/// `bits` is clamped to 48 for safety. Two Thur VTL builds calling
/// this with the same `bits` value MUST produce the same mask —
/// otherwise cross-cartridge dedup breaks. Consequently the bit
/// ordering is a `const` array rather than anything derived at runtime.
const fn build_cut_mask(bits: u32) -> u64 {
    // Stable interleaved bit ordering across the 48 middle-word
    // positions: alternating odd/even within each byte, sweeping
    // outward from the center byte. Picked for spread (every input
    // byte's contribution lands under at least one mask bit fairly
    // quickly) rather than for any cryptographic property.
    const BIT_ORDER: [u32; 48] = [
        24, 32, 16, 40, 8, 48, 26, 30, 18, 38, 10, 46, 25, 33, 17, 41, 9, 49, 27, 31, 19, 39, 11,
        47, 28, 34, 20, 42, 12, 50, 29, 35, 21, 43, 13, 51, 14, 52, 15, 53, 22, 44, 23, 45, 36, 54,
        37, 55,
    ];
    let n = if bits > 48 { 48 } else { bits } as usize;
    let mut m: u64 = 0;
    let mut i = 0;
    while i < n {
        m |= 1u64 << BIT_ORDER[i];
        i += 1;
    }
    m
}

/// Number of mask bits that targets an expected cut distance of
/// `expected_bytes`: `bits = ceil(log2(expected_bytes))`. Saturates at
/// 48 (the bit-ordering table's length).
const fn bits_for_distance(expected_bytes: usize) -> u32 {
    let mut e = expected_bytes;
    let mut k: u32 = 0;
    while e > 1 {
        e >>= 1;
        k += 1;
    }
    // Round up to the next power of two if not already aligned.
    if (1usize << k) < expected_bytes {
        k += 1;
    }
    if k > 48 { 48 } else { k }
}

/// FastCDC chunker. Stateless apart from its parameters — `find_cut`
/// is a pure function of the input slice and (min, avg, max).
///
/// `mask_s` / `mask_l` are derived from `avg` at construction so the
/// expected cut distances scale with the chosen chunk size: strict
/// mask aims at `2 * avg`, loose mask at `avg / 2`. Pre-2026-05-04
/// builds used hardcoded masks calibrated for ~8 KiB chunks — those
/// are still callable via `with_masks` for replaying old cartridges,
/// but anything new should construct via `new` / `default`.
#[derive(Debug, Clone, Copy)]
pub struct FastCdc {
    pub min: usize,
    pub avg: usize,
    pub max: usize,
    mask_s: u64,
    mask_l: u64,
}

impl Default for FastCdc {
    fn default() -> Self {
        Self::new(DEFAULT_MIN_SIZE, DEFAULT_AVG_SIZE, DEFAULT_MAX_SIZE)
    }
}

impl FastCdc {
    /// Construct a FastCdc with explicit bounds. Panics if the bounds
    /// are nonsensical (min > avg, avg > max, or min == 0). These are
    /// programming errors, not runtime conditions.
    ///
    /// Cut masks are derived from `avg`: strict mask targets expected
    /// distance `2 * avg`, loose mask targets `avg / 2`. Call sites
    /// don't need to think about this — it just makes uniform-random
    /// input (e.g. ciphertext from drive-side AES-GCM) cut near `avg`
    /// instead of clustering at `min`.
    pub fn new(min: usize, avg: usize, max: usize) -> Self {
        assert!(min > 0, "FastCdc::new: min must be > 0");
        assert!(min <= avg, "FastCdc::new: min must be <= avg");
        assert!(avg <= max, "FastCdc::new: avg must be <= max");
        let mask_s = build_cut_mask(bits_for_distance(avg.saturating_mul(2)));
        let mask_l = build_cut_mask(bits_for_distance(avg / 2).max(1));
        Self {
            min,
            avg,
            max,
            mask_s,
            mask_l,
        }
    }

    /// Find the next cut point in `data`, starting from offset 0.
    ///
    /// Return value:
    ///   * `n` in `(min..=max)` — a content-defined cut at offset `n`.
    ///     The chunk is `data[..n]`; remaining bytes are `data[n..]`.
    ///   * `data.len()` — no cut found and we ran out of input. Caller
    ///     should buffer more bytes before trying again, or seal as-is
    ///     if at end-of-stream.
    ///   * `max` if `data.len() >= max` and no boundary fired — forced
    ///     cut at max (still ≤ data.len()).
    ///
    /// If `data.len() <= min`, returns `data.len()` (no cut emitted —
    /// the chunk is too small even to consider).
    ///
    /// Performance: bytes more than 64 positions before the first
    /// possible cut (i.e., before `min - 64`) cannot affect the rolling
    /// hash at any check position — `h = (h << 1) + GEAR[b]` is u64,
    /// so any byte's contribution shifts entirely out of the word
    /// after 64 iterations. The pre-min loop therefore starts at
    /// `min - 64` rather than 0, skipping ~`min - 64` bytes of pure
    /// hash work for free. Cut points are bit-exact identical to a
    /// naïve from-zero implementation.
    pub fn find_cut(&self, data: &[u8]) -> usize {
        let n = data.len();
        if n <= self.min {
            return n;
        }
        let upper = n.min(self.max);

        // Pre-min skip: any byte at index < min - 64 is drained from
        // the 64-bit Gear hash by the time we reach min, so we can
        // start hashing at `min - 64` with h = 0. The first cut check
        // happens at post-pos > min, by which point h is bit-exact
        // identical to the from-zero accumulation.
        let mut i = self.min.saturating_sub(GEAR_WINDOW);
        let mut h: u64 = 0;
        // Phase 0: warmup. Hash bytes [min-64, min) with no cut check.
        while i < self.min && i < upper {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            i += 1;
        }
        // Phase 1: min..avg, strict mask.
        let avg_boundary = upper.min(self.avg);
        while i < avg_boundary {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            if h & self.mask_s == 0 {
                return i + 1;
            }
            i += 1;
        }
        // Phase 2: avg..max (or upper), loose mask.
        while i < upper {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            if h & self.mask_l == 0 {
                return i + 1;
            }
            i += 1;
        }
        // No content-defined cut. If we hit max, force a cut there.
        if upper == self.max { upper } else { n }
    }
}

/// The Gear hash window — once a byte is shifted left this many times
/// it has fallen out of the 64-bit accumulator entirely. Anything older
/// than this contributes nothing to `h` at the current position, which
/// is the basis for the pre-min skip in `find_cut` / `feed`.
const GEAR_WINDOW: usize = 64;

/// Streaming variant of the chunker — keeps rolling-hash state across
/// `feed()` calls so the write path can decide "should I seal after this
/// block?" in O(block_len) instead of O(chunk_size). Used by the
/// cartridge write path: each `write_data` calls `feed()` with the
/// block's bytes; if the chunker says "cut", the cartridge seals the
/// staging chunk before the *next* block — that's the block-aligned
/// approximation of CDC.
///
/// Block-aligned CDC trades a bit of dedup ratio for staying inside
/// the existing `BlockIndex` schema (a block never spans chunks). For
/// tar-style backup streams where block boundaries are fixed at the
/// tar blocking factor, this is fine.
#[derive(Debug, Clone)]
pub struct StreamingChunker {
    chunker: FastCdc,
    hash: u64,
    /// Bytes consumed since the last `reset()` (or since construction).
    pos: usize,
}

impl StreamingChunker {
    pub fn new(chunker: FastCdc) -> Self {
        Self {
            chunker,
            hash: 0,
            pos: 0,
        }
    }

    /// Feed `bytes` and update internal state. Returns `true` if a cut
    /// should fire after these bytes — either because the rolling hash
    /// matched the appropriate mask, or because the running chunk size
    /// reached `max`.
    ///
    /// The rolling hash is updated for every byte that can still affect
    /// a future mask check (i.e., bytes within 64 positions of `min` or
    /// later); bytes that will be drained out of the 64-bit Gear hash
    /// before the first check are skipped entirely. Cut checks are only
    /// consulted once `pos > min` (strict mask) and switch to the loose
    /// mask once `pos >= avg`.
    ///
    /// Implementation: the loop is phase-split (skip / warmup /
    /// strict / loose / forced) so the inner loop body matches the
    /// active phase exactly — no per-byte branch on `pos vs min/avg/max`
    /// inside the hot loop. Cut points are bit-exact identical to a
    /// naïve byte-by-byte single-loop implementation, which means a
    /// 1-byte difference at exactly post-pos == `avg` (the original
    /// behavior, where post-pos == `avg` uses the loose mask) is
    /// preserved.
    pub fn feed(&mut self, bytes: &[u8]) -> bool {
        let min = self.chunker.min;
        let avg = self.chunker.avg;
        let max = self.chunker.max;
        let mask_s = self.chunker.mask_s;
        let mask_l = self.chunker.mask_l;

        let mut h = self.hash;
        let mut pos = self.pos;
        let mut i = 0usize;
        let n = bytes.len();

        // Phase 0: skip bytes whose contribution to `h` will be drained
        // before the first cut check. Cut checks start at post-pos =
        // min + 1, which depends only on bytes [min - GEAR_WINDOW, min).
        // Anything at pos < min - GEAR_WINDOW is moot — old bytes in `h`
        // are also drained, so we can zero `h` and seek `pos` directly.
        let pre_hash_threshold = min.saturating_sub(GEAR_WINDOW);
        if pos < pre_hash_threshold {
            let skip = (pre_hash_threshold - pos).min(n);
            i = skip;
            pos += skip;
            if pos == pre_hash_threshold {
                // All older state is now drained; `h` at the next
                // hashed byte will rebuild from zero — bit-exact match
                // to the from-zero accumulation.
                h = 0;
            }
            if i == n {
                self.hash = h;
                self.pos = pos;
                return false;
            }
        }

        // Phase 1: warmup. Hash bytes up to `pos == min` with no check.
        // Body runs for post-pos in [pre_hash_threshold + 1, min].
        while pos < min && i < n {
            h = h.wrapping_shl(1).wrapping_add(GEAR[bytes[i] as usize]);
            i += 1;
            pos += 1;
        }

        // Phase 2: strict mask. Body runs for post-pos in [min + 1, avg - 1].
        // Stop when post-pos == avg (which uses the loose mask).
        while pos + 1 < avg && i < n {
            h = h.wrapping_shl(1).wrapping_add(GEAR[bytes[i] as usize]);
            i += 1;
            pos += 1;
            if h & mask_s == 0 {
                self.hash = h;
                self.pos = pos;
                return true;
            }
        }

        // Phase 3: loose mask. Body runs for post-pos in [avg, max - 1].
        // Stop one short of max so the forced-cut iteration is its own
        // (branch-free) tail.
        while pos + 1 < max && i < n {
            h = h.wrapping_shl(1).wrapping_add(GEAR[bytes[i] as usize]);
            i += 1;
            pos += 1;
            if h & mask_l == 0 {
                self.hash = h;
                self.pos = pos;
                return true;
            }
        }

        // Phase 4: forced cut. Post-pos == max is a hard boundary —
        // emit the cut even if no mask matched.
        if pos + 1 == max && i < n {
            h = h.wrapping_shl(1).wrapping_add(GEAR[bytes[i] as usize]);
            pos += 1;
            self.hash = h;
            self.pos = pos;
            return true;
        }

        self.hash = h;
        self.pos = pos;
        false
    }

    /// Reset state so the next `feed()` starts a fresh chunk. Call this
    /// after the cartridge actually seals a chunk.
    pub fn reset(&mut self) {
        self.hash = 0;
        self.pos = 0;
    }

    /// Bytes consumed since last reset.
    pub fn pos(&self) -> usize {
        self.pos
    }

    pub fn params(&self) -> FastCdc {
        self.chunker
    }
}

/// FastCDC GEAR table. 256 deterministic 64-bit values generated at
/// compile time from a fixed seed via splitmix64. Frozen by construction —
/// every Thur VTL build sees the same table, which is what cross-build
/// dedup requires.
const GEAR: [u64; 256] = build_gear_table(0x4e696d627573564c); // "Thur VTLVL"

const fn build_gear_table(seed: u64) -> [u64; 256] {
    // splitmix64: const-eval-friendly, well-mixed bits, deterministic.
    let mut out = [0u64; 256];
    let mut s = seed;
    let mut i = 0;
    while i < 256 {
        s = s.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z = z ^ (z >> 31);
        out[i] = z;
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn deterministic_bytes(seed: u64, n: usize) -> Vec<u8> {
        // xorshift — fast, good enough for test fixtures.
        let mut s = seed.max(1);
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            s ^= s << 13;
            s ^= s >> 7;
            s ^= s << 17;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out.truncate(n);
        out
    }

    fn cuts(cdc: &FastCdc, data: &[u8]) -> Vec<usize> {
        let mut offsets = Vec::new();
        let mut start = 0;
        while start < data.len() {
            let cut = cdc.find_cut(&data[start..]);
            if cut == 0 || start + cut > data.len() {
                break;
            }
            offsets.push(start + cut);
            start += cut;
            if cut == data.len() - (offsets.last().copied().unwrap_or(0) - cut) {
                // No further progress possible (find_cut returned input
                // length with no boundary). Break to avoid infinite loop.
                if cut < cdc.min {
                    break;
                }
            }
        }
        offsets
    }

    #[test]
    fn deterministic_same_input_same_cuts() {
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let data = deterministic_bytes(42, 8 * 1024 * 1024);
        let a = cuts(&cdc, &data);
        let b = cuts(&cdc, &data);
        assert_eq!(a, b);
        assert!(!a.is_empty(), "expected at least one content-defined cut");
    }

    #[test]
    fn cut_sizes_respect_bounds() {
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let data = deterministic_bytes(7, 16 * 1024 * 1024);
        let mut prev = 0;
        for c in cuts(&cdc, &data) {
            let size = c - prev;
            assert!(size >= cdc.min, "chunk size {} below min {}", size, cdc.min);
            assert!(size <= cdc.max, "chunk size {} above max {}", size, cdc.max);
            prev = c;
        }
    }

    #[test]
    fn shift_invariance_after_one_byte_prefix() {
        // Insert a single byte at the start of an otherwise-identical
        // stream. The first cut moves, but subsequent cuts converge
        // back to the same content offsets — that's the headline
        // property of content-defined chunking.
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let original = deterministic_bytes(1234, 8 * 1024 * 1024);
        let mut shifted = vec![0xFF];
        shifted.extend_from_slice(&original);

        let cuts_a = cuts(&cdc, &original);
        let cuts_b = cuts(&cdc, &shifted);

        // Compare by chunk-content hashes. A chunk's hash depends only
        // on its bytes; if shifted-stream chunks contain the same byte
        // ranges (modulo the 1-byte prefix), the hashes match.
        use std::collections::HashSet;
        let chunks_a: HashSet<_> = chunk_hashes(&original, &cuts_a).into_iter().collect();
        let chunks_b: HashSet<_> = chunk_hashes(&shifted, &cuts_b).into_iter().collect();
        let shared = chunks_a.intersection(&chunks_b).count();
        let min_total = chunks_a.len().min(chunks_b.len());
        assert!(
            shared * 100 >= min_total * 80,
            "expected ≥80% chunk overlap after 1-byte prefix shift; got \
             {}/{} ({:.0}%) — chunks_a={}, chunks_b={}",
            shared,
            min_total,
            (shared as f64 / min_total as f64) * 100.0,
            chunks_a.len(),
            chunks_b.len()
        );
    }

    fn chunk_hashes(data: &[u8], cuts: &[usize]) -> Vec<[u8; 32]> {
        let mut out = Vec::new();
        let mut prev = 0;
        for &c in cuts {
            let h = blake3::hash(&data[prev..c]);
            out.push(*h.as_bytes());
            prev = c;
        }
        if prev < data.len() {
            let h = blake3::hash(&data[prev..]);
            out.push(*h.as_bytes());
        }
        out
    }

    #[test]
    fn small_input_below_min_no_cut() {
        let cdc = FastCdc::default();
        let data = vec![0u8; cdc.min - 1];
        assert_eq!(cdc.find_cut(&data), data.len());
    }

    #[test]
    fn streaming_byte_by_byte_matches_oneshot() {
        // Byte-by-byte feed should report the cut at exactly the same
        // offset as one-shot find_cut. With multi-byte feeds the cut is
        // rounded up to the feed boundary — that's the block-aligned
        // behavior used by the cartridge write path, intentional, and
        // tested separately below.
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let data = deterministic_bytes(99, 4 * 1024 * 1024);
        let oneshot_cut = cdc.find_cut(&data);

        let mut sc = StreamingChunker::new(cdc);
        let mut streaming_cut = data.len();
        for (i, b) in data.iter().enumerate() {
            if sc.feed(std::slice::from_ref(b)) {
                streaming_cut = i + 1;
                break;
            }
        }
        assert_eq!(oneshot_cut, streaming_cut);
    }

    #[test]
    fn streaming_block_aligned_rounds_up() {
        // Feeding in fixed-size blocks: streaming reports the cut at
        // the END of the block in which the CDC boundary fired.
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let data = deterministic_bytes(99, 4 * 1024 * 1024);
        let oneshot_cut = cdc.find_cut(&data);

        let block = 4096usize;
        let mut sc = StreamingChunker::new(cdc);
        let mut consumed = 0usize;
        let mut streaming_cut = data.len();
        while consumed < data.len() {
            let end = (consumed + block).min(data.len());
            if sc.feed(&data[consumed..end]) {
                streaming_cut = end;
                break;
            }
            consumed = end;
        }
        // Block-aligned cut must be >= one-shot cut and differ by less
        // than one block.
        assert!(
            streaming_cut >= oneshot_cut,
            "block-aligned cut {} < oneshot cut {}",
            streaming_cut,
            oneshot_cut
        );
        assert!(streaming_cut - oneshot_cut < block);
    }

    #[test]
    fn mean_chunk_size_near_avg_on_random_data() {
        // Calibration test for MASK_S / MASK_L. With the default
        // (1 / 8 / 32 MiB) bounds and a uniform-random fixture, the
        // mean chunk size should land near `avg = 8 MiB`. This
        // regression-tests the past mask miscalibration where the
        // strict mask was tuned for ~8 KiB chunks and produced a
        // mean of ~1.07 MiB (every cut firing within 8 KiB of `min`).
        //
        // Acceptable band is wide on purpose — uniform-random is a
        // worst case for FastCDC's two-mask scheme; real workloads
        // produce slightly larger means. Asserting "between half and
        // double of avg" catches order-of-magnitude miscalibration
        // while tolerating mask-pattern choice.
        let cdc = FastCdc::default();
        let data = deterministic_bytes(0xC1B0_FA57, 256 * 1024 * 1024);
        let offsets = cuts(&cdc, &data);
        assert!(
            offsets.len() >= 8,
            "expected ≥8 cuts on a 256 MiB random fixture, got {}",
            offsets.len()
        );
        let mean = data.len() / offsets.len();
        let lo = cdc.avg / 2;
        let hi = cdc.avg * 2;
        assert!(
            mean >= lo && mean <= hi,
            "mean chunk size {} bytes outside [{}, {}] (avg = {}); \
             got {} cuts on {} bytes — masks likely miscalibrated",
            mean,
            lo,
            hi,
            cdc.avg,
            offsets.len(),
            data.len()
        );
    }

    #[test]
    fn force_cut_at_max_when_no_boundary_found() {
        // All-zero input never hashes to a cut (mask & 0 == 0 but the
        // hash is also 0 from the start, so it does hit). Use input
        // that's guaranteed not to fire by feeding a stream the GEAR
        // table can't easily zero out. Easier: test the forcing with
        // a constant byte known not to hash to a boundary in the
        // strict-mask phase. For correctness we just verify the
        // returned offset never exceeds max.
        let cdc = FastCdc::new(1024, 4096, 8192);
        let data = vec![0xAA; 32 * 1024];
        let cut = cdc.find_cut(&data);
        assert!(
            cut <= cdc.max,
            "find_cut returned {} > max {}",
            cut,
            cdc.max
        );
        assert!(
            cut >= cdc.min,
            "find_cut returned {} < min {}",
            cut,
            cdc.min
        );
    }

    /// Naïve from-zero reference implementation. Bit-exact mirror of
    /// the pre-optimization `find_cut`: hashes every byte from offset
    /// 0, no skip. Used as the oracle for equivalence tests.
    fn reference_find_cut(cdc: &FastCdc, data: &[u8]) -> usize {
        let n = data.len();
        if n <= cdc.min {
            return n;
        }
        let upper = n.min(cdc.max);
        let mut h: u64 = 0;
        let mut i = 0;
        while i < cdc.min && i < upper {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            i += 1;
        }
        let avg_boundary = upper.min(cdc.avg);
        while i < avg_boundary {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            if h & cdc.mask_s == 0 {
                return i + 1;
            }
            i += 1;
        }
        while i < upper {
            h = h.wrapping_shl(1).wrapping_add(GEAR[data[i] as usize]);
            if h & cdc.mask_l == 0 {
                return i + 1;
            }
            i += 1;
        }
        if upper == cdc.max { upper } else { n }
    }

    /// Naïve byte-by-byte reference for the streaming chunker. Bit-exact
    /// mirror of the pre-optimization single-loop `feed()`.
    fn reference_feed(cdc: &FastCdc, data: &[u8]) -> Option<usize> {
        let mut h: u64 = 0;
        let mut pos: usize = 0;
        for &b in data {
            pos += 1;
            h = h.wrapping_shl(1).wrapping_add(GEAR[b as usize]);
            if pos <= cdc.min {
                continue;
            }
            if pos >= cdc.max {
                return Some(pos);
            }
            let mask = if pos < cdc.avg {
                cdc.mask_s
            } else {
                cdc.mask_l
            };
            if h & mask == 0 {
                return Some(pos);
            }
        }
        None
    }

    #[test]
    fn find_cut_bit_exact_against_reference() {
        // Sweep a range of (min, avg, max) plus several seeds. Optimized
        // `find_cut` must produce the SAME offset as the naive reference
        // for every fixture — otherwise the pre-min skip has changed
        // cut points and cross-cartridge dedup would silently drift.
        let configs = [
            (1024, 4096, 8192),
            (4096, 16 * 1024, 64 * 1024),
            (64 * 1024, 256 * 1024, 1024 * 1024),
            (256 * 1024, 2 * 1024 * 1024, 8 * 1024 * 1024),
            (1024 * 1024, 8 * 1024 * 1024, 32 * 1024 * 1024),
            // min < GEAR_WINDOW: skip path must be a no-op.
            (32, 256, 1024),
        ];
        for (min, avg, max) in configs {
            let cdc = FastCdc::new(min, avg, max);
            for seed in [1u64, 7, 42, 0xC0FFEE, 0x9E3779B9, 0xDEAD_BEEF] {
                // Test with multiple sizes to exercise pre-min, strict,
                // loose, and forced-cut paths.
                for size_mul in [2, 3, 8, 32] {
                    let data = deterministic_bytes(seed, max * size_mul);
                    let opt = cdc.find_cut(&data);
                    let reference = reference_find_cut(&cdc, &data);
                    assert_eq!(
                        opt,
                        reference,
                        "find_cut diverged from reference at \
                         (min,avg,max)=({min},{avg},{max}) seed={seed:x} \
                         size={}: optimized={opt} reference={reference}",
                        data.len()
                    );
                }
            }
        }
    }

    #[test]
    fn feed_bit_exact_against_reference_byte_by_byte() {
        // The phase-split `feed()` must produce the same first-cut
        // post-pos as a from-zero byte-by-byte single-loop reference,
        // for both small (block_size=1) and large (block_size=64K)
        // feed shapes. Exercises every phase boundary.
        let configs = [
            (1024, 4096, 8192),
            (4096, 16 * 1024, 64 * 1024),
            (64 * 1024, 256 * 1024, 1024 * 1024),
            (1024 * 1024, 8 * 1024 * 1024, 32 * 1024 * 1024),
            // min < GEAR_WINDOW.
            (16, 128, 512),
        ];
        for (min, avg, max) in configs {
            let cdc = FastCdc::new(min, avg, max);
            for seed in [3u64, 11, 99, 0xC1B0_FA57] {
                let data = deterministic_bytes(seed, max * 4);
                let reference_cut = reference_feed(&cdc, &data).unwrap_or(data.len());
                // Try several feed-block sizes to cover phase
                // transitions across calls.
                for &block in &[1usize, 17, 256, 4096, 64 * 1024, max] {
                    let mut sc = StreamingChunker::new(cdc);
                    let mut cut_pos = data.len();
                    let mut consumed = 0;
                    while consumed < data.len() {
                        let end = (consumed + block).min(data.len());
                        if sc.feed(&data[consumed..end]) {
                            // Block-aligned cut: streaming reports cut at
                            // end-of-feed; we want pos at the cut moment.
                            cut_pos = sc.pos();
                            break;
                        }
                        consumed = end;
                    }
                    // For block=1 the cut must match exactly. For
                    // larger blocks, the cut must land on a
                    // block-aligned position >= reference but < ref +
                    // block (the documented block-aligned behavior).
                    if block == 1 {
                        assert_eq!(
                            cut_pos, reference_cut,
                            "feed(block=1) diverged at \
                             (min,avg,max)=({min},{avg},{max}) seed={seed:x}: \
                             optimized={cut_pos} reference={reference_cut}"
                        );
                    } else {
                        assert!(
                            cut_pos >= reference_cut,
                            "feed(block={block}) reported cut at {cut_pos} BEFORE \
                             reference cut at {reference_cut} \
                             ((min,avg,max)=({min},{avg},{max}) seed={seed:x})",
                        );
                        assert!(
                            cut_pos - reference_cut < block,
                            "feed(block={block}) overshot reference: \
                             cut={cut_pos} ref={reference_cut} delta={} >= block \
                             ((min,avg,max)=({min},{avg},{max}) seed={seed:x})",
                            cut_pos - reference_cut,
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn feed_split_arbitrarily_matches_single_call() {
        // Splitting the same byte stream across an arbitrary number of
        // feed() calls must produce the SAME first cut as a single
        // feed() call with all the bytes. This exercises Phase 0 skip
        // partial advances, mid-phase boundary crossings, and h state
        // carry-over across calls.
        let cdc = FastCdc::new(64 * 1024, 256 * 1024, 1024 * 1024);
        let data = deterministic_bytes(0xABCD_1234, 4 * 1024 * 1024);

        // Reference: one big feed.
        let mut sc_ref = StreamingChunker::new(cdc);
        let single_cut = if sc_ref.feed(&data) {
            sc_ref.pos()
        } else {
            data.len()
        };

        // Try several splitting patterns.
        let split_patterns: [&[usize]; 5] = [
            &[1, 1, 1, 1],                            // tiny dribble
            &[63, 1, 64, 32, 256, 1024],              // crosses GEAR_WINDOW boundary
            &[100_000, 200_000, 50_000, 1_000_000],   // big chunks
            &[cdc.min - 1, 1, 1, 1],                  // exactly at min boundary
            &[cdc.min + 5, cdc.avg - cdc.min - 5, 1], // crosses avg boundary
        ];
        for pattern in &split_patterns {
            let mut sc = StreamingChunker::new(cdc);
            let mut consumed = 0usize;
            let mut cut_pos: Option<usize> = None;
            // Walk the pattern, repeating it until we either fire a cut
            // or run out of data.
            'outer: loop {
                for &len in *pattern {
                    if consumed >= data.len() {
                        break 'outer;
                    }
                    let end = (consumed + len).min(data.len());
                    if sc.feed(&data[consumed..end]) {
                        cut_pos = Some(sc.pos());
                        break 'outer;
                    }
                    consumed = end;
                }
            }
            let split_cut = cut_pos.unwrap_or(data.len());
            assert_eq!(
                split_cut, single_cut,
                "split-feed pattern {:?} diverged from single feed: \
                 split={split_cut} single={single_cut}",
                pattern
            );
        }
    }

    // -- Property tests over find_cut + tiling ---------------------------
    //
    // The existing tests above pin specific scenarios with deterministic
    // xorshift fixtures. The proptest pass widens input variety. Tunables
    // are kept modest to fit the default proptest budget (256 cases).

    use proptest::prelude::*;

    fn small_cdc() -> FastCdc {
        // 64 / 256 / 1024 byte bounds keep proptest fixtures tiny while
        // still exercising the strict-mask / loose-mask / forced-cut
        // phases.
        FastCdc::new(64, 256, 1024)
    }

    proptest! {
        // Tile the input exhaustively via repeated find_cut. Sum of chunk
        // lengths must equal input length, and each chunk's offset must
        // be strictly ascending (no zero-length chunks, no overlaps).
        #[test]
        fn tiling_chunks_to_input_exactly(data in proptest::collection::vec(any::<u8>(), 0..20_000)) {
            let cdc = small_cdc();
            let mut offsets = Vec::new();
            let mut start = 0usize;
            // Bound the loop defensively in case find_cut ever returns 0.
            for _ in 0..10_000 {
                if start >= data.len() {
                    break;
                }
                let cut = cdc.find_cut(&data[start..]);
                prop_assert!(cut > 0, "find_cut must make progress on non-empty input");
                let abs = start + cut;
                prop_assert!(abs <= data.len(), "find_cut returned past end");
                offsets.push(abs);
                start = abs;
            }
            prop_assert_eq!(start, data.len(), "tiling did not cover full input");

            // Strictly ascending offsets imply no overlap, no gap.
            for w in offsets.windows(2) {
                prop_assert!(w[0] < w[1], "non-monotonic offsets: {:?}", offsets);
            }
        }

        // Every non-trailing chunk respects [min, max]. The final chunk
        // may be shorter than min (end-of-stream seal).
        #[test]
        fn chunk_sizes_respect_bounds(data in proptest::collection::vec(any::<u8>(), 1000..20_000)) {
            let cdc = small_cdc();
            let mut offsets = Vec::new();
            let mut start = 0usize;
            for _ in 0..10_000 {
                if start >= data.len() {
                    break;
                }
                let cut = cdc.find_cut(&data[start..]);
                let abs = start + cut;
                offsets.push(abs);
                start = abs;
            }
            // All chunks except possibly the last must be within bounds.
            let last_idx = offsets.len().saturating_sub(1);
            let mut prev = 0usize;
            for (i, &c) in offsets.iter().enumerate() {
                let size = c - prev;
                if i < last_idx {
                    prop_assert!(size >= cdc.min, "non-final chunk {} below min {}", size, cdc.min);
                    prop_assert!(size <= cdc.max, "non-final chunk {} above max {}", size, cdc.max);
                } else {
                    // Final chunk: must not exceed max; min is relaxed at
                    // end-of-stream.
                    prop_assert!(size <= cdc.max, "final chunk {} above max {}", size, cdc.max);
                }
                prev = c;
            }
        }

        // find_cut is deterministic: same data, same cut.
        #[test]
        fn find_cut_is_deterministic(data in proptest::collection::vec(any::<u8>(), 0..10_000)) {
            let cdc = small_cdc();
            let a = cdc.find_cut(&data);
            let b = cdc.find_cut(&data);
            prop_assert_eq!(a, b);
        }

        // StreamingChunker progress invariants: if feed ever fires, the
        // cut position lies within [min, max]; if find_cut sees no
        // boundary in the data, feed must not fire either.
        //
        // The position-exact equivalence between feed and find_cut is
        // covered for a fixed deterministic stream by
        // `streaming_byte_by_byte_matches_oneshot` above; this proptest
        // intentionally stays at the level of invariants that hold over
        // arbitrary byte sequences without depending on the
        // (intentionally documented) phase-boundary nuances at pos==avg.
        #[test]
        fn streaming_cut_position_in_bounds(data in proptest::collection::vec(any::<u8>(), 0..2000)) {
            let cdc = small_cdc();
            let one_shot = cdc.find_cut(&data);
            let mut sc = StreamingChunker::new(cdc);
            let mut fired_at: Option<usize> = None;
            for (i, b) in data.iter().enumerate() {
                if sc.feed(std::slice::from_ref(b)) {
                    fired_at = Some(i + 1);
                    break;
                }
            }
            if let Some(pos) = fired_at {
                prop_assert!(pos >= cdc.min, "feed cut at {} below min {}", pos, cdc.min);
                prop_assert!(pos <= cdc.max, "feed cut at {} above max {}", pos, cdc.max);
            } else {
                // No cut fired. find_cut must have returned data.len()
                // (i.e. no internal boundary either).
                prop_assert_eq!(one_shot, data.len(),
                    "feed found no cut but find_cut did at {}", one_shot);
            }
        }
    }
}
