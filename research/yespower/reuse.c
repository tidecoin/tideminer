/* Header/job reuse measurement for Tidecoin yespower.
 *
 * The pool keeps most of the 80-byte header fixed across a nonce sweep. This
 * program quantifies the only reusable SHA-256 quantity (the midstate over the
 * first 64 bytes) and shows that the 32-byte digest, the 128-byte PBKDF2 seed
 * and therefore S/V change completely when only the nonce changes.
 *
 * Build:
 *   gcc -O2 -std=gnu99 -I<vendored yespower dir> reuse.c sha256.c -o reuse
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include "sha256.h"

static int popcount(const uint8_t *a, const uint8_t *b, size_t n)
{
	int bits = 0;
	for (size_t i = 0; i < n; i++) {
		uint8_t x = a[i] ^ b[i];
		while (x) {
			bits += x & 1;
			x >>= 1;
		}
	}
	return bits;
}

int main(void)
{
	uint8_t base[80] = {0};
	uint8_t sha[2][32], seed[2][128];
	SHA256_CTX mid[2];
	int swa_sha = 0, swa_seed = 0, pairs = 0;
	int mid_identical = 0;

	/* Fixture-like prefix: only bytes 68..79 vary. */
	for (unsigned i = 0; i < 80; i++)
		base[i] = (uint8_t)(i * 7 + 1);

	for (unsigned n = 0; n < 4096; n++) {
		for (unsigned k = 0; k < 2; k++) {
			uint8_t hdr[80];
			memcpy(hdr, base, 80);
			uint32_t nonce = 2 * n + k;
			hdr[76] = nonce;
			hdr[77] = nonce >> 8;
			hdr[78] = nonce >> 16;
			hdr[79] = nonce >> 24;
			SHA256_Init(&mid[k]);
			SHA256_Update(&mid[k], hdr, 64);
			SHA256_Buf(hdr, 80, sha[k]);
			PBKDF2_SHA256(sha[k], 32, NULL, 0, 1, seed[k], 128);
		}
		if (memcmp(mid[0].state, mid[1].state, sizeof(mid[0].state)) == 0)
			mid_identical++;
		swa_sha += popcount(sha[0], sha[1], 32);
		swa_seed += popcount(seed[0], seed[1], 128);
		pairs++;
	}

	printf("{\"pairs\":%d,"
	       "\"midstate_identical_after_first_64_bytes\":%d,"
	       "\"mean_hamming_distance_sha256_bits\":%.3f,"
	       "\"sha256_bits\":256,"
	       "\"mean_hamming_distance_pbkdf2_seed_bits\":%.3f,"
	       "\"seed_bits\":1024}\n",
	       pairs, mid_identical,
	       (double)swa_sha / pairs, (double)swa_seed / pairs);
	return 0;
}
