"""Build isolated C variants, validate every digest, then run pinned randomized trials."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import random
import re
import shutil
import statistics
import subprocess
import tempfile

HERE = Path(__file__).resolve().parent
HELPERS = r'''
#include <time.h>
extern __thread uint64_t research_ns[6], research_count[4];
static uint64_t research_clock(void) {
    struct timespec t; clock_gettime(CLOCK_MONOTONIC_RAW, &t);
    return (uint64_t)t.tv_sec * 1000000000 + t.tv_nsec;
}
#define TIME_STAGE(i, stmt) do { uint64_t t = research_clock(); stmt; research_ns[i] += research_clock() - t; } while (0)
static void research_sha80(const uint8_t *src, size_t len, uint8_t *out) {
    static __thread SHA256_CTX cached;
    static __thread unsigned char prefix[64];
    static __thread int ready;
    if (len != 80) { SHA256_Buf(src, len, out); return; }
    if (!ready || memcmp(prefix, src, 64)) {
        SHA256_Init(&cached); SHA256_Update(&cached, src, 64);
        memcpy(prefix, src, 64); ready = 1;
    }
    SHA256_CTX ctx = cached;
    SHA256_Update(&ctx, src + 64, 16); SHA256_Final(out, &ctx);
}
'''


def replace_once(source, old, new):
    assert source.count(old) == 1, (old, source.count(old))
    return source.replace(old, new)


def variant(source, name):
    source = replace_once(source, '#include "yespower.h"', '#include "yespower.h"\n' + HELPERS)
    if "fixed" in name:
        for old,new in [('version = params->version','version = YESPOWER_1_0'),
                        ('N = params->N','N = 2048'), ('r = params->r','r = 8'),
                        ('pers = params->pers','pers = NULL'), ('perslen = params->perslen','perslen = 0')]:
            source = replace_once(source, old, new)
    if "noprefetch" in name:
        source = source.replace('#ifdef PREFETCH', '#if 0 /* research: prefetch disabled */')
    if "noforce" in name:
        source = re.sub(r'#define FORCE_REGALLOC_1 \\\n\s*__asm__\([^\n]+\);',
                        '#define FORCE_REGALLOC_1 /* research */', source)
        source = re.sub(r'#define FORCE_REGALLOC_2 \\\n\s*__asm__\([^\n]+\);',
                        '#define FORCE_REGALLOC_2 /* research */', source)
        assert '"=a" (x)' not in source
    if "midstate" in name:
        source = replace_once(source, '\tSHA256_Buf(src, srclen, sha256);',
                              '\tresearch_sha80(src, srclen, sha256);')
    if "parallel" in name:
        source = replace_once(source, '#define PWXFORM_SIMD_WRITE(X, Sw)',
            '#include "parallel_round.h"\n\n#define PWXFORM_SIMD_WRITE(X, Sw)')
        old = '\tPWXFORM_ROUND_WRITE4 PWXFORM_ROUND_WRITE2 PWXFORM_ROUND_WRITE2 \\\n'
        new = '\t{ __m128i xx[4] = {X0,X1,X2,X3}; \\\n' \
              '\t  research_round(xx,S0,S1,&w,1); research_round(xx,S0,S1,&w,0); research_round(xx,S0,S1,&w,0); \\\n' \
              '\t  X0=xx[0]; X1=xx[1]; X2=xx[2]; X3=xx[3]; } \\\n'
        source = replace_once(source, old, new)
    if "stages" in name:
        statements = [
            'SHA256_Buf(src, srclen, sha256);',
            'PBKDF2_SHA256(sha256, sizeof(sha256), src, srclen, 1, B, 128);',
            'smix1(B, 1, ctx->Sbytes / 128, (salsa20_blk_t *)ctx->S0, XY, NULL);',
            'smix1(B, r, N, V, XY, ctx);',
            'smix2(B, r, N, Nloop_rw /* must be > 2 */, V, XY, ctx);',
            'HMAC_SHA256_Buf(B + B_size - 64, 64,\n\t\t    sha256, sizeof(sha256), (uint8_t *)dst);',
        ]
        for i, statement in enumerate(statements):
            source = replace_once(source, statement, f'TIME_STAGE({i}, {statement[:-1]});')
    return source


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument('--cpu', type=int, default=2)
    p.add_argument('--hashes', type=int, default=2048)
    p.add_argument('--trials', type=int, default=5)
    p.add_argument('--variants', help='Comma-separated subset of variants')
    p.add_argument('--output', type=Path, default=HERE / 'results/cpu.json')
    p.add_argument("--source", type=Path, required=True, help="optimized yespower C source directory")
    p.add_argument("--reference", type=Path, required=True, help="scalar yespower C reference directory")
    args = p.parse_args()
    assert args.cpu in os.sched_getaffinity(0)
    specs = {
        'gcc_sse2': ('gcc', []),
        'gcc_native': ('gcc', ['-march=native']),
        'gcc_native_noforce': ('gcc', ['-march=native']),
        'gcc_sse2_noprefetch': ('gcc', []),
        'gcc_native_noprefetch': ('gcc', ['-march=native']),
        'gcc_native_midstate': ('gcc', ['-march=native']),
        'clang_native': ('clang', ['-march=native']),
        'gcc_native_parallel': ('gcc', ['-march=native']),
        'gcc_sse2_stages': ('gcc', []),
        'gcc_native_stages': ('gcc', ['-march=native']),
        'gcc_native_parallel_stats': ('gcc', ['-march=native','-DRESEARCH_STATS']),
        'gcc_native_fixed': ('gcc', ['-march=native']),
        'gcc_sse2_fixed': ('gcc', []),
        'gcc_native_nounroll': ('gcc', ['-march=native','-fno-unroll-loops']),
        'gcc_sse2_nounroll': ('gcc', ['-fno-unroll-loops']),
    }
    if args.variants:
        specs = {name: specs[name] for name in args.variants.split(',')}
    corpus = json.loads((HERE.parents[1] / 'tests/fixtures/yespower.json').read_text())
    inputs = [bytes.fromhex(v['header']) for v in corpus['vectors']]
    for i in range(256):
        inputs.append(b''.join(hashlib.sha256(f'research-{i}-{j}'.encode()).digest() for j in range(3))[:80])
    raw = b''.join(inputs)
    result = {'cpu': args.cpu, 'hashes_per_trial': args.hashes, 'trials': args.trials,
              'reference_headers': len(inputs), 'variants': {}, 'runs': [],
              'source_sha256': {f.name: hashlib.sha256(f.read_bytes()).hexdigest()
                                for f in args.source.iterdir() if f.is_file()},
              'cpuinfo': Path('/proc/cpuinfo').read_text().split('\n\n')[0]}
    with tempfile.TemporaryDirectory(prefix='tideminer-cpu-research-') as temporary:
        root = Path(temporary)
        ref = root / 'reference'
        cmd = ['gcc','-O2','-std=gnu99','-I',str(args.reference),str(HERE/'driver.c'),
               str(args.reference/'yespower-ref.c'),str(args.reference/'sha256.c'),'-o',str(ref)]
        subprocess.run(cmd, check=True, capture_output=True)
        expected = subprocess.run([str(ref)],input=raw,capture_output=True,check=True).stdout
        assert len(expected) == 32 * len(inputs)
        for i,v in enumerate(corpus['vectors']):
            assert expected[i*32:(i+1)*32].hex() == v['hash']
        binaries = {}
        for name, (cc, extra) in specs.items():
            folder = root / name
            shutil.copytree(args.source, folder)
            shutil.copy(HERE/'parallel_round.h', folder)
            code = variant((folder/'yespower-opt.c').read_text(), name)
            (folder/'yespower-opt.c').write_text(code)
            binary = folder / 'bench'
            flags = ['-O3','-std=gnu99','-funroll-loops','-fomit-frame-pointer',*extra]
            cmd = [cc,*flags,'-I',str(folder),str(HERE/'driver.c'),
                   str(folder/'yespower-opt.c'),str(folder/'sha256.c'),'-o',str(binary)]
            build = subprocess.run(cmd,capture_output=True,text=True)
            if build.returncode:
                raise RuntimeError(name + '\n' + build.stderr)
            actual = subprocess.run([str(binary)],input=raw,capture_output=True,check=True).stdout
            assert actual == expected, f'{name}: differential mismatch'
            result['variants'][name] = {'compiler': subprocess.check_output([cc,'--version'],text=True).splitlines()[0],
                'flags':flags,'parity':True,'transformed_source_sha256':hashlib.sha256(code.encode()).hexdigest()}
            binaries[name] = binary
            print(f'Built and validated {name}: {len(inputs)} headers',flush=True)
        rng = random.Random(20260913)
        for trial in range(args.trials):
            names = list(specs)
            rng.shuffle(names)
            for name in names:
                output = subprocess.check_output(['taskset','-c',str(args.cpu),str(binaries[name]),str(args.hashes)],text=True)
                row = json.loads(output)
                row.update(variant=name,trial=trial)
                result['runs'].append(row)
                print(f'{trial+1}/{args.trials} {name}: {row["hps"]:.1f} H/s',flush=True)
        checksums = {r['xor'] for r in result['runs']}
        assert len(checksums) == 1
        result['summary'] = {}
        for name in specs:
            rows = [r for r in result['runs'] if r['variant'] == name]
            rates = [r['hps'] for r in rows]
            result['summary'][name] = {'median_hps':statistics.median(rates),'min_hps':min(rates),'max_hps':max(rates),
                'median_stages_ns_per_hash':[statistics.median(r['stages_ns'][i]/r['hashes'] for r in rows) for i in range(6)]}
        args.output.parent.mkdir(parents=True,exist_ok=True)
        args.output.write_text(json.dumps(result,indent=2)+'\n')
        print(json.dumps(result['summary'],indent=2))


if __name__ == '__main__':
    main()
