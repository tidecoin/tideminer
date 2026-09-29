/*
 * SMix/Salsa/PWX structure adapted from yespower-ref.c:
 * Copyright 2009 Colin Percival
 * Copyright 2013-2019 Alexander Peslyak
 * All rights reserved.
 *
 * Redistribution and use in source and binary forms, with or without
 * modification, are permitted provided that the following conditions are met:
 * 1. Redistributions of source code must retain the above copyright notice,
 *    this list of conditions and the following disclaimer.
 * 2. Redistributions in binary form must reproduce the above copyright notice,
 *    this list of conditions and the following disclaimer in the documentation
 *    and/or other materials provided with the distribution.
 *
 * THIS SOFTWARE IS PROVIDED BY THE AUTHOR AND CONTRIBUTORS ``AS IS'' AND ANY
 * EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED
 * WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE
 * DISCLAIMED. IN NO EVENT SHALL THE AUTHOR OR CONTRIBUTORS BE LIABLE FOR ANY
 * DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES
 * (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES;
 * LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON
 * ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT
 * (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
 * SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.
 */
// Exact Tidecoin SMix evaluator with a conservative, bit-mask backward slice.
// SHA/PBKDF2/HMAC are delegated to the original C implementation.
#include <array>
#include <cassert>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <iostream>
#include <limits>
#include <string>
#include <vector>
extern "C" {
#include "sha256.h"
}

enum Op { INPUT, XOR, ADD, MUL, LOW, HIGH, PACK, ROT, LOAD_S, LOAD_V,
          STORE_S, STORE_V, STORE_B, OP_COUNT };
const char *names[] = {"input", "xor", "add", "pwx_multiply", "low32",
    "high32", "pack64", "rotate32", "load_s", "load_v", "store_s",
    "store_v", "store_b"};
const char *phases[] = {"s_init", "expand", "smix1", "smix2"};
struct Node {
    uint32_t p, q, aux;
    uint8_t op, phase, width, pad;
};
struct Word { uint64_t v; uint32_t id; };
std::vector<Node> nodes(1);
std::vector<uint64_t> inputs;
unsigned phase = 0;
bool split_words = false;
uint64_t mask(unsigned n) { return n == 64 ? UINT64_MAX : (uint64_t(1) << n) - 1; }
Word make(Op op, uint64_t v, uint32_t p=0, uint32_t q=0,
          unsigned aux=0, unsigned width=64) {
    assert(nodes.size() < UINT32_MAX);
    if(op==INPUT) {aux=inputs.size();inputs.push_back(v);}
    nodes.push_back({p,q,aux,uint8_t(op),uint8_t(phase),uint8_t(width),0});
    return {v & mask(width), uint32_t(nodes.size()-1)};
}
Word xorr(Word a, Word b, unsigned w=64) { return make(XOR,a.v^b.v,a.id,b.id,0,w); }
Word add(Word a, Word b, unsigned w=64) { return make(ADD,a.v+b.v,a.id,b.id,0,w); }
Word mul(Word a) { return make(MUL,uint64_t(uint32_t(a.v))*(a.v>>32),a.id); }
Word low(Word a) { return make(LOW,uint32_t(a.v),a.id,0,0,32); }
Word high(Word a) { return make(HIGH,a.v>>32,a.id,0,0,32); }
Word pack(Word a, Word b) { return make(PACK,a.v|(b.v<<32),a.id,b.id); }
Word rot(Word a, unsigned n) {
    auto x = uint32_t(a.v);
    return make(ROT,(x<<n)|(x>>(32-n)),a.id,0,n,32);
}
Word load(Word value, Word address, Op op, unsigned address_kind) {
    // S kind 0/1: bits 4..14 / 36..46. V kind: number of low index bits.
    uint64_t selected;
    if(op==LOAD_V)selected=address.v&mask(address_kind);
    else if(address_kind==0)selected=(address.v>>4)&2047;
    else if(address_kind==1)selected=(address.v>>36)&2047;
    else selected=address.v&mask(address_kind-2);
    return make(op,value.v,value.id,address.id,address_kind|(selected<<8));
}
Word save(Word a, Op op, Word address={0,0}, unsigned bits=0) {
    return make(op,a.v,a.id,address.id,bits|((address.v&mask(bits))<<8));
}

void salsa(Word *b) {
    std::array<Word,16> original, x;
    for (unsigned i=0;i<8;i++) {
        original[2*i]=low(b[i]); original[2*i+1]=high(b[i]);
    }
    for (unsigned i=0;i<16;i++) x[i*5%16]=original[i];
    auto qr=[&](unsigned a,unsigned b,unsigned c,unsigned d) {
        x[b]=xorr(x[b],rot(add(x[a],x[d],32),7),32);
        x[c]=xorr(x[c],rot(add(x[b],x[a],32),9),32);
        x[d]=xorr(x[d],rot(add(x[c],x[b],32),13),32);
        x[a]=xorr(x[a],rot(add(x[d],x[c],32),18),32);
    };
    qr(0,4,8,12); qr(5,9,13,1); qr(10,14,2,6); qr(15,3,7,11);
    qr(0,1,2,3); qr(5,6,7,4); qr(10,11,8,9); qr(15,12,13,14);
    for (unsigned i=0;i<8;i++)
        b[i]=pack(add(original[2*i],x[(2*i)*5%16],32),
                  add(original[2*i+1],x[(2*i+1)*5%16],32));
}
void salsa_block(Word *b) {
    std::array<Word,8> x;
    for(unsigned k=0;k<8;k++) x[k]=b[8+k];
    for(unsigned i=0;i<2;i++) {
        for(unsigned k=0;k<8;k++) x[k]=xorr(x[k],b[i*8+k]);
        salsa(x.data());
        for(unsigned k=0;k<8;k++) b[i*8+k]=x[k];
    }
}
struct Context {
    std::vector<Word> s=std::vector<Word>(12288);
    unsigned s0=0,s1=4096,s2=8192,w=0;
};
void pwx(Word *x, Context &c) {
    if(split_words) {
        // A exact first-word execution produces the address trace for B.
        std::array<Word,12> addresses;
        unsigned initial_w=c.w;
        for(unsigned k=0;k<2;k++) {
            c.w=initial_w;
            for(unsigned round=0;round<3;round++) for(unsigned j=0;j<4;j++) {
                if(k==0)addresses[4*round+j]=x[2*j];
                Word address=addresses[4*round+j];
                unsigned lo=(address.v&0x7ff0)/8,hi=((address.v>>32)&0x7ff0)/8;
                Word a=load(c.s[c.s0+lo+k],address,LOAD_S,0);
                Word b=load(c.s[c.s1+hi+k],address,LOAD_S,1);
                x[2*j+k]=xorr(add(mul(x[2*j+k]),a),b);
                if(round==0 || j<2) {
                    unsigned base=(j&1)?c.s1:c.s0;
                    c.s[base+2*c.w+k]=save(x[2*j+k],STORE_S);
                    if(j&1)c.w++;
                }
            }
        }
        c.w &= 2047;
        unsigned old=c.s2;c.s2=c.s1;c.s1=c.s0;c.s0=old;
        return;
    }
    for(unsigned round=0;round<3;round++) for(unsigned j=0;j<4;j++) {
        Word address=x[2*j];
        unsigned lo=(address.v & 0x7ff0)/8;
        unsigned hi=((address.v>>32)&0x7ff0)/8;
        for(unsigned k=0;k<2;k++) {
            Word a=load(c.s[c.s0+lo+k],address,LOAD_S,0);
            Word b=load(c.s[c.s1+hi+k],address,LOAD_S,1);
            x[2*j+k]=xorr(add(mul(x[2*j+k]),a),b);
        }
        if(round==0 || j<2) {
            unsigned base=(j&1)?c.s1:c.s0;
            for(unsigned k=0;k<2;k++) c.s[base+2*c.w+k]=save(x[2*j+k],STORE_S);
            if(j&1) c.w++;
        }
    }
    c.w &= 2047;
    unsigned old=c.s2; c.s2=c.s1; c.s1=c.s0; c.s0=old;
}
void blockmix(Word *b, unsigned r, Context &c) {
    std::array<Word,8> x;
    for(unsigned k=0;k<8;k++) x[k]=b[16*r-8+k];
    for(unsigned i=0;i<2*r;i++) {
        for(unsigned k=0;k<8;k++) x[k]=xorr(x[k],b[8*i+k]);
        pwx(x.data(),c);
        for(unsigned k=0;k<8;k++) b[8*i+k]=save(x[k],STORE_B);
    }
    salsa(b+16*r-8);
}
unsigned floor_log(unsigned x) { assert(x); return 31-__builtin_clz(x); }
void mix1(Word *b,unsigned r,unsigned n,std::vector<Word>&v,Context&c,bool init) {
    std::vector<Word> x(b,b+16*r);
    if(!init) {
        phase=1;
        for(unsigned k=1;k<r;k++) {
            for(unsigned q=0;q<16;q++) x[16*k+q]=x[16*(k-1)+q];
            blockmix(x.data()+16*k,1,c);
        }
        phase=2;
    }
    for(unsigned i=0;i<n;i++) {
        for(unsigned q=0;q<16*r;q++) v[i*16*r+q]=save(x[q],init?STORE_S:STORE_V);
        if(i>1) {
            Word addr=x[16*r-8]; unsigned bits=floor_log(i), p=1u<<bits;
            unsigned j=(uint32_t(addr.v)&(p-1))+(i-p);
            for(unsigned q=0;q<16*r;q++)
                x[q]=xorr(x[q],load(v[j*16*r+q],addr,init?LOAD_S:LOAD_V,
                    init ? 2+bits : bits));
        }
        if(init) salsa_block(x.data()); else blockmix(x.data(),r,c);
    }
    for(unsigned q=0;q<16*r;q++) b[q]=x[q];
}
void mix2(Word*b,std::vector<Word>&v,Context&c) {
    phase=3;
    for(unsigned i=0;i<684;i++) {
        Word addr=b[120]; unsigned j=addr.v&2047;
        for(unsigned q=0;q<128;q++) {
            b[q]=xorr(b[q],load(v[j*128+q],addr,LOAD_V,11));
            v[j*128+q]=save(b[q],STORE_V,addr,11);
        }
        blockmix(b,8,c);
    }
}
void slice(const Word*b) {
    std::vector<uint64_t> live(nodes.size());
    for(unsigned q=120;q<128;q++) live[b[q].id]=UINT64_MAX;
    uint64_t total[4][OP_COUNT]={},used[4][OP_COUNT]={},bits[4][OP_COUNT]={};
    for(size_t id=nodes.size()-1;id>0;id--) {
        auto n=nodes[id]; uint64_t m=live[id]&mask(n.width);
        total[n.phase][n.op]++;
        if(!m) continue;
        used[n.phase][n.op]++; bits[n.phase][n.op]+=__builtin_popcountll(m);
        auto put=[&](uint32_t p,uint64_t b){live[p]|=b;};
        switch(n.op) {
        case INPUT: break;
        case XOR: put(n.p,m); put(n.q,m); break;
        case ADD: {
            uint64_t needed=mask(64-__builtin_clzll(m));
            put(n.p,needed);put(n.q,needed);break;
        }
        case MUL: {
            auto needed=mask(std::min(32u,64u-unsigned(__builtin_clzll(m))));
            put(n.p,needed|(needed<<32));break;
        }
        case LOW: put(n.p,m);break;
        case HIGH: put(n.p,m<<32);break;
        case PACK: put(n.p,m&UINT32_MAX);put(n.q,m>>32);break;
        case ROT: put(n.p, uint32_t((uint32_t(m)>>n.aux)|(uint32_t(m)<<(32-n.aux))));break;
        case LOAD_S: {
            unsigned kind=n.aux&255;
            put(n.p,m);put(n.q,kind==0?0x7ff0ULL:kind==1?0x7ff000000000ULL:mask(kind-2));break;
        }
        case LOAD_V: put(n.p,m);put(n.q,mask(n.aux&255));break;
        case STORE_S: case STORE_V: case STORE_B:
            put(n.p,m);if(n.q)put(n.q,mask(n.aux&255));break;
        default: std::abort();
        }
    }
    // Re-evaluate only live nodes, zeroing all unneeded result bits. Check
    // every live lookup's address as well as the final full HMAC key.
    std::vector<uint64_t> replay(nodes.size());
    uint64_t checked_loads=0,checked_stores=0;
    for(size_t id=1;id<nodes.size();id++) {
        auto n=nodes[id];uint64_t m=live[id]&mask(n.width);
        if(!m)continue;
        uint64_t a=replay[n.p],b=replay[n.q],v=0;
        switch(n.op) {
        case INPUT:v=inputs[n.aux];break;
        case XOR:v=a^b;break;
        case ADD:v=a+b;break;
        case MUL:v=uint64_t(uint32_t(a))*(a>>32);break;
        case LOW:v=uint32_t(a);break;
        case HIGH:v=a>>32;break;
        case PACK:v=a|(b<<32);break;
        case ROT:v=uint32_t((uint32_t(a)<<n.aux)|(uint32_t(a)>>(32-n.aux)));break;
        case LOAD_S: case LOAD_V: {
            unsigned kind=n.aux&255;
            uint64_t selected;
            if(n.op==LOAD_V)selected=b&mask(kind);
            else if(kind==0)selected=(b>>4)&2047;
            else if(kind==1)selected=(b>>36)&2047;
            else selected=b&mask(kind-2);
            assert(selected==(n.aux>>8));checked_loads++;v=a;break;
        }
        case STORE_S:case STORE_V:case STORE_B:
            if(n.q) {assert((b&mask(n.aux&255))==(n.aux>>8));checked_stores++;}
            v=a;break;
        default:std::abort();
        }
        replay[id]=v&m;
    }
    for(unsigned q=120;q<128;q++)assert(replay[b[q].id]==b[q].v);
    std::cout<<"\"nodes\":"<<nodes.size()-1
        <<",\"pruned_replay_verified\":true,\"replayed_load_addresses\":"<<checked_loads
        <<",\"replayed_store_addresses\":"<<checked_stores
        <<",\"phases\":{";
    for(unsigned p=0;p<4;p++) {
        if(p)std::cout<<',';
        std::cout<<'"'<<phases[p]<<"\":{";
        for(unsigned o=0;o<OP_COUNT;o++) {
            if(o)std::cout<<',';
            std::cout<<'"'<<names[o]<<"\":{\"total\":"<<total[p][o]
                <<",\"live\":"<<used[p][o]<<",\"live_bits\":"<<bits[p][o]<<'}';
        }
        std::cout<<'}';
    }
    std::cout<<'}';
}
uint32_t read32(const uint8_t*p) {return p[0]|uint32_t(p[1])<<8|uint32_t(p[2])<<16|uint32_t(p[3])<<24;}
void write32(uint8_t*p,uint32_t x){for(unsigned i=0;i<4;i++)p[i]=x>>(8*i);}
int main(int argc,char**argv) {
    if((argc!=2 && argc!=3) || std::string(argv[1]).size()!=160)return 2;
    if(argc==3) {if(std::string(argv[2])!="split")return 2;split_words=true;}
    uint8_t header[80],seed[128],sha[32],key[64],out[32];
    for(unsigned i=0;i<80;i++) {
        std::string s(argv[1]+2*i,2);char *end=nullptr;
        unsigned long v=std::strtoul(s.c_str(),&end,16);if(*end)return 2;header[i]=v;
    }
    SHA256_Buf(header,80,sha);PBKDF2_SHA256(sha,32,nullptr,0,1,seed,128);
    std::copy(seed,seed+32,sha);
    nodes.reserve(9000000);
    std::array<Word,128>b{};
    // Reference SIMD word shuffle, i -> (i*5)%16.
    for(unsigned block=0;block<2;block++) for(unsigned q=0;q<8;q++) {
        uint64_t lo=read32(seed+64*block+4*((2*q)*5%16));
        uint64_t hi=read32(seed+64*block+4*((2*q+1)*5%16));
        b[8*block+q]=make(INPUT,lo|(hi<<32));
    }
    Context c; std::vector<Word>v(2048*128);
    mix1(b.data(),1,768,c.s,c,true);
    mix1(b.data(),8,2048,v,c,false);
    mix2(b.data(),v,c);
    for(unsigned q=0;q<8;q++) {
        write32(key+4*((2*q)*5%16),uint32_t(b[120+q].v));
        write32(key+4*((2*q+1)*5%16),b[120+q].v>>32);
    }
    HMAC_SHA256_Buf(key,64,sha,32,out);
    std::cout<<"{\"digest\":\"";
    for(auto x:out)std::printf("%02x",x);
    std::cout<<"\",";slice(b.data());std::cout<<"}\n";
}
