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

// Granlund-Montgomery-Warren fast division
// Implementation based on https://github.com/ridiculousfish/libdivide/blob/afb8a8a722c57e10f0dabc028cc24d44fae2a2f1/libdivide.h#L915
uint fast_mod(uint n, uvec3 data) {
    uint d = data.x;
    uint magic = data.y;
    uint shift = data.z;
    uint discarded;
    uint q;
    umulExtended(n, magic, q, discarded);
    uint t = (((n - q) >> 1) + q) >> shift;
    return n - t * d;
}

uvec2 fast_mod(uvec2 n, uvec3 d) {
    return uvec2(fast_mod(n.x, d), fast_mod(n.y, d));
}
