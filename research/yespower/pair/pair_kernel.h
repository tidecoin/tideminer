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
/*
 * Research-only pair kernel: interleave two independent Tidecoin yespower
 * hashes inside a single thread so that the out-of-order engine can overlap
 * two dependent S-box chains instead of one.
 *
 * Included at the end of yespower-opt.c, in the pass-2 preprocessing context
 * (v1.0 macros SALSA20_2 / PWXFORM-with-writes are in scope).
 */
#ifndef _YESPOWER_PAIR_KERNEL_H_
#define _YESPOWER_PAIR_KERNEL_H_

typedef struct {
	__m128i X0, X1, X2, X3;
	uint8_t *S0, *S1, *S2;
	size_t w;
} pwx_pair_state_t;

static void pair_state_init(pwx_pair_state_t *p, pwxform_ctx_t *c)
{
	p->S0 = c->S0;
	p->S1 = c->S1;
	p->S2 = c->S2;
	p->w = c->w;
}

static void pair_state_store(pwx_pair_state_t *p, pwxform_ctx_t *c)
{
	c->S0 = p->S0;
	c->S1 = p->S1;
	c->S2 = p->S2;
	c->w = p->w;
}

#define PAIR_PWX_SIMD(X, SP0, SP1) { \
	uint64_t xp; \
	uint32_t lop = xp = EXTRACT64(X) & 0x7ff000007ff0ULL; \
	uint32_t hip = xp >> 32; \
	X = _mm_mul_epu32(HI32(X), X); \
	X = _mm_add_epi64(X, *(__m128i *)((SP0) + lop)); \
	X = _mm_xor_si128(X, *(__m128i *)((SP1) + hip)); \
}

#define PAIR_PWX_RW(p, X, SW) { \
	PAIR_PWX_SIMD(X, (p)->S0, (p)->S1) \
	*(__m128i *)((SW) + (p)->w) = X; \
}

#define PAIR_PWX_ROUND4(p) { \
	PAIR_PWX_RW(p, (p)->X0, (p)->S0) \
	PAIR_PWX_RW(p, (p)->X1, (p)->S1) \
	(p)->w += 16; \
	PAIR_PWX_RW(p, (p)->X2, (p)->S0) \
	PAIR_PWX_RW(p, (p)->X3, (p)->S1) \
	(p)->w += 16; \
}

#define PAIR_PWX_ROUND2(p) { \
	PAIR_PWX_RW(p, (p)->X0, (p)->S0) \
	PAIR_PWX_RW(p, (p)->X1, (p)->S1) \
	(p)->w += 16; \
	PAIR_PWX_SIMD((p)->X2, (p)->S0, (p)->S1) \
	PAIR_PWX_SIMD((p)->X3, (p)->S0, (p)->S1) \
}

#define PAIR_ROTATE(p) { \
	uint8_t *pt = (p)->S2; \
	(p)->S2 = (p)->S1; \
	(p)->S1 = (p)->S0; \
	(p)->S0 = pt; \
}

/* Interleave the two contexts at round granularity. Per-context order is
 * identical to the single-hash PWXFORM (write4, write2, write2, wrap, rotate). */
static inline __attribute__((always_inline)) void pair_pwx_step2(pwx_pair_state_t *a, pwx_pair_state_t *b)
{
	PAIR_PWX_ROUND4(a)
	PAIR_PWX_ROUND4(b)
	PAIR_PWX_ROUND2(a)
	PAIR_PWX_ROUND2(b)
	PAIR_PWX_ROUND2(a)
	PAIR_PWX_ROUND2(b)
	a->w &= 0x7ff0;
	b->w &= 0x7ff0;
	PAIR_ROTATE(a)
	PAIR_ROTATE(b)
}

static inline __attribute__((always_inline)) void pair_xor_x(pwx_pair_state_t *p, const salsa20_blk_t *in,
    size_t i)
{
	p->X0 = _mm_xor_si128(p->X0, in[i].q[0]);
	p->X1 = _mm_xor_si128(p->X1, in[i].q[1]);
	p->X2 = _mm_xor_si128(p->X2, in[i].q[2]);
	p->X3 = _mm_xor_si128(p->X3, in[i].q[3]);
}

static inline __attribute__((always_inline)) void pair_xor_x2(pwx_pair_state_t *p,
    const salsa20_blk_t *in1, const salsa20_blk_t *in2, size_t i)
{
	p->X0 = _mm_xor_si128(in1[i].q[0], in2[i].q[0]);
	p->X1 = _mm_xor_si128(in1[i].q[1], in2[i].q[1]);
	p->X2 = _mm_xor_si128(in1[i].q[2], in2[i].q[2]);
	p->X3 = _mm_xor_si128(in1[i].q[3], in2[i].q[3]);
}

/* inout[i] ^= in[i] is written back; X ^= the new inout[i]. */
static inline __attribute__((always_inline)) void pair_xor_write_xor(pwx_pair_state_t *p,
    salsa20_blk_t *inout, const salsa20_blk_t *in, size_t i)
{
	__m128i y0 = _mm_xor_si128(inout[i].q[0], in[i].q[0]);
	__m128i y1 = _mm_xor_si128(inout[i].q[1], in[i].q[1]);
	__m128i y2 = _mm_xor_si128(inout[i].q[2], in[i].q[2]);
	__m128i y3 = _mm_xor_si128(inout[i].q[3], in[i].q[3]);
	inout[i].q[0] = y0;
	inout[i].q[1] = y1;
	inout[i].q[2] = y2;
	inout[i].q[3] = y3;
	p->X0 = _mm_xor_si128(p->X0, y0);
	p->X1 = _mm_xor_si128(p->X1, y1);
	p->X2 = _mm_xor_si128(p->X2, y2);
	p->X3 = _mm_xor_si128(p->X3, y3);
}

static inline __attribute__((always_inline)) void pair_write_x(pwx_pair_state_t *p, salsa20_blk_t *out,
    size_t i)
{
	out[i].q[0] = p->X0;
	out[i].q[1] = p->X1;
	out[i].q[2] = p->X2;
	out[i].q[3] = p->X3;
}

static inline __attribute__((always_inline)) void pair_read_x(pwx_pair_state_t *p, const salsa20_blk_t *in,
    size_t i)
{
	p->X0 = in[i].q[0];
	p->X1 = in[i].q[1];
	p->X2 = in[i].q[2];
	p->X3 = in[i].q[3];
}

/* SALSA20_2 on this context's X; the resulting X stays in the state. */
static inline __attribute__((always_inline)) void pair_salsa20_2(pwx_pair_state_t *p, salsa20_blk_t *out)
{
	__m128i X0 = p->X0, X1 = p->X1, X2 = p->X2, X3 = p->X3;
	SALSA20(*out);
	p->X0 = X0;
	p->X1 = X1;
	p->X2 = X2;
	p->X3 = X3;
}

static inline __attribute__((always_inline)) void pair_blockmix(
    const salsa20_blk_t *Ain, salsa20_blk_t *Aout,
    const salsa20_blk_t *Bin, salsa20_blk_t *Bout,
    size_t r, pwx_pair_state_t *pa, pwx_pair_state_t *pb)
{
	size_t i;

	r = r * 2 - 1;

	pair_read_x(pa, Ain, r);
	pair_read_x(pb, Bin, r);

	i = 0;
	do {
		pair_xor_x(pa, Ain, i);
		pair_xor_x(pb, Bin, i);
		pair_pwx_step2(pa, pb);
		if (unlikely(i >= r))
			break;
		pair_write_x(pa, Aout, i);
		pair_write_x(pb, Bout, i);
		i++;
	} while (1);

	pair_salsa20_2(pa, &Aout[i]);
	pair_salsa20_2(pb, &Bout[i]);
}

/* Both contexts must have the same r; A's integerify is not returned because
 * both post-SALSA X words are available in the states. */
static inline __attribute__((always_inline)) void pair_blockmix_xor(
    const salsa20_blk_t *A1, const salsa20_blk_t *A2, salsa20_blk_t *Aout,
    const salsa20_blk_t *B1, const salsa20_blk_t *B2, salsa20_blk_t *Bout,
    size_t r, pwx_pair_state_t *pa, pwx_pair_state_t *pb)
{
	size_t i;

	r = r * 2 - 1;

#ifdef PREFETCH
	PREFETCH(&A2[r], _MM_HINT_T0)
	for (i = 0; i < r; i++)
		PREFETCH(&A2[i], _MM_HINT_T0)
	PREFETCH(&B2[r], _MM_HINT_T0)
	for (i = 0; i < r; i++)
		PREFETCH(&B2[i], _MM_HINT_T0)
#endif

	pair_xor_x2(pa, A1, A2, r);
	pair_xor_x2(pb, B1, B2, r);

	i = 0;
	r--;
	do {
		pair_xor_x(pa, A1, i);
		pair_xor_x(pa, A2, i);
		pair_xor_x(pb, B1, i);
		pair_xor_x(pb, B2, i);
		pair_pwx_step2(pa, pb);
		pair_write_x(pa, Aout, i);
		pair_write_x(pb, Bout, i);

		pair_xor_x(pa, A1, i + 1);
		pair_xor_x(pa, A2, i + 1);
		pair_xor_x(pb, B1, i + 1);
		pair_xor_x(pb, B2, i + 1);
		pair_pwx_step2(pa, pb);

		if (unlikely(i >= r))
			break;

		pair_write_x(pa, Aout, i + 1);
		pair_write_x(pb, Bout, i + 1);

		i += 2;
	} while (1);
	i++;

	pair_salsa20_2(pa, &Aout[i]);
	pair_salsa20_2(pb, &Bout[i]);
}

static inline __attribute__((always_inline)) void pair_blockmix_xor_save(
    salsa20_blk_t *A1out, salsa20_blk_t *A2,
    salsa20_blk_t *B1out, salsa20_blk_t *B2,
    size_t r, pwx_pair_state_t *pa, pwx_pair_state_t *pb)
{
	size_t i;

	r = r * 2 - 1;

#ifdef PREFETCH
	PREFETCH(&A2[r], _MM_HINT_T0)
	for (i = 0; i < r; i++)
		PREFETCH(&A2[i], _MM_HINT_T0)
	PREFETCH(&B2[r], _MM_HINT_T0)
	for (i = 0; i < r; i++)
		PREFETCH(&B2[i], _MM_HINT_T0)
#endif

	pair_xor_x2(pa, A1out, A2, r);
	pair_xor_x2(pb, B1out, B2, r);

	i = 0;
	r--;
	do {
		pair_xor_write_xor(pa, A2, A1out, i);
		pair_xor_write_xor(pb, B2, B1out, i);
		pair_pwx_step2(pa, pb);
		pair_write_x(pa, A1out, i);
		pair_write_x(pb, B1out, i);

		pair_xor_write_xor(pa, A2, A1out, i + 1);
		pair_xor_write_xor(pb, B2, B1out, i + 1);
		pair_pwx_step2(pa, pb);

		if (unlikely(i >= r))
			break;

		pair_write_x(pa, A1out, i + 1);
		pair_write_x(pb, B1out, i + 1);

		i += 2;
	} while (1);
	i++;

	pair_salsa20_2(pa, &A1out[i]);
	pair_salsa20_2(pb, &B1out[i]);
}

/* Pair version of smix1. Each context keeps its own V/XY and pointer walk. */
static inline __attribute__((always_inline)) void smix1_pair(
    uint8_t *AB, size_t r, uint32_t N,
    uint8_t *BB, salsa20_blk_t *AV, salsa20_blk_t *AXY,
    salsa20_blk_t *BV, salsa20_blk_t *BXY,
    pwxform_ctx_t *actx, pwxform_ctx_t *bctx)
{
	size_t s = 2 * r;
	salsa20_blk_t *AX = AV, *AY = &AV[s], *AV_j;
	salsa20_blk_t *BX = BV, *BY = &BV[s], *BV_j;
	uint32_t i, jA, jB, n;
	pwx_pair_state_t pa, pb;

	for (i = 0; i < 2; i++) {
		const salsa20_blk_t *src = (salsa20_blk_t *)&AB[i * 64];
		salsa20_blk_t *tmp = AY;
		salsa20_blk_t *dst = &AX[i];
		size_t k;
		for (k = 0; k < 16; k++)
			tmp->w[k] = le32dec(&src->w[k]);
		salsa20_simd_shuffle(tmp, dst);
	}
	for (i = 0; i < 2; i++) {
		const salsa20_blk_t *src = (salsa20_blk_t *)&BB[i * 64];
		salsa20_blk_t *tmp = BY;
		salsa20_blk_t *dst = &BX[i];
		size_t k;
		for (k = 0; k < 16; k++)
			tmp->w[k] = le32dec(&src->w[k]);
		salsa20_simd_shuffle(tmp, dst);
	}

	pair_state_init(&pa, actx);
	pair_state_init(&pb, bctx);

	for (i = 1; i < r; i++)
		pair_blockmix(&AX[(i - 1) * 2], &AX[i * 2],
		    &BX[(i - 1) * 2], &BX[i * 2], 1, &pa, &pb);

	pair_blockmix(AX, AY, BX, BY, r, &pa, &pb);
	AX = AY + s;
	BX = BY + s;
	pair_blockmix(AY, AX, BY, BX, r, &pa, &pb);
	jA = (uint32_t)AX[r * 2 - 1].d[0];
	jB = (uint32_t)BX[r * 2 - 1].d[0];

	for (n = 2; n < N; n <<= 1) {
		uint32_t m = (n < N / 2) ? n : (N - 1 - n);
		for (i = 1; i < m; i += 2) {
			AY = AX + s;
			BY = BX + s;
			jA &= n - 1;
			jA += i - 1;
			jB &= n - 1;
			jB += i - 1;
			AV_j = &AV[jA * s];
			BV_j = &BV[jB * s];
			pair_blockmix_xor(AX, AV_j, AY, BX, BV_j, BY,
			    r, &pa, &pb);
			jA = (uint32_t)_mm_cvtsi128_si32(pa.X0);
			jB = (uint32_t)_mm_cvtsi128_si32(pb.X0);
			jA &= n - 1;
			jA += i;
			jB &= n - 1;
			jB += i;
			AV_j = &AV[jA * s];
			BV_j = &BV[jB * s];
			AX = AY + s;
			BX = BY + s;
			pair_blockmix_xor(AY, AV_j, AX, BY, BV_j, BX,
			    r, &pa, &pb);
			jA = (uint32_t)_mm_cvtsi128_si32(pa.X0);
			jB = (uint32_t)_mm_cvtsi128_si32(pb.X0);
		}
	}
	n >>= 1;

	jA &= n - 1;
	jA += N - 2 - n;
	jB &= n - 1;
	jB += N - 2 - n;
	AV_j = &AV[jA * s];
	BV_j = &BV[jB * s];
	AY = AX + s;
	BY = BX + s;
	pair_blockmix_xor(AX, AV_j, AY, BX, BV_j, BY, r, &pa, &pb);
	jA = (uint32_t)_mm_cvtsi128_si32(pa.X0);
	jB = (uint32_t)_mm_cvtsi128_si32(pb.X0);
	jA &= n - 1;
	jA += N - 1 - n;
	jB &= n - 1;
	jB += N - 1 - n;
	AV_j = &AV[jA * s];
	BV_j = &BV[jB * s];
	pair_blockmix_xor(AY, AV_j, AXY, BY, BV_j, BXY, r, &pa, &pb);

	pair_state_store(&pa, actx);
	pair_state_store(&pb, bctx);

	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = &AXY[i];
		salsa20_blk_t *tmp = &AXY[s];
		salsa20_blk_t *dst = (salsa20_blk_t *)&AB[i * 64];
		size_t k;
		for (k = 0; k < 16; k++)
			le32enc(&tmp->w[k], src->w[k]);
		salsa20_simd_unshuffle(tmp, dst);
	}
	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = &BXY[i];
		salsa20_blk_t *tmp = &BXY[s];
		salsa20_blk_t *dst = (salsa20_blk_t *)&BB[i * 64];
		size_t k;
		for (k = 0; k < 16; k++)
			le32enc(&tmp->w[k], src->w[k]);
		salsa20_simd_unshuffle(tmp, dst);
	}
}

/* Pair version of smix2: 684 read/write iterations for Tidecoin. */
static inline __attribute__((always_inline)) void smix2_pair(
    uint8_t *AB, size_t r, uint32_t N, uint32_t Nloop,
    uint8_t *BB, salsa20_blk_t *AV, salsa20_blk_t *AXY,
    salsa20_blk_t *BV, salsa20_blk_t *BXY,
    pwxform_ctx_t *actx, pwxform_ctx_t *bctx)
{
	size_t s = 2 * r;
	salsa20_blk_t *AX = AXY, *AY = &AXY[s];
	salsa20_blk_t *BX = BXY, *BY = &BXY[s];
	uint32_t i, jA, jB;
	pwx_pair_state_t pa, pb;

	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = (salsa20_blk_t *)&AB[i * 64];
		salsa20_blk_t *tmp = AY;
		salsa20_blk_t *dst = &AX[i];
		size_t k;
		for (k = 0; k < 16; k++)
			tmp->w[k] = le32dec(&src->w[k]);
		salsa20_simd_shuffle(tmp, dst);
	}
	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = (salsa20_blk_t *)&BB[i * 64];
		salsa20_blk_t *tmp = BY;
		salsa20_blk_t *dst = &BX[i];
		size_t k;
		for (k = 0; k < 16; k++)
			tmp->w[k] = le32dec(&src->w[k]);
		salsa20_simd_shuffle(tmp, dst);
	}

	pair_state_init(&pa, actx);
	pair_state_init(&pb, bctx);

	jA = (uint32_t)AX[r * 2 - 1].d[0] & (N - 1);
	jB = (uint32_t)BX[r * 2 - 1].d[0] & (N - 1);

	do {
		salsa20_blk_t *AV_j = &AV[jA * s];
		salsa20_blk_t *BV_j = &BV[jB * s];
		pair_blockmix_xor_save(AX, AV_j, BX, BV_j, r, &pa, &pb);
		jA = (uint32_t)_mm_cvtsi128_si32(pa.X0) & (N - 1);
		jB = (uint32_t)_mm_cvtsi128_si32(pb.X0) & (N - 1);
		AV_j = &AV[jA * s];
		BV_j = &BV[jB * s];
		pair_blockmix_xor_save(AX, AV_j, BX, BV_j, r, &pa, &pb);
		jA = (uint32_t)_mm_cvtsi128_si32(pa.X0) & (N - 1);
		jB = (uint32_t)_mm_cvtsi128_si32(pb.X0) & (N - 1);
	} while (Nloop -= 2);

	pair_state_store(&pa, actx);
	pair_state_store(&pb, bctx);

	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = &AX[i];
		salsa20_blk_t *tmp = AY;
		salsa20_blk_t *dst = (salsa20_blk_t *)&AB[i * 64];
		size_t k;
		for (k = 0; k < 16; k++)
			le32enc(&tmp->w[k], src->w[k]);
		salsa20_simd_unshuffle(tmp, dst);
	}
	for (i = 0; i < 2 * r; i++) {
		const salsa20_blk_t *src = &BX[i];
		salsa20_blk_t *tmp = BY;
		salsa20_blk_t *dst = (salsa20_blk_t *)&BB[i * 64];
		size_t k;
		for (k = 0; k < 16; k++)
			le32enc(&tmp->w[k], src->w[k]);
		salsa20_simd_unshuffle(tmp, dst);
	}
}

/* Full two-hash entry. Mirrors yespower() for YESPOWER_1_0 only. */
int yespower_pair(yespower_local_t *la, yespower_local_t *lb,
    const uint8_t *srca, const uint8_t *srcb, size_t srclen,
    const yespower_params_t *params,
    yespower_binary_t *dsta, yespower_binary_t *dstb)
{
	uint32_t N = params->N;
	uint32_t r = params->r;
	const uint8_t *pers = params->pers;
	size_t perslen = params->perslen;
	size_t B_size, V_size, XY_size, need;
	uint8_t *AB, *BB, *AS, *BS;
	salsa20_blk_t *AV, *AXY, *BV, *BXY;
	pwxform_ctx_t actx, bctx;
	uint8_t sha256a[32], sha256b[32];
	const uint8_t *psrc;
	size_t plen;

	if (params->version != YESPOWER_1_0 ||
	    N < 1024 || N > 512 * 1024 || r < 8 || r > 32 ||
	    (N & (N - 1)) != 0 || (!pers && perslen)) {
		errno = EINVAL;
		goto fail;
	}

	B_size = (size_t)128 * r;
	V_size = B_size * N;
	XY_size = B_size + 64;
	actx.Sbytes = bctx.Sbytes = 3 * Swidth_to_Sbytes1(Swidth_1_0);
	need = B_size + V_size + XY_size + actx.Sbytes;

	if (la->aligned_size < need) {
		if (free_region(la) || !alloc_region(la, need))
			goto fail;
	}
	if (lb->aligned_size < need) {
		if (free_region(lb) || !alloc_region(lb, need))
			goto fail;
	}
	AB = (uint8_t *)la->aligned;
	AV = (salsa20_blk_t *)(AB + B_size);
	AXY = (salsa20_blk_t *)((uint8_t *)AV + V_size);
	AS = (uint8_t *)AXY + XY_size;
	actx.S0 = AS;
	actx.S1 = AS + Swidth_to_Sbytes1(Swidth_1_0);
	actx.S2 = AS + 2 * Swidth_to_Sbytes1(Swidth_1_0);
	actx.w = 0;

	BB = (uint8_t *)lb->aligned;
	BV = (salsa20_blk_t *)(BB + B_size);
	BXY = (salsa20_blk_t *)((uint8_t *)BV + V_size);
	BS = (uint8_t *)BXY + XY_size;
	bctx.S0 = BS;
	bctx.S1 = BS + Swidth_to_Sbytes1(Swidth_1_0);
	bctx.S2 = BS + 2 * Swidth_to_Sbytes1(Swidth_1_0);
	bctx.w = 0;

	SHA256_Buf(srca, srclen, sha256a);
	SHA256_Buf(srcb, srclen, sha256b);

	if (pers) { psrc = pers; plen = perslen; }
	else { psrc = srca; plen = 0; }
	PBKDF2_SHA256(sha256a, sizeof(sha256a), psrc, plen, 1, AB, 128);
	if (pers) { psrc = pers; plen = perslen; }
	else { psrc = srcb; plen = 0; }
	PBKDF2_SHA256(sha256b, sizeof(sha256b), psrc, plen, 1, BB, 128);
	memcpy(sha256a, AB, sizeof(sha256a));
	memcpy(sha256b, BB, sizeof(sha256b));

	smix1(AB, 1, actx.Sbytes / 128, (salsa20_blk_t *)actx.S0, AXY, NULL);
	smix1(BB, 1, bctx.Sbytes / 128, (salsa20_blk_t *)bctx.S0, BXY, NULL);

	smix1_pair(AB, r, N, BB, AV, AXY, BV, BXY, &actx, &bctx);
	{
		uint32_t Nloop_rw = (N + 2) / 3;
		Nloop_rw++;
		Nloop_rw &= ~(uint32_t)1;
		smix2_pair(AB, r, N, Nloop_rw, BB, AV, AXY, BV, BXY,
		    &actx, &bctx);
	}

	HMAC_SHA256_Buf(AB + B_size - 64, 64, sha256a, sizeof(sha256a),
	    (uint8_t *)dsta);
	HMAC_SHA256_Buf(BB + B_size - 64, 64, sha256b, sizeof(sha256b),
	    (uint8_t *)dstb);

	return 0;

fail:
	memset(dsta, 0xff, sizeof(*dsta));
	memset(dstb, 0xff, sizeof(*dstb));
	return -1;
}

#endif /* _YESPOWER_PAIR_KERNEL_H_ */
