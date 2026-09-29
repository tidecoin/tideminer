/* Experimental CPU translation of the CUDA conflict-detection idea.
 * No changes to production code. Every variant must pass the scalar oracle.
 */
static inline void research_round(__m128i X[4], uint8_t *S0, uint8_t *S1,
    size_t *wp, int first)
{
    uint32_t lo[4], hi[4];
    size_t w = *wp;
    for (int g = 0; g < 4; g++) {
        uint64_t x = _mm_cvtsi128_si64(X[g]);
        lo[g] = (uint32_t)x & 0x7ff0;
        hi[g] = (uint32_t)(x >> 32) & 0x7ff0;
    }
    int conflict = lo[1] == w || lo[2] == w || hi[2] == w ||
        lo[3] == w || hi[3] == w || (first && lo[3] == w + 16);
#ifdef RESEARCH_STATS
    research_count[0]++;
    research_count[1] += conflict;
#endif
    if (conflict) {
        for (int g = 0; g < 4; g++) {
            __m128i a = *(__m128i *)(S0 + lo[g]);
            __m128i b = *(__m128i *)(S1 + hi[g]);
            X[g] = _mm_xor_si128(_mm_add_epi64(
                _mm_mul_epu32(_mm_srli_epi64(X[g],32), X[g]), a), b);
            if (first || g < 2) {
                *(__m128i *)((g & 1 ? S1 : S0) + w) = X[g];
                if (g & 1) w += 16;
            }
        }
    } else {
        __m128i a[4], b[4];
        for (int g = 0; g < 4; g++) {
            a[g] = *(__m128i *)(S0 + lo[g]);
            b[g] = *(__m128i *)(S1 + hi[g]);
        }
        for (int g = 0; g < 4; g++)
            X[g] = _mm_xor_si128(_mm_add_epi64(
                _mm_mul_epu32(_mm_srli_epi64(X[g],32), X[g]), a[g]), b[g]);
        *(__m128i *)(S0 + w) = X[0];
        *(__m128i *)(S1 + w) = X[1];
        if (first) {
            *(__m128i *)(S0 + w + 16) = X[2];
            *(__m128i *)(S1 + w + 16) = X[3];
        }
        w += first ? 32 : 16;
    }
    *wp = w;
}
