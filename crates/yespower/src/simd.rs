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
//! The eleven 128-bit operations yespower needs, per backend.
//!
//! Words are kept in the optimized C's "SIMD-shuffled" Salsa20 layout, so one
//! 64-byte block is four vectors and pwxform sees two 64-bit lanes per vector.

/// A 128-bit vector of four u32 lanes (equivalently two u64 lanes, little-endian).
pub trait Vec128: Copy {
    fn zero() -> Self;
    fn from_u32s(words: [u32; 4]) -> Self;
    fn to_u32s(self) -> [u32; 4];
    fn add32(self, other: Self) -> Self;
    fn xor(self, other: Self) -> Self;
    /// XOR that the optimizer may not re-associate with neighbouring XORs.
    /// BlockMix folds `b1 ^ b2` into X; computing `b1 ^ b2` apart keeps one XOR
    /// on X's critical path, but LLVM rewrites it back into `(X ^ b1) ^ b2`.
    #[inline(always)]
    fn xor_opaque(self, other: Self) -> Self {
        self.xor(other)
    }
    /// `self ^ rotl32(a + b, S)`, the Salsa20 ARX step.
    fn arx<const S: u32>(self, a: Self, b: Self) -> Self;
    /// pshufd 0x93: lanes (3, 0, 1, 2).
    fn shuf_93(self) -> Self;
    /// pshufd 0x4e: lanes (2, 3, 0, 1).
    fn shuf_4e(self) -> Self;
    /// pshufd 0x39: lanes (1, 2, 3, 0).
    fn shuf_39(self) -> Self;
    /// Low 64 bits.
    fn low64(self) -> u64;
    /// pwxform S-box byte offsets: the low 64-bit word ANDed with `mask2`
    /// (the 32-bit mask in both halves), split into (low half, high half).
    #[inline(always)]
    fn sbox_offsets(self, mask2: u64) -> (usize, usize) {
        let a = self.low64() & mask2;
        ((a as u32) as usize, (a >> 32) as usize)
    }
    /// Per 64-bit lane: `hi32 * lo32` as a 64-bit product (SSE2 `pmuludq(hi, x)`).
    fn mul_lohi(self) -> Self;
    fn add64(self, other: Self) -> Self;
    /// Scalar backends run Salsa20 on 16 plain words instead of emulated lanes.
    const SCALAR_SALSA: bool = false;
    /// Hint that the 64-byte line at `p` will be read soon (no-op where unsupported).
    #[inline(always)]
    fn prefetch(_p: *const Self) {}
    /// One pwxform lane step: `(hi * lo + S0[x.lo & mask]) ^ S1[x.hi & mask]`.
    ///
    /// # Safety
    /// `s0`/`s1` point to tables that every `mask2`-masked byte offset stays inside.
    #[inline(always)]
    unsafe fn pwx(self, s0: *const Self, s1: *const Self, mask2: u64) -> Self {
        // Byte offsets (16-byte aligned by the mask) keep address math to the load.
        // On x86 this is the C's `movq; and rax, Smask2; mov ecx, eax; shr rax, 32`
        // (one instruction fewer than masking each half: worth ~3.5% natively).
        let (lo, hi) = self.sbox_offsets(mask2);
        // SAFETY: the masked offsets stay inside the tables, per the caller.
        unsafe {
            let p0 = s0.cast::<u8>().add(lo).cast::<Self>();
            let p1 = s1.cast::<u8>().add(hi).cast::<Self>();
            self.mul_lohi().add64(*p0).xor(*p1)
        }
    }
}

/// Portable fallback: plain 64-bit integer code, correct everywhere.
///
/// Stored as two u64 words (u32 lanes 0|1 and 2|3), because pwxform, which
/// dominates the work, is natively 64-bit; Salsa20's 32-bit lane operations are
/// done on the halves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(C, align(16))]
pub struct Portable(pub [u64; 2]);

const LO32: u64 = 0xffff_ffff;

#[inline(always)]
fn add32x2(a: u64, b: u64) -> u64 {
    // Two independent 32-bit adds: mask the carry out of the low half.
    let lo = (a & LO32).wrapping_add(b & LO32) & LO32;
    let hi = (a >> 32).wrapping_add(b >> 32) << 32;
    lo | hi
}

#[inline(always)]
fn rotl32x2<const S: u32>(x: u64) -> u64 {
    let lo = (x as u32).rotate_left(S);
    let hi = ((x >> 32) as u32).rotate_left(S);
    u64::from(lo) | (u64::from(hi) << 32)
}

impl Vec128 for Portable {
    const SCALAR_SALSA: bool = true;
    #[inline(always)]
    fn zero() -> Self {
        Self([0; 2])
    }
    #[inline(always)]
    fn from_u32s(w: [u32; 4]) -> Self {
        Self([
            u64::from(w[0]) | (u64::from(w[1]) << 32),
            u64::from(w[2]) | (u64::from(w[3]) << 32),
        ])
    }
    #[inline(always)]
    fn to_u32s(self) -> [u32; 4] {
        let [a, b] = self.0;
        [a as u32, (a >> 32) as u32, b as u32, (b >> 32) as u32]
    }
    #[inline(always)]
    fn add32(self, o: Self) -> Self {
        Self([add32x2(self.0[0], o.0[0]), add32x2(self.0[1], o.0[1])])
    }
    #[inline(always)]
    fn xor(self, o: Self) -> Self {
        Self([self.0[0] ^ o.0[0], self.0[1] ^ o.0[1]])
    }
    #[inline(always)]
    fn arx<const S: u32>(self, a: Self, b: Self) -> Self {
        let t = a.add32(b);
        Self([
            self.0[0] ^ rotl32x2::<S>(t.0[0]),
            self.0[1] ^ rotl32x2::<S>(t.0[1]),
        ])
    }
    #[inline(always)]
    fn shuf_93(self) -> Self {
        // [l3, l0, l1, l2]
        let [q0, q1] = self.0;
        Self([(q1 >> 32) | (q0 << 32), (q0 >> 32) | (q1 << 32)])
    }
    #[inline(always)]
    fn shuf_4e(self) -> Self {
        // [l2, l3, l0, l1]
        Self([self.0[1], self.0[0]])
    }
    #[inline(always)]
    fn shuf_39(self) -> Self {
        // [l1, l2, l3, l0]
        let [q0, q1] = self.0;
        Self([(q0 >> 32) | (q1 << 32), (q1 >> 32) | (q0 << 32)])
    }
    #[inline(always)]
    fn low64(self) -> u64 {
        self.0[0]
    }
    #[inline(always)]
    fn mul_lohi(self) -> Self {
        Self(self.0.map(|x| (x >> 32) * (x & LO32)))
    }
    #[inline(always)]
    fn add64(self, o: Self) -> Self {
        Self([
            self.0[0].wrapping_add(o.0[0]),
            self.0[1].wrapping_add(o.0[1]),
        ])
    }
}

#[cfg(target_arch = "x86_64")]
mod sse2 {
    use super::Vec128;
    use core::arch::x86_64::*;

    /// x86-64 SSE2 (baseline on every x86-64 CPU).
    #[derive(Clone, Copy, Debug)]
    #[repr(transparent)]
    pub struct Sse2(pub __m128i);

    // SAFETY (all methods): SSE2 is part of the x86-64 baseline, so these
    // intrinsics are always available; they are pure register operations.
    #[allow(unsafe_code)]
    impl Vec128 for Sse2 {
        #[inline(always)]
        fn zero() -> Self {
            Self(unsafe { _mm_setzero_si128() })
        }
        #[inline(always)]
        fn from_u32s(w: [u32; 4]) -> Self {
            Self(unsafe { _mm_set_epi32(w[3] as i32, w[2] as i32, w[1] as i32, w[0] as i32) })
        }
        #[inline(always)]
        fn to_u32s(self) -> [u32; 4] {
            let mut out = [0u32; 4];
            unsafe { _mm_storeu_si128(out.as_mut_ptr().cast(), self.0) };
            out
        }
        #[inline(always)]
        fn add32(self, o: Self) -> Self {
            Self(unsafe { _mm_add_epi32(self.0, o.0) })
        }
        #[inline(always)]
        fn xor(self, o: Self) -> Self {
            Self(unsafe { _mm_xor_si128(self.0, o.0) })
        }
        #[inline(always)]
        fn xor_opaque(self, o: Self) -> Self {
            let mut t = self.0;
            // SAFETY: one register-only SSE2 instruction.
            unsafe {
                core::arch::asm!(
                    "pxor {t}, {o}",
                    t = inout(xmm_reg) t,
                    o = in(xmm_reg) o.0,
                    options(pure, nomem, nostack, preserves_flags),
                );
            }
            Self(t)
        }
        #[inline(always)]
        fn arx<const S: u32>(self, a: Self, b: Self) -> Self {
            unsafe {
                let t = _mm_add_epi32(a.0, b.0);
                let out = _mm_xor_si128(self.0, _mm_sll_epi32(t, _mm_cvtsi32_si128(S as i32)));
                Self(_mm_xor_si128(
                    out,
                    _mm_srl_epi32(t, _mm_cvtsi32_si128(32 - S as i32)),
                ))
            }
        }
        #[inline(always)]
        fn shuf_93(self) -> Self {
            Self(unsafe { _mm_shuffle_epi32::<0x93>(self.0) })
        }
        #[inline(always)]
        fn shuf_4e(self) -> Self {
            Self(unsafe { _mm_shuffle_epi32::<0x4e>(self.0) })
        }
        #[inline(always)]
        fn shuf_39(self) -> Self {
            Self(unsafe { _mm_shuffle_epi32::<0x39>(self.0) })
        }
        #[inline(always)]
        fn low64(self) -> u64 {
            unsafe { _mm_cvtsi128_si64(self.0) as u64 }
        }
        #[inline(always)]
        fn sbox_offsets(self, mask2: u64) -> (usize, usize) {
            // Same 4 instructions as the C's inline asm. Written in plain Rust,
            // LLVM re-splits the AND per half (5 instructions per pwxform step).
            let (lo, hi): (usize, usize);
            // SAFETY: register-only arithmetic; no memory access, stack or flags
            // contract beyond the declared operands (flags are clobbered by and/shr,
            // which asm! assumes by default).
            unsafe {
                core::arch::asm!(
                    "movq {hi}, {x}",
                    "and {hi}, {m}",
                    "mov {lo:e}, {hi:e}",
                    "shr {hi}, 32",
                    x = in(xmm_reg) self.0,
                    m = in(reg) mask2,
                    hi = out(reg) hi,
                    lo = out(reg) lo,
                    options(pure, nomem, nostack),
                );
            }
            (lo, hi)
        }
        #[inline(always)]
        fn mul_lohi(self) -> Self {
            Self(unsafe { _mm_mul_epu32(_mm_shuffle_epi32::<0xb1>(self.0), self.0) })
        }
        #[inline(always)]
        fn add64(self, o: Self) -> Self {
            Self(unsafe { _mm_add_epi64(self.0, o.0) })
        }
        #[inline(always)]
        fn prefetch(p: *const Self) {
            // Prefetch never faults, whatever the address.
            unsafe { _mm_prefetch::<_MM_HINT_T0>(p.cast()) }
        }
    }
}
#[cfg(target_arch = "x86_64")]
pub use sse2::Sse2;

#[cfg(target_arch = "aarch64")]
mod aarch64 {
    use super::{Portable, Vec128};

    /// AArch64 (Apple Silicon included): the portable 64-bit integer code, plus
    /// V_j prefetch and a pwxform step whose S-box loads use register offsets.
    ///
    /// NEON loses here: pwxform's critical path runs through a vector-to-GPR
    /// move and slower vector load/add/xor latencies (a full NEON backend was
    /// 13-15% slower than Portable on an M3 Max). Measured on M3 Max against
    /// Portable, K=1: +18% per thread, +11-13% on all 14 cores.
    ///
    /// Also measured and rejected on M3 Max: 128-byte scratch alignment (neutral;
    /// the 64-byte prefetch stride is what matters: 128 lost 10%), prefetching
    /// the next V entry (-7%), write prefetch of smix1's output (neutral to -3%),
    /// an immediate S-box mask (-8%), an opaque BlockMix XOR (-3%), a one-instruction
    /// `and h, mask, x, lsr #32` high offset (-0.7%), a 32-bit AND for the low offset
    /// (-2.8%), an unrolled prefetch loop (neutral), `pldl1strm` (-2%) and `stnp`
    /// V stores (-4%, all cores), NEON Salsa20/2 (-10%: vector latency on the serial
    /// chain) and smix1 without its 2x unroll (-0.7%, though 66 fewer stack
    /// accesses: the spills around Salsa20 cost nothing measurable); and builds with
    /// target-cpu=apple-m3 (neutral), fat LTO + panic=abort (-2.5%) and PGO (-10%).
    #[derive(Clone, Copy, Debug)]
    #[repr(transparent)]
    pub struct Aarch64(pub Portable);

    #[allow(unsafe_code)]
    impl Vec128 for Aarch64 {
        const SCALAR_SALSA: bool = true;
        #[inline(always)]
        fn zero() -> Self {
            Self(Portable::zero())
        }
        #[inline(always)]
        fn from_u32s(w: [u32; 4]) -> Self {
            Self(Portable::from_u32s(w))
        }
        #[inline(always)]
        fn to_u32s(self) -> [u32; 4] {
            self.0.to_u32s()
        }
        #[inline(always)]
        fn add32(self, o: Self) -> Self {
            Self(self.0.add32(o.0))
        }
        #[inline(always)]
        fn xor(self, o: Self) -> Self {
            Self(self.0.xor(o.0))
        }
        #[inline(always)]
        fn arx<const S: u32>(self, a: Self, b: Self) -> Self {
            Self(self.0.arx::<S>(a.0, b.0))
        }
        #[inline(always)]
        fn shuf_93(self) -> Self {
            Self(self.0.shuf_93())
        }
        #[inline(always)]
        fn shuf_4e(self) -> Self {
            Self(self.0.shuf_4e())
        }
        #[inline(always)]
        fn shuf_39(self) -> Self {
            Self(self.0.shuf_39())
        }
        #[inline(always)]
        fn low64(self) -> u64 {
            self.0.low64()
        }
        #[inline(always)]
        fn mul_lohi(self) -> Self {
            Self(self.0.mul_lohi())
        }
        #[inline(always)]
        fn add64(self, o: Self) -> Self {
            Self(self.0.add64(o.0))
        }
        #[inline(always)]
        fn prefetch(p: *const Self) {
            // SAFETY: PRFM is a hint; it never faults, whatever the address.
            // Into L1: an L2 hint measured 9% slower (M3 Max, all cores).
            unsafe {
                core::arch::asm!(
                    "prfm pldl1keep, [{p}]",
                    p = in(reg) p,
                    options(readonly, nostack, preserves_flags),
                );
            }
        }
        #[inline(always)]
        unsafe fn pwx(self, s0: *const Self, s1: *const Self, mask2: u64) -> Self {
            // LDP has no register-offset form, so LLVM emits `add; ldp` per S-box
            // entry. Two register-offset LDRs (second base 8 bytes on) take the add
            // off the critical path. The S0 word stays UMADDL's accumulator: M3
            // forwards it late (a separate umull + add measured 7-9% slower).
            let [mut q0, mut q1] = self.0.0;
            let (s0, s1) = (s0.cast::<u8>(), s1.cast::<u8>());
            let (s0b, s1b) = (s0.wrapping_add(8), s1.wrapping_add(8));
            // SAFETY: loads at s0/s1 plus the masked offsets (plus 8 for the high
            // word) stay inside the tables, per the caller; registers only otherwise.
            unsafe {
                core::arch::asm!(
                    "and {m}, {q0}, {mask}",
                    "lsr {h}, {m}, #32",
                    "ldr {a0}, [{s0}, {m:w}, uxtw]",
                    "ldr {a1}, [{s0b}, {m:w}, uxtw]",
                    "ldr {b0}, [{s1}, {h}]",
                    "ldr {b1}, [{s1b}, {h}]",
                    "lsr {t0}, {q0}, #32",
                    "lsr {t1}, {q1}, #32",
                    "umaddl {t0}, {t0:w}, {q0:w}, {a0}",
                    "umaddl {t1}, {t1:w}, {q1:w}, {a1}",
                    "eor {q0}, {t0}, {b0}",
                    "eor {q1}, {t1}, {b1}",
                    q0 = inout(reg) q0,
                    q1 = inout(reg) q1,
                    mask = in(reg) mask2,
                    s0 = in(reg) s0,
                    s1 = in(reg) s1,
                    s0b = in(reg) s0b,
                    s1b = in(reg) s1b,
                    m = out(reg) _,
                    h = out(reg) _,
                    a0 = out(reg) _,
                    a1 = out(reg) _,
                    b0 = out(reg) _,
                    b1 = out(reg) _,
                    t0 = out(reg) _,
                    t1 = out(reg) _,
                    options(pure, readonly, nostack, preserves_flags),
                );
            }
            Self(Portable([q0, q1]))
        }
    }
}
#[cfg(target_arch = "aarch64")]
pub use aarch64::Aarch64;

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
mod wasm {
    use super::Vec128;
    use core::arch::wasm32::*;

    /// WebAssembly SIMD128 (Chrome 91+, Firefox 89+, Safari 16.4+, Node 16.4+).
    #[derive(Clone, Copy, Debug)]
    #[repr(transparent)]
    pub struct Simd128(pub v128);

    impl Vec128 for Simd128 {
        #[inline(always)]
        fn zero() -> Self {
            Self(u32x4_splat(0))
        }
        #[inline(always)]
        fn from_u32s(w: [u32; 4]) -> Self {
            Self(u32x4(w[0], w[1], w[2], w[3]))
        }
        #[inline(always)]
        fn to_u32s(self) -> [u32; 4] {
            [
                u32x4_extract_lane::<0>(self.0),
                u32x4_extract_lane::<1>(self.0),
                u32x4_extract_lane::<2>(self.0),
                u32x4_extract_lane::<3>(self.0),
            ]
        }
        #[inline(always)]
        fn add32(self, o: Self) -> Self {
            Self(i32x4_add(self.0, o.0))
        }
        #[inline(always)]
        fn xor(self, o: Self) -> Self {
            Self(v128_xor(self.0, o.0))
        }
        #[inline(always)]
        fn arx<const S: u32>(self, a: Self, b: Self) -> Self {
            let t = i32x4_add(a.0, b.0);
            Self(v128_xor(
                v128_xor(self.0, i32x4_shl(t, S)),
                u32x4_shr(t, 32 - S),
            ))
        }
        #[inline(always)]
        fn shuf_93(self) -> Self {
            Self(i32x4_shuffle::<3, 0, 1, 2>(self.0, self.0))
        }
        #[inline(always)]
        fn shuf_4e(self) -> Self {
            Self(i32x4_shuffle::<2, 3, 0, 1>(self.0, self.0))
        }
        #[inline(always)]
        fn shuf_39(self) -> Self {
            Self(i32x4_shuffle::<1, 2, 3, 0>(self.0, self.0))
        }
        #[inline(always)]
        fn low64(self) -> u64 {
            u64x2_extract_lane::<0>(self.0)
        }
        #[inline(always)]
        fn sbox_offsets(self, mask2: u64) -> (usize, usize) {
            // Two i32 lane extracts (vmovd/vpextrd), each masked with an
            // immediate: 3% faster in V8 than one i64 extract + AND + shift,
            // which LLVM would otherwise merge this into.
            let lo = u32x4_extract_lane::<0>(self.0) & (mask2 as u32);
            let hi = u32x4_extract_lane::<1>(self.0) & ((mask2 >> 32) as u32);
            (lo as usize, hi as usize)
        }
        #[inline(always)]
        fn mul_lohi(self) -> Self {
            // Want [x0*x1, x2*x3] = pmuludq, which WASM spells i64x2.extmul_low
            // over [x0, x2] and [x1, x3]. LLVM rewrites a plain shuffle of the
            // even lanes into an AND mask, which breaks the extmul pattern and
            // falls back to an emulated i64x2.mul (3 pmuludq + 5 fixups in V8).
            // A constant swizzle is opaque to that rewrite.
            let even = i8x16_swizzle(
                self.0,
                i8x16(0, 1, 2, 3, 8, 9, 10, 11, 0, 1, 2, 3, 8, 9, 10, 11),
            );
            let odd = i32x4_shuffle::<1, 3, 1, 3>(self.0, self.0);
            Self(u64x2_extmul_low_u32x4(even, odd))
        }
        #[inline(always)]
        fn add64(self, o: Self) -> Self {
            Self(i64x2_add(self.0, o.0))
        }
    }
}
#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
pub use wasm::Simd128;

/// The fastest backend compiled into this build.
#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
pub type Best = Simd128;
#[cfg(target_arch = "x86_64")]
pub type Best = Sse2;
#[cfg(target_arch = "aarch64")]
pub type Best = Aarch64;
#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    all(target_arch = "wasm32", target_feature = "simd128")
)))]
pub type Best = Portable;

#[cfg(test)]
mod tests {
    use super::*;

    fn check<V: Vec128>() {
        let a = [0x8000_0001u32, 0xffff_fffe, 0x1234_5678, 0x9abc_def0];
        let b = [0x0f0f_0f0f, 0x0000_0003, 0xdead_beef, 0x0000_0001];
        let c = [5, 6, 7, 8];
        let (pa, pb, pc) = (
            Portable::from_u32s(a),
            Portable::from_u32s(b),
            Portable::from_u32s(c),
        );
        let (va, vb, vc) = (V::from_u32s(a), V::from_u32s(b), V::from_u32s(c));
        assert_eq!(va.to_u32s(), a);
        assert_eq!(va.add32(vb).to_u32s(), pa.add32(pb).to_u32s());
        assert_eq!(va.xor(vb).to_u32s(), pa.xor(pb).to_u32s());
        assert_eq!(vc.arx::<7>(va, vb).to_u32s(), pc.arx::<7>(pa, pb).to_u32s());
        assert_eq!(
            vc.arx::<18>(va, vb).to_u32s(),
            pc.arx::<18>(pa, pb).to_u32s()
        );
        assert_eq!(va.shuf_93().to_u32s(), [a[3], a[0], a[1], a[2]]);
        assert_eq!(va.shuf_4e().to_u32s(), [a[2], a[3], a[0], a[1]]);
        assert_eq!(va.shuf_39().to_u32s(), [a[1], a[2], a[3], a[0]]);
        assert_eq!(va.low64(), u64::from(a[0]) | u64::from(a[1]) << 32);
        assert_eq!(va.mul_lohi().to_u32s(), pa.mul_lohi().to_u32s());
        assert_eq!(
            pa.mul_lohi().0,
            [
                u64::from(a[0]) * u64::from(a[1]),
                u64::from(a[2]) * u64::from(a[3])
            ]
        );
        assert_eq!(va.add64(vb).to_u32s(), pa.add64(pb).to_u32s());
        assert_eq!(V::zero().to_u32s(), [0; 4]);
    }

    #[test]
    fn backends_agree_with_portable() {
        // Portable against hand-computed lane semantics, then every backend against it.
        let p = Portable::from_u32s([1, 2, 3, 4]);
        assert_eq!(p.shuf_93().to_u32s(), [4, 1, 2, 3]);
        assert_eq!(p.shuf_39().to_u32s(), [2, 3, 4, 1]);
        assert_eq!(
            Portable::from_u32s([u32::MAX, 1, 0x8000_0000, 7])
                .add32(Portable::from_u32s([1, 1, 0x8000_0000, 1]))
                .to_u32s(),
            [0, 2, 0, 8]
        );
        check::<Portable>();
        check::<Best>();
    }
}
