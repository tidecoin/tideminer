/* Diffusion probe of one PWX round with random tables.
 *
 * For random states a and random S entries, flip each of the 64 input bits of
 * a, recompute one round, and measure how many output bits and how many
 * *next-round address* bits change (bits 4..14 and 36..46 of the new word).
 * Used only to document that the address derivation has no local few-bit
 * structure a composite shortcut could exploit; it is not a security proof.
 *
 * Build: gcc -O2 -std=gnu99 diffusion.c -o diffusion
 */
#include <stdint.h>
#include <stdio.h>
#include <string.h>

static uint64_t rng_state = 0x9e3779b97f4a7c15ull;
static uint64_t rnd(void)
{
	uint64_t x = rng_state;
	x ^= x << 13;
	x ^= x >> 7;
	x ^= x << 17;
	return rng_state = x;
}

static uint8_t S0[32768], S1[32768];

static inline uint64_t round1(uint64_t a)
{
	uint32_t lo = (uint32_t)a & 0x7ff0;
	uint32_t hi = (uint32_t)(a >> 32) & 0x7ff0;
	uint64_t s0, s1;
	memcpy(&s0, S0 + lo, 8);
	memcpy(&s1, S1 + hi, 8);
	uint64_t p = (uint64_t)(uint32_t)a * (uint32_t)(a >> 32);
	return (p + s0) ^ s1;
}

static inline uint64_t next_addr(uint64_t y)
{
	return ((y >> 32) & 0x7ff0) | (y & 0x7ff0);
}

int main(void)
{
	const int samples = 20000;
	uint64_t sum_out = 0, sum_next_addr = 0, flips = 0;
	uint64_t next_addr_changed = 0, out_changed = 0;
	for (unsigned i = 0; i < sizeof(S0); i++) {
		S0[i] = (uint8_t)rnd();
		S1[i] = (uint8_t)rnd();
	}
	for (int n = 0; n < samples; n++) {
		uint64_t a = rnd();
		uint64_t base = round1(a);
		uint64_t base_next = next_addr(base);
		for (int bit = 0; bit < 64; bit++) {
			uint64_t y = round1(a ^ (1ull << bit));
			uint64_t next = next_addr(y);
			sum_out += __builtin_popcountll(base ^ y);
			sum_next_addr += __builtin_popcountll(base_next ^ next);
			flips++;
			if (y != base)
				out_changed++;
			if (next != base_next)
				next_addr_changed++;
		}
	}
	printf("{\"samples\":%d,\"input_bits\":64,"
	       "\"mean_output_bits_changed_per_flip\":%.3f,"
	       "\"fraction_flips_changing_output\":%.5f,"
	       "\"mean_next_address_bits_changed_per_flip\":%.3f,"
	       "\"next_address_bits\":22,"
	       "\"fraction_flips_changing_next_address\":%.5f}\n",
	       samples, (double)sum_out / flips, (double)out_changed / flips,
	       (double)sum_next_addr / flips,
	       (double)next_addr_changed / flips);
	return 0;
}
