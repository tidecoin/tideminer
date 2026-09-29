/*
 * Copyright 2009 Colin Percival
 * Copyright 2012-2019 Alexander Peslyak
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions
 * are met:
 * 1. Redistributions of source code must retain the above copyright
 *    notice, this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright
 *    notice, this list of conditions and the following disclaimer in the
 *    documentation and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND
 * ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE
 * IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE
 * ARE DISCLAIMED.  IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE
 * FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL
 * DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS
 * OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION)
 * HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT
 * LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY
 * OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF
 * SUCH DAMAGE.
 */
// yespower 1.0 (N = 2048, r = 8) memory-hard core for tideminer's Metal GPU worker.
//
// One hash per 8 SIMD lanes ("warp-8"): lane l (0..7) owns 64-bit word l of every
// 64-byte block (4 x ulong2 in the SIMD-shuffled Salsa20 order of the Rust kernel;
// words 2j / 2j+1 are the halves of pwxform vector j). Per pwxform round each lane
// loads its word of the S0/S1 entries; the rare round whose reads hit an entry written
// earlier in the same round (checked exactly) is replayed sequentially by all 8 lanes.
// BlockMix inputs are loaded one sub-block ahead, so the random V_j line arrives while
// pwxform runs. The CPU does SHA-256 / PBKDF2 before and HMAC after (prepare/finish in
// tidecoin-yespower); this kernel turns B's first 32 words into its last 16.
//
// Dispatch count * 8 threads in threadgroups that are multiples of 8 (32 measured best).
// Measured on an M3 Max (30-core GPU), bit-exact against the Rust kernel: 1 thread per
// hash ~1,150 H/s at 120 hashes in flight; warp-8 1,750; + look-ahead 2,150.
#include <metal_stdlib>
using namespace metal;

#define N_ 2048u
#define R_ 8u
#define S_ (2u * R_)
#define SBOX_ENTRIES 2048u
#define SMASK 0x7ff0u
#define SMASK2 ((ulong(SMASK) << 32) | ulong(SMASK))
#define NLOOP 684u
#define SBOX_BLOCKS (3u * SBOX_ENTRIES / 4u)

typedef device ulong* wptr; // word pointer; a block is 8 words

struct Lane { uint l; uint base; };   // lane in hash, first SIMD lane of this hash

// simd_shuffle has no 64-bit form: move the word as two 32-bit halves.
inline ulong from(ulong v, Lane L, uint src) { return as_type<ulong>(simd_shuffle(as_type<uint2>(v), ushort(L.base + src))); }

// ---- full-block helpers (every lane holds the whole block, e.g. for Salsa20)
struct Block { ulong w[8]; };
inline Block gather(ulong v, Lane L) { Block b; for (uint i = 0; i < 8; i++) b.w[i] = from(v, L, i); return b; }

inline void unshuffle(Block b, thread uint* w) {
    for (int q = 0; q < 4; q++) {
        uint l[4] = { uint(b.w[2 * q]), uint(b.w[2 * q] >> 32), uint(b.w[2 * q + 1]), uint(b.w[2 * q + 1] >> 32) };
        for (int k = 0; k < 4; k++) w[((q * 4 + k) * 5) % 16] = l[k];
    }
}
inline Block shuffle_words(thread const uint* w) {
    uint s[16];
    for (int i = 0; i < 16; i++) s[i] = w[(i * 5) % 16];
    Block b;
    for (int q = 0; q < 4; q++) {
        b.w[2 * q] = ulong(s[4 * q]) | (ulong(s[4 * q + 1]) << 32);
        b.w[2 * q + 1] = ulong(s[4 * q + 2]) | (ulong(s[4 * q + 3]) << 32);
    }
    return b;
}
// Salsa20/2 of the lanes' block; returns this lane's word of the result.
inline ulong salsa20_2(ulong v, Lane L) {
    uint x[16], b[16];
    unshuffle(gather(v, L), x);
    for (int i = 0; i < 16; i++) b[i] = x[i];
#define R(a, b_, c, s) x[a] ^= rotate(x[b_] + x[c], uint(s))
    R(4, 0, 12, 7);   R(8, 4, 0, 9);    R(12, 8, 4, 13);  R(0, 12, 8, 18);
    R(9, 5, 1, 7);    R(13, 9, 5, 9);   R(1, 13, 9, 13);  R(5, 1, 13, 18);
    R(14, 10, 6, 7);  R(2, 14, 10, 9);  R(6, 2, 14, 13);  R(10, 6, 2, 18);
    R(3, 15, 11, 7);  R(7, 3, 15, 9);   R(11, 7, 3, 13);  R(15, 11, 7, 18);
    R(1, 0, 3, 7);    R(2, 1, 0, 9);    R(3, 2, 1, 13);   R(0, 3, 2, 18);
    R(6, 5, 4, 7);    R(7, 6, 5, 9);    R(4, 7, 6, 13);   R(5, 4, 7, 18);
    R(11, 10, 9, 7);  R(8, 11, 10, 9);  R(9, 8, 11, 13);  R(10, 9, 8, 18);
    R(12, 15, 14, 7); R(13, 12, 15, 9); R(14, 13, 12, 13); R(15, 14, 13, 18);
#undef R
    for (int i = 0; i < 16; i++) x[i] += b[i];
    return shuffle_words(x).w[L.l];
}
inline uint integerify(ulong v, Lane L) { return uint(from(v, L, 0)); }

struct Sbox { wptr s0, s1, s2; uint w; };

inline ulong pwx_word(ulong x, ulong p0, ulong p1) {
    return ((x >> 32) * (x & 0xffffffffUL) + p0) ^ p1;
}

// Rare: exact sequential round, done redundantly by all 8 lanes on the full block.
inline ulong pwx_round_slow(ulong v, thread Sbox& sb, bool first, uint W, Lane L) {
    Block x = gather(v, L);
    for (uint k = 0; k < 4; k++) {
        ulong ak = x.w[2 * k] & SMASK2;
        uint lk = uint(ak) >> 4, hk = uint(ak >> 32) >> 4;
        ulong r0 = pwx_word(x.w[2 * k], sb.s0[lk * 2u], sb.s1[hk * 2u]);
        ulong r1 = pwx_word(x.w[2 * k + 1], sb.s0[lk * 2u + 1u], sb.s1[hk * 2u + 1u]);
        x.w[2 * k] = r0; x.w[2 * k + 1] = r1;
        if (first) { wptr t = (k & 1u) == 0 ? sb.s0 : sb.s1; t[(W + (k >> 1)) * 2u] = r0; t[(W + (k >> 1)) * 2u + 1u] = r1; }
        else if (k < 2u) { wptr t = k == 0 ? sb.s0 : sb.s1; t[W * 2u] = r0; t[W * 2u + 1u] = r1; }
    }
    return x.w[L.l];
}

// One pwxform round on this lane's word. `first`: round 0 (4 writes).
inline ulong pwx_round(ulong v, thread Sbox& sb, bool first, Lane L) {
    uint j = L.l >> 1, h = L.l & 1u;
    ulong a = from(v, L, 2u * j) & SMASK2;             // low word of vector j
    uint lo = uint(a) >> 4, hi = uint(a >> 32) >> 4;   // entry indices
    uint W = sb.w >> 4;
    // Conflict: a read by vector j hits an entry written earlier in this round.
    uint c = uint((j >= 1u && lo == W) || (j >= 2u && hi == W) || (first && j == 3u && lo == W + 1u));
    c |= simd_shuffle_xor(c, ushort(1)); c |= simd_shuffle_xor(c, ushort(2)); c |= simd_shuffle_xor(c, ushort(4));
    ulong r;
    if (!c) {
        r = pwx_word(v, sb.s0[lo * 2u + h], sb.s1[hi * 2u + h]);
        if (first) { wptr t = (j & 1u) == 0 ? sb.s0 : sb.s1; t[(W + (j >> 1)) * 2u + h] = r; }
        else if (j < 2u) { wptr t = j == 0 ? sb.s0 : sb.s1; t[W * 2u + h] = r; }
    } else {
        r = pwx_round_slow(v, sb, first, W, L);
    }
    // Entries written here are read by other lanes in later rounds.
    simdgroup_barrier(mem_flags::mem_device);
    return r;
}

inline ulong pwxform(ulong v, thread Sbox& sb, Lane L) {
    v = pwx_round(v, sb, true, L);  sb.w += 32u;
    v = pwx_round(v, sb, false, L); sb.w += 16u;
    v = pwx_round(v, sb, false, L); sb.w += 16u;
    sb.w &= SMASK;
    wptr s0 = sb.s0, s1 = sb.s1, s2 = sb.s2;
    sb.s0 = s2; sb.s1 = s0; sb.s2 = s1;
    return v;
}

#define BW(p, i) ((p) + 8u * (i))    // block i (8 words)

inline void salsa_mix(wptr bin, wptr bout, Lane L) {
    ulong x = BW(bin, 1)[L.l];
    for (uint i = 0; i < 2; i++) { x ^= BW(bin, i)[L.l]; x = salsa20_2(x, L); BW(bout, i)[L.l] = x; }
}
inline uint salsa_mix_xor(wptr b1, wptr b2, wptr bout, Lane L) {
    ulong x = BW(b1, 1)[L.l] ^ BW(b2, 1)[L.l];
    for (uint i = 0; i < 2; i++) { x ^= BW(b1, i)[L.l] ^ BW(b2, i)[L.l]; x = salsa20_2(x, L); BW(bout, i)[L.l] = x; }
    return integerify(x, L);
}
inline void pwx_mix(wptr bin, wptr bout, uint r, thread Sbox& sb, Lane L) {
    uint last = 2u * r - 1u;
    ulong x = BW(bin, last)[L.l];
    for (uint i = 0; i <= last; i++) {
        x ^= BW(bin, i)[L.l];
        x = pwxform(x, sb, L);
        if (i < last) BW(bout, i)[L.l] = x;
    }
    x = salsa20_2(x, L);
    BW(bout, last)[L.l] = x;
}
inline uint pwx_mix_xor(wptr b1, wptr b2, wptr bout, uint r, thread Sbox& sb, Lane L) {
    uint last = 2u * r - 1u;
    ulong x = BW(b1, last)[L.l] ^ BW(b2, last)[L.l];
    ulong n1 = BW(b1, 0)[L.l], n2 = BW(b2, 0)[L.l];
    for (uint i = 0; i <= last; i++) {
        ulong c1 = n1, c2 = n2;
        if (i < last) { n1 = BW(b1, i + 1)[L.l]; n2 = BW(b2, i + 1)[L.l]; }
        x ^= c1 ^ c2;
        x = pwxform(x, sb, L);
        if (i < last) BW(bout, i)[L.l] = x;
    }
    x = salsa20_2(x, L);
    BW(bout, last)[L.l] = x;
    return integerify(x, L);
}
inline uint pwx_mix_xor_save(wptr xio, wptr vj, thread Sbox& sb, Lane L) {
    const uint LAST = S_ - 1u;
    ulong x = BW(xio, LAST)[L.l] ^ BW(vj, LAST)[L.l];
    ulong nv = BW(vj, 0)[L.l], nx = BW(xio, 0)[L.l];
    for (uint i = 0; i <= LAST; i++) {
        ulong cv = nv, cx = nx;
        if (i < LAST) { nv = BW(vj, i + 1)[L.l]; nx = BW(xio, i + 1)[L.l]; }
        ulong y = cv ^ cx;
        BW(vj, i)[L.l] = y;
        x ^= y;
        x = pwxform(x, sb, L);
        if (i < LAST) BW(xio, i)[L.l] = x;
    }
    x = salsa20_2(x, L);
    BW(xio, LAST)[L.l] = x;
    return integerify(x, L);
}

// B (256 words) <-> blocks. Every lane converts the whole block and keeps its word.
inline void b_to_blocks(device uint* b, wptr out, uint count, Lane L) {
    for (uint i = 0; i < count; i++) { uint w[16]; for (int k = 0; k < 16; k++) w[k] = b[i * 16 + k]; BW(out, i)[L.l] = shuffle_words(w).w[L.l]; }
}
inline void blocks_to_b(wptr in, device uint* b, uint count, Lane L) {
    for (uint i = 0; i < count; i++) {
        uint w[16]; unshuffle(gather(BW(in, i)[L.l], L), w);
        if (L.l == 0) for (int k = 0; k < 16; k++) b[i * 16 + k] = w[k];
    }
}

template <bool PWX>
inline void smix1(device uint* b, uint r, uint n, wptr v, wptr out, thread Sbox& sb, Lane L) {
    uint s = 2u * r;
    b_to_blocks(b, v, 2, L);
    if (PWX) for (uint i = 1; i < r; i++) pwx_mix(BW(v, (i - 1) * 2), BW(v, i * 2), 1, sb, L);
    #define ENTRY(e) BW(v, (e) * s)
    if (PWX) { pwx_mix(ENTRY(0), ENTRY(1), r, sb, L); pwx_mix(ENTRY(1), ENTRY(2), r, sb, L); }
    else { salsa_mix(ENTRY(0), ENTRY(1), L); salsa_mix(ENTRY(1), ENTRY(2), L); }
    uint j = integerify(BW(v, 2u * s + s - 1u)[L.l], L);
    uint cur = 2, nn = 2;
    while (nn < n) {
        uint m = nn < n / 2 ? nn : n - 1u - nn;
        for (uint i = 1; i < m; i += 2) {
            uint jj = (j & (nn - 1u)) + i - 1u;
            j = PWX ? pwx_mix_xor(ENTRY(cur), ENTRY(jj), ENTRY(cur + 1), r, sb, L) : salsa_mix_xor(ENTRY(cur), ENTRY(jj), ENTRY(cur + 1), L);
            jj = (j & (nn - 1u)) + i;
            j = PWX ? pwx_mix_xor(ENTRY(cur + 1), ENTRY(jj), ENTRY(cur + 2), r, sb, L) : salsa_mix_xor(ENTRY(cur + 1), ENTRY(jj), ENTRY(cur + 2), L);
            cur += 2;
        }
        nn <<= 1;
    }
    nn >>= 1;
    uint jj = (j & (nn - 1u)) + n - 2u - nn;
    j = PWX ? pwx_mix_xor(ENTRY(cur), ENTRY(jj), ENTRY(cur + 1), r, sb, L) : salsa_mix_xor(ENTRY(cur), ENTRY(jj), ENTRY(cur + 1), L);
    jj = (j & (nn - 1u)) + n - 1u - nn;
    if (PWX) pwx_mix_xor(ENTRY(cur + 1), ENTRY(jj), out, r, sb, L); else salsa_mix_xor(ENTRY(cur + 1), ENTRY(jj), out, L);
    #undef ENTRY
    blocks_to_b(out, b, s, L);
}

inline void smix2(device uint* b, wptr v, wptr x, thread Sbox& sb, Lane L) {
    b_to_blocks(b, x, S_, L);
    uint j = integerify(BW(x, S_ - 1u)[L.l], L) & (N_ - 1u);
    for (uint it = 0; it < NLOOP; it++) j = pwx_mix_xor_save(x, BW(v, j * S_), sb, L) & (N_ - 1u);
    blocks_to_b(x, b, S_, L);
}

constant uint V_W = N_ * S_ * 8u;
constant uint S_W = SBOX_BLOCKS * 8u;
constant uint XY_W = S_ * 8u;
constant uint LANE_W = V_W + S_W + XY_W + 128u;   // + 1 KiB for b (128 ulong)

kernel void yespower_smix8(device const uint* in [[buffer(0)]],
                           device uint* out [[buffer(1)]],
                           device ulong* scratch [[buffer(2)]],
                           constant uint& count [[buffer(3)]],
                           uint gid [[thread_position_in_grid]],
                           uint sl [[thread_index_in_simdgroup]]) {
    uint t = gid >> 3;
    Lane L = { gid & 7u, sl & ~7u };
    if (t >= count) return;   // whole 8-lane groups exit together (count * 8 threads)
    wptr base = scratch + ulong(t) * LANE_W;
    wptr v = base, s = base + V_W, xy = s + S_W;
    device uint* b = (device uint*)(xy + XY_W);
    if (L.l == 0) for (int k = 0; k < 32; k++) b[k] = in[t * 32 + k];
    simdgroup_barrier(mem_flags::mem_device);
    Sbox none = { s, s, s, 0 };
    smix1<false>(b, 1, 3u * SBOX_ENTRIES * 16u / 128u, s, xy, none, L);
    simdgroup_barrier(mem_flags::mem_device);
    Sbox sb = { s, s + SBOX_ENTRIES * 2u, s + 4u * SBOX_ENTRIES, 0 };
    smix1<true>(b, R_, N_, v, xy, sb, L);
    simdgroup_barrier(mem_flags::mem_device);
    smix2(b, v, xy, sb, L);
    simdgroup_barrier(mem_flags::mem_device);
    if (L.l == 0) for (int k = 0; k < 16; k++) out[t * 16 + k] = b[240 + k];
}
