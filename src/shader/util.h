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
