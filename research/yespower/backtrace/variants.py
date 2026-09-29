"""Exact experimental rewrites of copied C source; never patch production files."""

TAIL = r'''
#if _YESPOWER_OPT_C_PASS_ > 1
/* Last SMix2 step: V and the first 15 output chunks have no future reader. */
static void backtrace_finish(salsa20_blk_t *restrict input,
    const salsa20_blk_t *restrict history, size_t r, pwxform_ctx_t *ctx)
{
    uint8_t *S0=ctx->S0, *S1=ctx->S1, *S2=ctx->S2;
    size_t w=ctx->w;
    DECL_X
    DECL_SMASK2REG
    XOR_X_2(input[2*r-1], history[2*r-1])
    for (size_t i=0; i<2*r; i++) {
        XOR_X(input[i])
        XOR_X(history[i])
        PWXFORM
    }
    SALSA20(input[2*r-1])
    /* No context export: this hash has no further S use. */
}
#endif
'''

LAZY = r'''
#if _YESPOWER_OPT_C_PASS_ > 1
/* Research only, fixed Tidecoin parameters. Retain X states as XOR deltas.
 * V[j] = original_V[j] XOR every earlier X_t that selected j.
 * Per-thread storage is static to keep allocator time outside the hot loop.
 */
static void backtrace_lazy_smix2(uint8_t *B, size_t r, uint32_t N,
    uint32_t loops, salsa20_blk_t *V, salsa20_blk_t *XY, pwxform_ctx_t *ctx)
{
    if (r != 8 || N != 2048 || loops != 684) abort();
    static __thread salsa20_blk_t states[685][16];
    int heads[2048], previous[684];
    salsa20_blk_t material[16], tmp;
    (void)XY;
    for (size_t j=0;j<2048;j++) heads[j]=-1;
    for (size_t i=0;i<16;i++) {
        const salsa20_blk_t *src=(const salsa20_blk_t *)(B+64*i);
        for (size_t k=0;k<16;k++) tmp.w[k]=le32dec(&src->w[k]);
        salsa20_simd_shuffle(&tmp,&states[0][i]);
    }
    for (uint32_t t=0;t<loops;t++) {
        uint32_t j=integerify(states[t],r)&(N-1);
        const salsa20_blk_t *vj=V+j*16;
        if (heads[j]>=0) {
            memcpy(material,vj,sizeof(material));
            for (int p=heads[j];p>=0;p=previous[p])
                for (size_t b=0;b<16;b++)
                    for (size_t q=0;q<8;q++) material[b].d[q]^=states[p][b].d[q];
            vj=material;
        }
        blockmix_xor(states[t],vj,states[t+1],r,ctx);
        previous[t]=heads[j]; heads[j]=t;
    }
    // The final HMAC only consumes this chunk.
    salsa20_simd_unshuffle(&states[loops][15],&tmp);
    for (size_t k=0;k<16;k++) le32enc(B+960+4*k,tmp.w[k]);
}
#endif
'''


def once(s, old, new):
    assert s.count(old) == 1, (old[:80], s.count(old))
    return s.replace(old, new)


def transform(source, name):
    if name == "baseline":
        return source
    if name == "tail":
        marker = "#if _YESPOWER_OPT_C_PASS_ == 1\n/**\n * integerify(B, r):"
        source = once(source, marker, TAIL + "\n" + marker)
        start = source.index("\t\tdo {", source.index("static void smix2("))
        end = source.index("\n#if _YESPOWER_OPT_C_PASS_ == 1", start)
        old = source[start:end]
        new = r'''
#if _YESPOWER_OPT_C_PASS_ > 1
        if (Nloop > 2) {
            do {
                salsa20_blk_t *V_j=&V[j*s];
                j=blockmix_xor_save(X,V_j,r,ctx)&(N-1);
                V_j=&V[j*s];
                j=blockmix_xor_save(X,V_j,r,ctx)&(N-1);
            } while ((Nloop-=2)>2);
        }
        salsa20_blk_t *last=&V[j*s];
        j=blockmix_xor_save(X,last,r,ctx)&(N-1);
        backtrace_finish(X,&V[j*s],r,ctx);
#else
''' + old + "\n#endif"
        source = source[:start] + new + source[end:]
        pos = source.index("static void smix2(")
        # Only the OUTPUT loop of smix2; preserve all input chunks.
        old = "\tfor (i = 0; i < 2 * r; i++) {\n\t\tconst salsa20_blk_t *src = &X[i];"
        new = "\tfor (i = (_YESPOWER_OPT_C_PASS_ > 1 ? 2*r-1 : 0); i < 2 * r; i++) {\n\t\tconst salsa20_blk_t *src = &X[i];"
        source = source[:pos] + once(source[pos:], old, new)
        return source
    if name == "lazy_v":
        marker = "static void smix2(uint8_t *B, size_t r, uint32_t N, uint32_t Nloop,"
        source = once(source, marker, LAZY + "\n" + marker)
        old = "\tsmix2(B, r, N, Nloop_rw /* must be > 2 */, V, XY, ctx);"
        return once(source, old, "#if _YESPOWER_OPT_C_PASS_ > 1\n"
                    "\tbacktrace_lazy_smix2(B,r,N,Nloop_rw,V,XY,ctx);\n#else\n" + old + "\n#endif")
    raise ValueError(name)
