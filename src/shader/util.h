uint bswap32(uint value)
{
    return (value & 0x000000FFU) << 24 |
               (value & 0x0000FF00U) << 8 |
               (value & 0x00FF0000U) >> 8 |
               (value & 0xFF000000U) >> 24;
}

uvec2 bswap64(uvec2 value) {
    return uvec2(bswap32(value.y), bswap32(value.x));
}

// Unsigned fast modulo, from Lemire, D., Kaser, O., & Kurz, N. (2019). Faster remainder by direct computation.
// URL: https://arxiv.org/pdf/1902.01961.pdf
uint fast_mod(uint n, uint64_t c, uint d) {
    uint64_t low_bits = c * n;
    uvec2 c_ = unpackUint2x32(c);
    uint lo = uint(low_bits);
    uint hi = uint(low_bits >> 32);
    // Our goal: ((__uint128_t)lowbits * d) >> 64
    uint discarded;
    uint lod;
    uint hid_lo;
    uint hid_hi;
    umulExtended(lo, d, lod, discarded);
    umulExtended(hi, d, hid_hi, hid_lo);
    uint carry;
    uaddCarry(lod, hid_lo, carry);
    return hid_hi + carry;
}

uvec2 fast_mod(uvec2 n, uint64_t c, uint d) {
    return uvec2(fast_mod(n.x, c, d), fast_mod(n.y, c, d));
}
