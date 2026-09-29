/* Research driver for the two-hash interleaved pair kernel.
 * Modes:
 *   stdin       read 80-byte headers, print 32-byte single-kernel digests
 *   pairstdin   read pairs of 80-byte headers, print 64 bytes of pair digests
 *   bench N     verify pair == single over N nonces, then time 2*N hashes each
 */
#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include "yespower.h"

int yespower_pair(yespower_local_t *, yespower_local_t *,
    const uint8_t *, const uint8_t *, size_t, const yespower_params_t *,
    yespower_binary_t *, yespower_binary_t *);

static uint64_t ns(void)
{
	struct timespec t;
	clock_gettime(CLOCK_MONOTONIC_RAW, &t);
	return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}

static yespower_params_t params = {YESPOWER_1_0, 2048, 8, NULL, 0};

int main(int argc, char **argv)
{
	yespower_local_t local, local2;
	yespower_binary_t out, out2;
	unsigned char header[80] = {0};

	if (yespower_init_local(&local) || yespower_init_local(&local2))
		return 2;

	if (argc == 1) {
		size_t n;
		while ((n = fread(header, 1, 80, stdin))) {
			if (n != 80 ||
			    yespower(&local, header, 80, &params, &out))
				return 3;
			if (fwrite(out.uc, 1, 32, stdout) != 32)
				return 4;
		}
		if (ferror(stdin))
			return 5;
		return yespower_free_local(&local) || yespower_free_local(&local2);
	}

	if (!strcmp(argv[1], "pairstdin")) {
		unsigned char header2[80];
		size_t n;
		while ((n = fread(header, 1, 80, stdin))) {
			if (n != 80)
				return 3;
			if (fread(header2, 1, 80, stdin) != 80)
				return 3;
			if (yespower_pair(&local, &local2, header, header2, 80,
			    &params, &out, &out2))
				return 3;
			if (fwrite(out.uc, 1, 32, stdout) != 32 ||
			    fwrite(out2.uc, 1, 32, stdout) != 32)
				return 4;
		}
		if (ferror(stdin))
			return 5;
		return yespower_free_local(&local) || yespower_free_local(&local2);
	}

	if (!strcmp(argv[1], "bench") && argc >= 3) {
		unsigned count = strtoul(argv[2], NULL, 10);
		unsigned i;
		unsigned char (*hdrs)[80] = calloc(count, 80);
		unsigned char (*d1)[32] = calloc(count, 32);
		unsigned char (*d2)[32] = calloc(count, 32);
		uint64_t t0, t1, tsingle = 0, tpair = 0;
		if (!hdrs || !d1 || !d2 || !count)
			return 6;
		for (i = 0; i < count; i++)
			for (int j = 0; j < 4; j++)
				hdrs[i][76 + j] = (unsigned char)(i >> (8 * j));
		for (i = 0; i + 1 < count; i += 2) {
			if (yespower(&local, hdrs[i], 80, &params,
			    (yespower_binary_t *)d1[i]) ||
			    yespower(&local, hdrs[i + 1], 80, &params,
			    (yespower_binary_t *)d1[i + 1]) ||
			    yespower_pair(&local, &local2, hdrs[i], hdrs[i + 1],
			    80, &params, (yespower_binary_t *)d2[i],
			    (yespower_binary_t *)d2[i + 1]))
				return 7;
			if (memcmp(d1[i], d2[i], 32) || memcmp(d1[i + 1], d2[i + 1], 32)) {
				printf("{\"mismatch_at\":%u}\n", i);
				return 8;
			}
		}
		for (int trial = 0; trial < 4; trial++) {
			if (trial & 1) {
				t0 = ns();
				for (i = 0; i < count; i++)
					yespower(&local, hdrs[i], 80, &params,
					    (yespower_binary_t *)d1[i]);
				tsingle += ns() - t0;
				t0 = ns();
				for (i = 0; i + 1 < count; i += 2)
					yespower_pair(&local, &local2, hdrs[i], hdrs[i + 1],
					    80, &params, (yespower_binary_t *)d2[i],
					    (yespower_binary_t *)d2[i + 1]);
				tpair += ns() - t0;
			} else {
				t0 = ns();
				for (i = 0; i + 1 < count; i += 2)
					yespower_pair(&local, &local2, hdrs[i], hdrs[i + 1],
					    80, &params, (yespower_binary_t *)d2[i],
					    (yespower_binary_t *)d2[i + 1]);
				tpair += ns() - t0;
				t0 = ns();
				for (i = 0; i < count; i++)
					yespower(&local, hdrs[i], 80, &params,
					    (yespower_binary_t *)d1[i]);
				tsingle += ns() - t0;
			}
		}
		printf("{\"hashes\":%u,\"pair_ns\":%llu,\"single_ns\":%llu,"
		    "\"pair_hps\":%.6f,\"single_hps\":%.6f,\"speedup\":%.4f}\n",
		    count, (unsigned long long)tpair, (unsigned long long)tsingle,
		    count * 4e9 / tpair, count * 4e9 / tsingle,
		    (double)tsingle / (double)tpair);
		return 0;
	}

	return 10;
}
