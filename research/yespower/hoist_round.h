/* Research-only diagnostic: explicit per-round load hoisting with NO hazard
 * check. All four pairs' S entries are loaded before any of the round's stores
 * are issued. This is exactly correct only when no later pair in the round
 * reads a slot written by an earlier pair (observed conflict rate is about
 * 0.26% of rounds), so digests can differ. It exists to measure the upper
 * bound of software-reordering PWXFORM on an out-of-order CPU; it must never
 * be used for mining.
 */
static inline void hoist_round(__m128i X[4], uint8_t *S0, uint8_t *S1,
    size_t *wp, int first)
{
	uint32_t lo[4], hi[4];
	size_t w = *wp;
	__m128i a[4], b[4];
	int g;

	for (g = 0; g < 4; g++) {
		uint64_t x = _mm_cvtsi128_si64(X[g]);
		lo[g] = (uint32_t)x & 0x7ff0;
		hi[g] = (uint32_t)(x >> 32) & 0x7ff0;
	}
	for (g = 0; g < 4; g++) {
		a[g] = *(__m128i *)(S0 + lo[g]);
		b[g] = *(__m128i *)(S1 + hi[g]);
	}
	for (g = 0; g < 4; g++)
		X[g] = _mm_xor_si128(_mm_add_epi64(
		    _mm_mul_epu32(_mm_srli_epi64(X[g], 32), X[g]), a[g]), b[g]);

	*(__m128i *)(S0 + w) = X[0];
	*(__m128i *)(S1 + w) = X[1];
	if (first) {
		*(__m128i *)(S0 + w + 16) = X[2];
		*(__m128i *)(S1 + w + 16) = X[3];
		w += 32;
	} else {
		w += 16;
	}
	*wp = w;
}
