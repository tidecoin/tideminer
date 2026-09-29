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
//! yespower 1.0 core (N = 2048, r = 8), ported from Openwall's yespower-opt.c
//! pass 2, computing `K` independent hashes in lock-step.
//!
//! Every step loops over the K lanes innermost, so the compiler emits K
//! independent dependency chains side by side. yespower is bound by the latency
//! of data-dependent S-box loads; interleaving lanes hides that latency, which a
//! single hash cannot do. Lanes never share memory.
//!
//! Layout follows the optimized C: 64-byte blocks in the Salsa20 "SIMD-shuffled"
//! word order (`shuffled[i] = original[i * 5 % 16]`), four 128-bit vectors each.
#![allow(unsafe_code)]
// Lane loops index several per-lane arrays in lock-step by design.
#![allow(clippy::needless_range_loop)]

use crate::simd::Vec128;
use core::array::from_fn;

pub const N: u32 = 2048;
pub const R: usize = 8;
/// Blocks (64 bytes) per V entry: 2r.
const S: usize = 2 * R;
/// 16-byte entries per S-box table: 2^Swidth with Swidth = 11.
const SBOX_ENTRIES: usize = 1 << 11;
/// Byte mask applied to each 32-bit half before indexing S0/S1.
const SMASK: u32 = ((SBOX_ENTRIES as u32) - 1) * 16;
/// SMASK in both 32-bit halves of a 64-bit word.
const SMASK2: u64 = ((SMASK as u64) << 32) | SMASK as u64;
/// smix2 iterations: ceil(N / 3) rounded up to even.
const NLOOP: u32 = (N.div_ceil(3) + 1) & !1;
/// S-box memory = 3 tables, filled by a salsa-only smix1 with r = 1.
const SBOX_BLOCKS: usize = 3 * SBOX_ENTRIES / 4;

pub type Block<V> = [V; 4];

/// Blocks starting on a 64-byte cache-line boundary, like the C's page-aligned
/// scratch. Allocators put large buffers 16 bytes past a page boundary; then a
/// 1 KiB V entry spans 17 cache lines instead of 16, and every V_j read (the
/// accesses that miss L2) touches one line more.
struct CacheAligned<V> {
    buf: Vec<V>,
    start: usize,
}

impl<V: Vec128> CacheAligned<V> {
    fn new(blocks: usize) -> Self {
        const { assert!(core::mem::size_of::<V>() == 16 && core::mem::align_of::<V>() == 16) };
        let buf = vec![V::zero(); blocks * 4 + 3];
        let start = (64 - buf.as_ptr() as usize % 64) % 64 / 16;
        Self { buf, start }
    }

    /// The heap buffer never moves, so the pointer stays aligned and valid.
    fn blocks(&mut self) -> *mut Block<V> {
        self.buf[self.start..].as_mut_ptr().cast()
    }

    #[cfg(test)]
    fn address(&self) -> usize {
        self.buf[self.start..].as_ptr() as usize
    }
}

/// Owned scratch for one lane: V (2 MiB), S (96 KiB), XY and B.
pub struct Lane<V: Vec128> {
    v: CacheAligned<V>,
    s: CacheAligned<V>,
    xy: CacheAligned<V>,
    b: [u32; 256],
}

impl<V: Vec128> Lane<V> {
    pub fn new() -> Self {
        Self {
            v: CacheAligned::new(N as usize * S),
            s: CacheAligned::new(SBOX_BLOCKS),
            xy: CacheAligned::new(S),
            b: [0; 256],
        }
    }
}

impl<V: Vec128> Default for Lane<V> {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn load_block<V: Vec128>(words: &[u32]) -> Block<V> {
    let w: [u32; 16] = from_fn(|i| words[i * 5 % 16]);
    [
        V::from_u32s([w[0], w[1], w[2], w[3]]),
        V::from_u32s([w[4], w[5], w[6], w[7]]),
        V::from_u32s([w[8], w[9], w[10], w[11]]),
        V::from_u32s([w[12], w[13], w[14], w[15]]),
    ]
}

#[inline(always)]
fn store_block<V: Vec128>(block: &Block<V>, words: &mut [u32]) {
    for (q, v) in block.iter().enumerate() {
        for (l, word) in v.to_u32s().into_iter().enumerate() {
            words[(q * 4 + l) * 5 % 16] = word;
        }
    }
}

#[inline(always)]
fn xor_into<V: Vec128>(x: &mut Block<V>, y: &Block<V>) {
    for q in 0..4 {
        x[q] = x[q].xor(y[q]);
    }
}

#[inline(always)]
fn xor2<V: Vec128>(a: &Block<V>, b: &Block<V>) -> Block<V> {
    from_fn(|q| a[q].xor(b[q]))
}

/// Salsa20/2 on one block in plain words (scalar backends): un-shuffle, one
/// column+row double round, re-shuffle, add the input, as in the generic C.
#[inline(always)]
fn salsa20_2_words<V: Vec128>(block: &mut Block<V>) {
    let mut w = [0u32; 16];
    store_block(block, &mut w); // w = original (un-shuffled) word order
    let b = w;
    let x = &mut w;
    macro_rules! r {
        ($a:expr, $b:expr, $c:expr, $s:expr) => {
            x[$a] ^= x[$b].wrapping_add(x[$c]).rotate_left($s)
        };
    }
    r!(4, 0, 12, 7);
    r!(8, 4, 0, 9);
    r!(12, 8, 4, 13);
    r!(0, 12, 8, 18);
    r!(9, 5, 1, 7);
    r!(13, 9, 5, 9);
    r!(1, 13, 9, 13);
    r!(5, 1, 13, 18);
    r!(14, 10, 6, 7);
    r!(2, 14, 10, 9);
    r!(6, 2, 14, 13);
    r!(10, 6, 2, 18);
    r!(3, 15, 11, 7);
    r!(7, 3, 15, 9);
    r!(11, 7, 3, 13);
    r!(15, 11, 7, 18);
    r!(1, 0, 3, 7);
    r!(2, 1, 0, 9);
    r!(3, 2, 1, 13);
    r!(0, 3, 2, 18);
    r!(6, 5, 4, 7);
    r!(7, 6, 5, 9);
    r!(4, 7, 6, 13);
    r!(5, 4, 7, 18);
    r!(11, 10, 9, 7);
    r!(8, 11, 10, 9);
    r!(9, 8, 11, 13);
    r!(10, 9, 8, 18);
    r!(12, 15, 14, 7);
    r!(13, 12, 15, 9);
    r!(14, 13, 12, 13);
    r!(15, 14, 13, 18);
    for i in 0..16 {
        x[i] = x[i].wrapping_add(b[i]);
    }
    *block = load_block(x);
}

/// Salsa20/2 on each lane's block, in place: X = X + doubleround(X).
#[inline(always)]
fn salsa20_2<V: Vec128, const K: usize>(x: &mut [Block<V>; K]) {
    if V::SCALAR_SALSA {
        for block in x.iter_mut() {
            salsa20_2_words(block);
        }
        return;
    }
    let z = *x;
    // Columns.
    for k in 0..K {
        x[k][1] = x[k][1].arx::<7>(x[k][0], x[k][3]);
    }
    for k in 0..K {
        x[k][2] = x[k][2].arx::<9>(x[k][1], x[k][0]);
    }
    for k in 0..K {
        x[k][3] = x[k][3].arx::<13>(x[k][2], x[k][1]);
    }
    for k in 0..K {
        x[k][0] = x[k][0].arx::<18>(x[k][3], x[k][2]);
    }
    for k in 0..K {
        x[k][1] = x[k][1].shuf_93();
        x[k][2] = x[k][2].shuf_4e();
        x[k][3] = x[k][3].shuf_39();
    }
    // Rows.
    for k in 0..K {
        x[k][3] = x[k][3].arx::<7>(x[k][0], x[k][1]);
    }
    for k in 0..K {
        x[k][2] = x[k][2].arx::<9>(x[k][3], x[k][0]);
    }
    for k in 0..K {
        x[k][1] = x[k][1].arx::<13>(x[k][2], x[k][3]);
    }
    for k in 0..K {
        x[k][0] = x[k][0].arx::<18>(x[k][1], x[k][2]);
    }
    for k in 0..K {
        x[k][1] = x[k][1].shuf_39();
        x[k][2] = x[k][2].shuf_4e();
        x[k][3] = x[k][3].shuf_93();
    }
    for k in 0..K {
        for q in 0..4 {
            x[k][q] = x[k][q].add32(z[k][q]);
        }
    }
}

#[inline(always)]
fn integerify<V: Vec128>(x: &Block<V>) -> u32 {
    x[0].low64() as u32
}

/// Per-lane S-box state: three rotating 32 KiB tables and the write cursor.
#[derive(Clone, Copy)]
struct Sbox<V> {
    s0: *mut V,
    s1: *mut V,
    s2: *mut V,
    /// Write cursor in bytes (multiple of 16), as the C keeps it.
    w: usize,
    /// SMASK2, loaded opaquely so it lives in a register (see `pwx`).
    mask: u64,
}

/// One pwxform lane step: `(hi * lo + S0[x.lo & mask]) ^ S1[x.hi & mask]`.
///
/// # Safety
/// `s0`/`s1` point to tables of `SBOX_ENTRIES` vectors; the mask keeps both
/// indices below that.
#[inline(always)]
unsafe fn pwx<V: Vec128>(x: V, s0: *const V, s1: *const V, mask: u64) -> V {
    // SAFETY: masked offsets <= SMASK < SBOX_ENTRIES * 16; caller guarantees table size.
    unsafe { x.pwx(s0, s1, mask) }
}

/// pwxform for yespower 1.0: three rounds, S writes in rounds 0 (all four
/// lanes' halves) and 1-2 (first two), then rotate (S0, S1, S2) <- (S2, S0, S1).
///
/// # Safety
/// Each `Sbox` points into its own live, exclusively used S allocation.
#[inline(always)]
unsafe fn pwxform<V: Vec128, const K: usize>(x: &mut [Block<V>; K], sb: &mut [Sbox<V>; K]) {
    // SAFETY (all blocks): table pointers valid per caller; writes stay below
    // SBOX_ENTRIES * 16 bytes: w <= SMASK - 48 after masking (a multiple of 64) and
    // advances by at most 48 before the next mask.
    unsafe {
        // Lanes innermost: V8 emits instructions in source order, so this is
        // what puts K independent S-box lookups next to each other (measured:
        // lane-major order loses the K = 2 gain entirely in WASM).
        {
            for j in 0..4 {
                for k in 0..K {
                    let v = pwx(x[k][j], sb[k].s0, sb[k].s1, sb[k].mask);
                    x[k][j] = v;
                    let table = if j & 1 == 0 { sb[k].s0 } else { sb[k].s1 };
                    *table.cast::<u8>().add(sb[k].w + 16 * (j >> 1)).cast::<V>() = v;
                }
            }
            for k in 0..K {
                sb[k].w += 32;
            }
            for _ in 0..2 {
                for j in 0..4 {
                    for k in 0..K {
                        let v = pwx(x[k][j], sb[k].s0, sb[k].s1, sb[k].mask);
                        x[k][j] = v;
                        if j < 2 {
                            let table = if j == 0 { sb[k].s0 } else { sb[k].s1 };
                            *table.cast::<u8>().add(sb[k].w).cast::<V>() = v;
                        }
                    }
                }
                for k in 0..K {
                    sb[k].w += 16;
                }
            }
        }
    }
    for k in 0..K {
        sb[k].w &= SMASK as usize;
        let s = sb[k];
        sb[k].s0 = s.s2;
        sb[k].s1 = s.s0;
        sb[k].s2 = s.s1;
    }
}

type In<V, const K: usize> = [*const Block<V>; K];
type Out<V, const K: usize> = [*mut Block<V>; K];

/// BlockMix variants. `SalsaMix` fills the S-boxes (r = 1, no pwxform);
/// `PwxMix` is the main BlockMix_pwxform.
trait Mixer<V: Vec128, const K: usize> {
    /// Bout = BlockMix(Bin) over `r` 128-byte blocks.
    unsafe fn mix(&mut self, bin: In<V, K>, bout: Out<V, K>, r: usize);
    /// Bout = BlockMix(Bin1 ^ Bin2); returns Integerify(Bout).
    unsafe fn mix_xor(&mut self, b1: In<V, K>, b2: In<V, K>, bout: Out<V, K>, r: usize)
    -> [u32; K];
}

struct SalsaMix;

impl<V: Vec128, const K: usize> Mixer<V, K> for SalsaMix {
    #[inline(always)]
    unsafe fn mix(&mut self, bin: In<V, K>, bout: Out<V, K>, r: usize) {
        debug_assert_eq!(r, 1);
        // SAFETY: caller passes valid, non-overlapping 2-block regions per lane.
        unsafe {
            let mut x: [Block<V>; K] = from_fn(|k| *bin[k].add(1));
            for i in 0..2 {
                for k in 0..K {
                    xor_into(&mut x[k], &*bin[k].add(i));
                }
                salsa20_2(&mut x);
                for k in 0..K {
                    *bout[k].add(i) = x[k];
                }
            }
        }
    }

    #[inline(always)]
    unsafe fn mix_xor(
        &mut self,
        b1: In<V, K>,
        b2: In<V, K>,
        bout: Out<V, K>,
        r: usize,
    ) -> [u32; K] {
        debug_assert_eq!(r, 1);
        // SAFETY: as above.
        unsafe {
            let mut x: [Block<V>; K] = from_fn(|k| xor2(&*b1[k].add(1), &*b2[k].add(1)));
            for i in 0..2 {
                for k in 0..K {
                    // (b1 ^ b2) first: it only needs loads, so a single XOR remains
                    // on X's critical path (as GCC schedules the C).
                    let (p, q) = (&*b1[k].add(i), &*b2[k].add(i));
                    let t: Block<V> = from_fn(|n| p[n].xor_opaque(q[n]));
                    xor_into(&mut x[k], &t);
                }
                salsa20_2(&mut x);
                for k in 0..K {
                    *bout[k].add(i) = x[k];
                }
            }
            from_fn(|k| integerify(&x[k]))
        }
    }
}

struct PwxMix<V: Vec128, const K: usize> {
    sb: [Sbox<V>; K],
}

impl<V: Vec128, const K: usize> Mixer<V, K> for PwxMix<V, K> {
    #[inline(always)]
    unsafe fn mix(&mut self, bin: In<V, K>, bout: Out<V, K>, r: usize) {
        let last = 2 * r - 1;
        // Locals cannot alias the S-box stores, so the pointers and cursor stay
        // in registers instead of being reloaded after every write.
        let mut sb = self.sb;
        // SAFETY: valid 2r-block regions per lane; S-box pointers owned by self.
        unsafe {
            let mut x: [Block<V>; K] = from_fn(|k| *bin[k].add(last));
            for i in 0..=last {
                for k in 0..K {
                    xor_into(&mut x[k], &*bin[k].add(i));
                }
                pwxform(&mut x, &mut sb);
                if i < last {
                    for k in 0..K {
                        *bout[k].add(i) = x[k];
                    }
                }
            }
            self.sb = sb;
            salsa20_2(&mut x);
            for k in 0..K {
                *bout[k].add(last) = x[k];
            }
        }
    }

    #[inline(always)]
    unsafe fn mix_xor(
        &mut self,
        b1: In<V, K>,
        b2: In<V, K>,
        bout: Out<V, K>,
        r: usize,
    ) -> [u32; K] {
        let last = 2 * r - 1;
        // Locals cannot alias the S-box stores, so the pointers and cursor stay
        // in registers instead of being reloaded after every write.
        let mut sb = self.sb;
        // SAFETY: as above.
        unsafe {
            for k in 0..K {
                for i in 0..=last {
                    V::prefetch(b2[k].add(i).cast());
                }
            }
            let mut x: [Block<V>; K] = from_fn(|k| xor2(&*b1[k].add(last), &*b2[k].add(last)));
            for i in 0..=last {
                for k in 0..K {
                    // (b1 ^ b2) first: it only needs loads, so a single XOR remains
                    // on X's critical path (as GCC schedules the C).
                    let (p, q) = (&*b1[k].add(i), &*b2[k].add(i));
                    let t: Block<V> = from_fn(|n| p[n].xor_opaque(q[n]));
                    xor_into(&mut x[k], &t);
                }
                pwxform(&mut x, &mut sb);
                if i < last {
                    for k in 0..K {
                        *bout[k].add(i) = x[k];
                    }
                }
            }
            self.sb = sb;
            salsa20_2(&mut x);
            for k in 0..K {
                *bout[k].add(last) = x[k];
            }
            from_fn(|k| integerify(&x[k]))
        }
    }
}

impl<V: Vec128, const K: usize> PwxMix<V, K> {
    /// X = BlockMix(X ^ V_j) with V_j <- X ^ V_j saved (smix2 read-write loop).
    #[inline(always)]
    unsafe fn mix_xor_save(&mut self, x_io: Out<V, K>, vj: Out<V, K>) -> [u32; K] {
        const LAST: usize = S - 1;
        // Locals cannot alias the S-box stores, so the pointers and cursor stay
        // in registers instead of being reloaded after every write.
        let mut sb = self.sb;
        // SAFETY: x_io and vj are distinct 16-block regions per lane.
        unsafe {
            for k in 0..K {
                for i in 0..=LAST {
                    V::prefetch(vj[k].add(i).cast_const().cast());
                }
            }
            let mut x: [Block<V>; K] = from_fn(|k| xor2(&*x_io[k].add(LAST), &*vj[k].add(LAST)));
            for i in 0..=LAST {
                for k in 0..K {
                    let y = xor2(&*vj[k].add(i), &*x_io[k].add(i));
                    *vj[k].add(i) = y;
                    xor_into(&mut x[k], &y);
                }
                pwxform(&mut x, &mut sb);
                if i < LAST {
                    for k in 0..K {
                        *x_io[k].add(i) = x[k];
                    }
                }
            }
            self.sb = sb;
            salsa20_2(&mut x);
            for k in 0..K {
                *x_io[k].add(LAST) = x[k];
            }
            from_fn(|k| integerify(&x[k]))
        }
    }
}

/// First SMix loop: fill V sequentially, each entry mixing a data-dependent
/// earlier one (Wrap indexing). Reads the first 128 bytes of B, writes all 128r.
///
/// # Safety
/// `v[k]` holds `n * 2r` blocks and `out[k]` 2r blocks, all distinct per lane.
#[inline(always)]
unsafe fn smix1<V: Vec128, M: Mixer<V, K>, const K: usize>(
    b: &mut [&mut [u32; 256]; K],
    r: usize,
    n: u32,
    v: Out<V, K>,
    out: Out<V, K>,
    mixer: &mut M,
) {
    let s = 2 * r;
    // SAFETY: all offsets stay within v (entry < n) and out (2r blocks).
    unsafe {
        let entry = |e: usize| -> Out<V, K> { from_fn(|k| v[k].add(e * s)) };
        let at =
            |e: [u32; K]| -> In<V, K> { from_fn(|k| v[k].add(e[k] as usize * s).cast_const()) };
        let ro = |p: Out<V, K>| -> In<V, K> { p.map(|q| q.cast_const()) };
        for k in 0..K {
            for i in 0..2 {
                *v[k].add(i) = load_block(&b[k][i * 16..]);
            }
        }
        for i in 1..r {
            let src: In<V, K> = from_fn(|k| v[k].add((i - 1) * 2).cast_const());
            let dst: Out<V, K> = from_fn(|k| v[k].add(i * 2));
            mixer.mix(src, dst, 1);
        }
        mixer.mix(ro(entry(0)), entry(1), r);
        mixer.mix(ro(entry(1)), entry(2), r);
        let mut j: [u32; K] = from_fn(|k| integerify(&*v[k].add(2 * s + s - 1)));
        let mut cur = 2usize;
        let mut nn = 2u32;
        while nn < n {
            let m = if nn < n / 2 { nn } else { n - 1 - nn };
            let mut i = 1u32;
            while i < m {
                let jj = j.map(|j| (j & (nn - 1)) + i - 1);
                j = mixer.mix_xor(ro(entry(cur)), at(jj), entry(cur + 1), r);
                let jj = j.map(|j| (j & (nn - 1)) + i);
                j = mixer.mix_xor(ro(entry(cur + 1)), at(jj), entry(cur + 2), r);
                cur += 2;
                i += 2;
            }
            nn <<= 1;
        }
        nn >>= 1;
        let jj = j.map(|j| (j & (nn - 1)) + n - 2 - nn);
        j = mixer.mix_xor(ro(entry(cur)), at(jj), entry(cur + 1), r);
        let jj = j.map(|j| (j & (nn - 1)) + n - 1 - nn);
        mixer.mix_xor(ro(entry(cur + 1)), at(jj), out, r);
        for k in 0..K {
            for i in 0..s {
                store_block(&*out[k].add(i), &mut b[k][i * 16..]);
            }
        }
    }
}

/// Second SMix loop (read-write), `NLOOP` data-dependent iterations over V.
///
/// # Safety
/// As `smix1`, with `x[k]` a distinct 2r-block region.
#[inline(always)]
unsafe fn smix2<V: Vec128, const K: usize>(
    b: &mut [&mut [u32; 256]; K],
    v: Out<V, K>,
    x: Out<V, K>,
    mixer: &mut PwxMix<V, K>,
) {
    // SAFETY: j is masked below N; regions per caller.
    unsafe {
        for k in 0..K {
            for i in 0..S {
                *x[k].add(i) = load_block(&b[k][i * 16..]);
            }
        }
        let mut j: [u32; K] = from_fn(|k| integerify(&*x[k].add(S - 1)) & (N - 1));
        for _ in 0..NLOOP {
            let vj: Out<V, K> = from_fn(|k| v[k].add(j[k] as usize * S));
            j = mixer.mix_xor_save(x, vj).map(|j| j & (N - 1));
        }
        for k in 0..K {
            for i in 0..S {
                store_block(&*x[k].add(i), &mut b[k][i * 16..]);
            }
        }
    }
}

/// yespower 1.0, N = 2048, r = 8, no personalization, over K 80-byte inputs.
pub fn hash_lanes<V: Vec128, const K: usize>(
    lanes: &mut [Lane<V>; K],
    inputs: &[[u8; 80]; K],
) -> [[u8; 32]; K] {
    let mut prehash = [[0u8; 32]; K];
    for k in 0..K {
        let digest = crate::sha::sha256(&inputs[k]);
        // yespower 1.0 without personalization: PBKDF2 salt is empty. Only the
        // first 128 bytes of B are consumed (smix1 expands the rest).
        let mut b0 = [0u8; 128];
        crate::sha::pbkdf2_sha256_1(&digest, &[], &mut b0);
        prehash[k].copy_from_slice(&b0[..32]);
        for (word, bytes) in lanes[k].b.iter_mut().zip(b0.chunks_exact(4)) {
            *word = u32::from_le_bytes(bytes.try_into().unwrap());
        }
    }
    let v: Out<V, K> = from_fn(|k| lanes[k].v.blocks());
    let s: Out<V, K> = from_fn(|k| lanes[k].s.blocks());
    let xy: Out<V, K> = from_fn(|k| lanes[k].xy.blocks());
    // The raw pointers above target the lanes' heap buffers, never `b`.
    let mut b: [&mut [u32; 256]; K] = lanes.each_mut().map(|lane| &mut lane.b);
    // SAFETY: every pointer targets its own lane's buffers of the sizes the
    // kernel requires; lanes are distinct and outlive this call.
    unsafe {
        smix1(
            &mut b,
            1,
            (3 * SBOX_ENTRIES * 16 / 128) as u32,
            s,
            xy,
            &mut SalsaMix,
        );
        // x86: an opaque mask keeps the C's single 64-bit AND (see Sse2 backend).
        // Elsewhere the constant lets each half use an immediate.
        let mask = if cfg!(target_arch = "x86_64") {
            core::hint::black_box(SMASK2)
        } else {
            SMASK2
        };
        let mut pwx = PwxMix {
            sb: from_fn(|k| {
                let base = s[k].cast::<V>();
                Sbox {
                    s0: base,
                    s1: base.add(SBOX_ENTRIES),
                    s2: base.add(2 * SBOX_ENTRIES),
                    w: 0,
                    mask,
                }
            }),
        };
        smix1(&mut b, R, N, v, xy, &mut pwx);
        smix2(&mut b, v, xy, &mut pwx);
    }
    from_fn(|k| {
        let mut key = [0u8; 64];
        for (bytes, word) in key.chunks_exact_mut(4).zip(&b[k][240..]) {
            bytes.copy_from_slice(&word.to_le_bytes());
        }
        crate::sha::hmac_sha256(&key, &prehash[k])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simd::Best;

    #[test]
    fn scratch_is_cache_line_aligned() {
        for _ in 0..8 {
            let lane = Lane::<Best>::new();
            assert_eq!(lane.v.address() % 64, 0);
            assert_eq!(lane.s.address() % 64, 0);
            assert_eq!(lane.xy.address() % 64, 0);
        }
    }
}
