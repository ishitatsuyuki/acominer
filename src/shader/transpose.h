// Copyright 2021 Tatsuyuki Ishi <ishitatsuyuki@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// AoS transpose functions inspired by Catanzaro, B., Keller, A., & Garland, M. (2014). A decomposition for in-place matrix transposition. PPoPP '14.
// URL: https://on-demand.gputechconf.com/gtc/2014/presentations/S4664-transposition-fast-array-structure-accesses.pdf
// Some differences from the paper:
// 1. Shared memory is used instead of subgroup shuffle, because it has better support and likely gives shorter
//    generated code especially for the row shuffle step which was done on registers and took O(n log n) steps in
//    their implementation.
// 2. The row/column shuffle steps are fused so $i$ in the row shuffle has been substituted with that from $s'$.
// Note that the authors chose gather for column shuffle but scatter for row shuffle so keep an eye on that when reading
// their pseudocode.

// Column-to-row (AoS to SoA) transpose. m is size of the struct, n is size of the array.
// Requirements: m < n, m and n are power of twos
// sh is a shared memory array defined as `shared uint sh[m][n*k]` where k can be any natural number
// A barrier is required between the scatter and gather step. Although, on AMD hardware that translates to a noop.
//
// To obtain the definition, follow these steps:
// 1. Invert the formula for "conflict removal" step to obtain the scatter version of it.
// 2. Substitute i in the "row shuffle" formula with r_j(i) we obtained in step 1.
//    These two formula forms the C2R scatter step.
// 3. Use the "column shuffle" formula as-is for the gather step.
#define C2R_SCATTER_IMPL(sh, m, n, i, j, val) sh[(i-j/(n/m))&(m-1)][i+((j*m)&(n-1))+(j&~(n-1))] = val
#define C2R_SCATTER(sh, m, n, i, j, val) C2R_SCATTER_IMPL(sh, (m), (n), (i), (j), val)
#define C2R_GATHER_IMPL(sh, m, n, i, j) sh[(j-i)&(m-1)][j]
#define C2R_GATHER(sh, m, n, i, j) C2R_GATHER_IMPL(sh, (m), (n), (i), (j))

// Row-to-column (SoA to AoS) transpose. Requirements are same as above.
// To obtain the definition, just swap the scatter and gather op above
#define R2C_SCATTER_IMPL(sh, m, n, i, j, val) sh[(j-i)&(m-1)][j] = val
#define R2C_SCATTER(sh, m, n, i, j, val) R2C_SCATTER_IMPL(sh, (m), (n), (i), (j), val)
#define R2C_GATHER_IMPL(sh, m, n, i, j) sh[(i-j/(n/m))&(m-1)][i+((j*m)&(n-1))+(j&~(n-1))]
#define R2C_GATHER(sh, m, n, i, j) R2C_GATHER_IMPL(sh, (m), (n), (i), (j))
