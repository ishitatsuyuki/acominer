// Copyright 2017 Yurio Miyazawa (a.k.a zawawa) <me@yurio.net>
//
// This file is part of Gateless Gate Sharp.
//
// Gateless Gate Sharp is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// Gateless Gate Sharp is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with Gateless Gate Sharp.  If not, see <http://www.gnu.org/licenses/>.

/* FNV */

#define FNV_PRIME 0x01000193u
#define fnv(x, y) ((x) * FNV_PRIME ^ (y))

/* Keccak */

#define fshr(lo, hi, shift) ((lo) >> (shift)) | ((hi) << (32 - (shift)))
#define fshl(lo, hi, shift) ((lo) << (shift)) | ((hi) >> (32 - (shift)))
// 64-bit rotate (in left direction) by `shift`.
uvec2 ROTL64_1(uvec2 x, uint shift) {
    return fshr(x.yx, x, 32 - shift);
}
// 64-bit rotate (in left direction) by `shift + 32`.
uvec2 ROTL64_2(uvec2 x, uint shift) {
    return fshr(x, x.yx, 32 - shift);
}

#define bitselect(src2, src1, src0) ((src0) & (src1)) | (~(src0) & (src2))

const uvec2 Keccak_f1600_RC[24] = {
    uvec2(0x00000001, 0x00000000),
    uvec2(0x00008082, 0x00000000),
    uvec2(0x0000808a, 0x80000000),
    uvec2(0x80008000, 0x80000000),
    uvec2(0x0000808b, 0x00000000),
    uvec2(0x80000001, 0x00000000),
    uvec2(0x80008081, 0x80000000),
    uvec2(0x00008009, 0x80000000),
    uvec2(0x0000008a, 0x00000000),
    uvec2(0x00000088, 0x00000000),
    uvec2(0x80008009, 0x00000000),
    uvec2(0x8000000a, 0x00000000),
    uvec2(0x8000808b, 0x00000000),
    uvec2(0x0000008b, 0x80000000),
    uvec2(0x00008089, 0x80000000),
    uvec2(0x00008003, 0x80000000),
    uvec2(0x00008002, 0x80000000),
    uvec2(0x00000080, 0x80000000),
    uvec2(0x0000800a, 0x00000000),
    uvec2(0x8000000a, 0x80000000),
    uvec2(0x80008081, 0x80000000),
    uvec2(0x00008080, 0x80000000),
    uvec2(0x80000001, 0x00000000),
    uvec2(0x80008008, 0x80000000),
};

#define KECCAKF_1600_RND_A(a) do { \
    uvec2 m0 = a[0] ^ a[5] ^ a[10] ^ a[15] ^ a[20] ^ ROTL64_1(a[2] ^ a[7] ^ a[12] ^ a[17] ^ a[22], 1);\
    uvec2 m1 = a[1] ^ a[6] ^ a[11] ^ a[16] ^ a[21] ^ ROTL64_1(a[3] ^ a[8] ^ a[13] ^ a[18] ^ a[23], 1);\
    uvec2 m2 = a[2] ^ a[7] ^ a[12] ^ a[17] ^ a[22] ^ ROTL64_1(a[4] ^ a[9] ^ a[14] ^ a[19] ^ a[24], 1);\
    uvec2 m3 = a[3] ^ a[8] ^ a[13] ^ a[18] ^ a[23] ^ ROTL64_1(a[0] ^ a[5] ^ a[10] ^ a[15] ^ a[20], 1);\
    uvec2 m4 = a[4] ^ a[9] ^ a[14] ^ a[19] ^ a[24] ^ ROTL64_1(a[1] ^ a[6] ^ a[11] ^ a[16] ^ a[21], 1);\
    \
    uvec2 tmp = a[1]^m0;\
    \
    a[0] ^= m4;\
    a[5] ^= m4; \
    a[10] ^= m4; \
    a[15] ^= m4; \
    a[20] ^= m4; \
    \
    a[6] ^= m0; \
    a[11] ^= m0; \
    a[16] ^= m0; \
    a[21] ^= m0; \
    \
    a[2] ^= m1; \
    a[7] ^= m1; \
    a[12] ^= m1; \
    a[17] ^= m1; \
    a[22] ^= m1; \
    \
    a[3] ^= m2; \
    a[8] ^= m2; \
    a[13] ^= m2; \
    a[18] ^= m2; \
    a[23] ^= m2; \
    \
    a[4] ^= m3; \
    a[9] ^= m3; \
    a[14] ^= m3; \
    a[19] ^= m3; \
    a[24] ^= m3; \
    \
    a[1] = ROTL64_2(a[6], 12);\
    a[6] = ROTL64_1(a[9], 20);\
    a[9] = ROTL64_2(a[22], 29);\
    a[22] = ROTL64_2(a[14], 7);\
    a[14] = ROTL64_1(a[20], 18);\
    a[20] = ROTL64_2(a[2], 30);\
    a[2] = ROTL64_2(a[12], 11);\
    a[12] = ROTL64_1(a[13], 25);\
    a[13] = ROTL64_1(a[19],  8);\
    a[19] = ROTL64_2(a[23], 24);\
    a[23] = ROTL64_2(a[15], 9);\
    a[15] = ROTL64_1(a[4], 27);\
    a[4] = ROTL64_1(a[24], 14);\
    a[24] = ROTL64_1(a[21],  2);\
    a[21] = ROTL64_2(a[8], 23);\
    a[8] = ROTL64_2(a[16], 13);\
    a[16] = ROTL64_2(a[5], 4);\
    a[5] = ROTL64_1(a[3], 28);\
    a[3] = ROTL64_1(a[18], 21);\
    a[18] = ROTL64_1(a[17], 15);\
    a[17] = ROTL64_1(a[11], 10);\
    a[11] = ROTL64_1(a[7],  6);\
    a[7] = ROTL64_1(a[10],  3);\
    a[10] = ROTL64_1(tmp,  1);\
  } while(false)

#define KECCAKF_1600_RND_B(a, i, outsz) do { \
    uvec2 m5 = a[0]; uvec2 m6 = a[1]; a[0] = bitselect(a[0]^a[2],a[0],a[1]); \
    a[0] ^= Keccak_f1600_RC[i]; \
    if (outsz > 1) { \
        a[1] = bitselect(a[1]^a[3],a[1],a[2]); a[2] = bitselect(a[2]^a[4],a[2],a[3]); a[3] = bitselect(a[3]^m5,a[3],a[4]); a[4] = bitselect(a[4]^m6,a[4],m5);\
        if (outsz > 4) { \
            m5 = a[5]; m6 = a[6]; a[5] = bitselect(a[5]^a[7],a[5],a[6]); a[6] = bitselect(a[6]^a[8],a[6],a[7]); a[7] = bitselect(a[7]^a[9],a[7],a[8]); a[8] = bitselect(a[8]^m5,a[8],a[9]); a[9] = bitselect(a[9]^m6,a[9],m5);\
            if (outsz > 8) { \
                m5 = a[10]; m6 = a[11]; a[10] = bitselect(a[10]^a[12],a[10],a[11]); a[11] = bitselect(a[11]^a[13],a[11],a[12]); a[12] = bitselect(a[12]^a[14],a[12],a[13]); a[13] = bitselect(a[13]^m5,a[13],a[14]); a[14] = bitselect(a[14]^m6,a[14],m5);\
                m5 = a[15]; m6 = a[16]; a[15] = bitselect(a[15]^a[17],a[15],a[16]); a[16] = bitselect(a[16]^a[18],a[16],a[17]); a[17] = bitselect(a[17]^a[19],a[17],a[18]); a[18] = bitselect(a[18]^m5,a[18],a[19]); a[19] = bitselect(a[19]^m6,a[19],m5);\
                m5 = a[20]; m6 = a[21]; a[20] = bitselect(a[20]^a[22],a[20],a[21]); a[21] = bitselect(a[21]^a[23],a[21],a[22]); a[22] = bitselect(a[22]^a[24],a[22],a[23]); a[23] = bitselect(a[23]^m5,a[23],a[24]); a[24] = bitselect(a[24]^m6,a[24],m5);\
            } \
        } \
    } \
 } while(false)


#define KECCAK_PROCESS(st, in_size, out_size)    do { \
    KECCAKF_1600_RND_A(st); \
    [[dont_unroll]] \
    for (int r = 0; r < 23; ++r) { \
        KECCAKF_1600_RND_B(st, r, 25); \
        KECCAKF_1600_RND_A(st); \
    } \
    KECCAKF_1600_RND_B(st, 23, out_size); \
} while(false)
