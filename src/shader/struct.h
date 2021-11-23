// Copyright 2021 Tatsuyuki Ishi <ishitatsuyuki@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

struct OutputEntry {
    uint gid;
    uint[8] mix;
};

layout(buffer_reference, std430, buffer_reference_align = 4) readonly buffer ReadDag
{
    uint values[];
};

layout(buffer_reference, std430, buffer_reference_align = 4) writeonly buffer WriteDag
{
    uint values[];
};

struct Config {
    ReadDag dag_read;
    WriteDag dag_write;
    uvec2 g_header[4];
    uint64_t start_nonce;
    uint64_t target;
};

#define OUTPUT_ENTRY_COUNT 8
struct Output {
    uint output_count;
    OutputEntry outputs[OUTPUT_ENTRY_COUNT];
};
