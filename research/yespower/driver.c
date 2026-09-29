#define _GNU_SOURCE
#include <stdio.h>
#include <string.h>
#include <time.h>
#include "yespower.h"

__thread uint64_t research_ns[6], research_count[4];
static uint64_t ns(void) {
    struct timespec t;
    clock_gettime(CLOCK_MONOTONIC_RAW, &t);
    return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
int main(int argc, char **argv) {
    yespower_local_t local;
    yespower_params_t params = {YESPOWER_1_0, 2048, 8, NULL, 0};
    yespower_binary_t out;
    unsigned char header[80] = {0}, checksum[32] = {0};
    if (yespower_init_local(&local)) return 2;
    if (argc == 1) {
        size_t n;
        while ((n = fread(header, 1, 80, stdin))) {
            if (n != 80 || yespower(&local, header, 80, &params, &out)) return 3;
            if (fwrite(out.uc, 1, 32, stdout) != 32) return 4;
        }
        if (ferror(stdin)) return 5;
    } else {
        unsigned count = strtoul(argv[1], NULL, 10);
        if (!count) return 6;
        for (unsigned i = 0; i < 64; i++) {
            header[76] = i;
            if (yespower(&local, header, 80, &params, &out)) return 7;
        }
        memset(research_ns, 0, sizeof(research_ns));
        memset(research_count, 0, sizeof(research_count));
        uint64_t start = ns();
        for (unsigned i = 0; i < count; i++) {
            for (int j = 0; j < 4; j++) header[76+j] = i >> (8*j);
            if (yespower(&local, header, 80, &params, &out)) return 8;
            for (int j = 0; j < 32; j++) checksum[j] ^= out.uc[j];
        }
        uint64_t elapsed = ns() - start;
        printf("{\"hashes\":%u,\"ns\":%llu,\"hps\":%.6f,\"scratch_bytes\":%zu,\"stages_ns\":[",
            count, (unsigned long long)elapsed, count * 1e9 / elapsed, local.aligned_size);
        for (int i = 0; i < 6; i++) printf("%s%llu", i ? "," : "", (unsigned long long)research_ns[i]);
        printf("],\"counts\":[");
        for (int i = 0; i < 4; i++) printf("%s%llu", i ? "," : "", (unsigned long long)research_count[i]);
        printf("],\"xor\":\"");
        for (int j = 0; j < 32; j++) printf("%02x", checksum[j]);
        printf("\"}\n");
    }
    return yespower_free_local(&local) ? 9 : 0;
}
