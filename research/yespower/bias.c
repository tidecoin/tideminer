/* Per-nonce and per-time uniformity probe for Tidecoin yespower.
 *
 * Hashes a fixed header prefix while varying only the nonce field (mode=nonce)
 * or only the time field (mode=time), and reports a histogram of the most
 * significant digest byte (byte 31, the byte the target comparison reads
 * first), a histogram by 4 bits of the varying field, and the adjacent-value
 * equality rate of that byte.
 *
 * Build:
 *   gcc -O3 -std=gnu99 -funroll-loops -march=native \
 *     -I<vendored yespower dir> bias.c yespower-opt.c sha256.c -o bias
 * Run: bias <nonce|time> <start> <count>   (prints one JSON line)
 */
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include "yespower.h"

static int top[256];
static int cond[16][256];
static long long adjacent_equal;

int main(int argc, char **argv)
{
	yespower_local_t local;
	yespower_params_t params = {YESPOWER_1_0, 2048, 8, NULL, 0};
	yespower_binary_t out;
	unsigned char header[80] = {0};
	if (argc != 4)
		return 2;
	int use_time = !strcmp(argv[1], "time");
	uint32_t start = (uint32_t)strtoul(argv[2], NULL, 0);
	uint32_t count = (uint32_t)strtoul(argv[3], NULL, 0);
	if (!count || yespower_init_local(&local))
		return 3;
	for (unsigned i = 0; i < 80; i++)
		header[i] = (unsigned char)(i * 11 + 3);
	int previous = -1;
	for (uint32_t k = 0; k < count; k++) {
		uint32_t v = start + k;
		if (use_time) {
			header[68] = v;
			header[69] = v >> 8;
			header[70] = v >> 16;
			header[71] = v >> 24;
		} else {
			header[76] = v;
			header[77] = v >> 8;
			header[78] = v >> 16;
			header[79] = v >> 24;
		}
		if (yespower(&local, header, 80, &params, &out))
			return 4;
		unsigned b = out.uc[31];
		top[b]++;
		cond[v & 15][b]++;
		if ((int)b == previous)
			adjacent_equal++;
		previous = b;
	}
	printf("{\"mode\":\"%s\",\"start\":%u,\"count\":%u,\"top\":[",
	       argv[1], start, count);
	for (int i = 0; i < 256; i++)
		printf("%s%d", i ? "," : "", top[i]);
	printf("],\"cond\":[");
	for (int j = 0; j < 16; j++) {
		printf("%s[", j ? "," : "");
		for (int i = 0; i < 256; i++)
			printf("%s%d", i ? "," : "", cond[j][i]);
		printf("]");
	}
	printf("],\"adjacent_equal\":%lld}\n", adjacent_equal);
	return yespower_free_local(&local);
}
