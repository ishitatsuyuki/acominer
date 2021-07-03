// Copyright 2021 Tatsuyuki Ishi <ishitatsuyuki@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

// Code based on https://github.com/rust-ethereum/ethash

use sha3::{Digest, Keccak256, Keccak512};
use byteorder::{LittleEndian, ByteOrder};
use core::ops::BitXor;

fn is_prime(x: usize) -> bool{
    miller_rabin::is_prime(&x, 1)
}

pub const DATASET_BYTES_INIT: usize = 1073741824; // 2 to the power of 30.
pub const DATASET_BYTES_GROWTH: usize = 8388608; // 2 to the power of 23.
pub const CACHE_BYTES_INIT: usize = 16777216; // 2 to the power of 24.
pub const CACHE_BYTES_GROWTH: usize = 131072; // 2 to the power of 17.
pub const MIX_BYTES: usize = 128;
pub const HASH_BYTES: usize = 64;
pub const CACHE_ROUNDS: usize = 3;

/// Get the cache size required given the block number.
pub fn get_cache_size(epoch: usize) -> usize {
    let mut sz = CACHE_BYTES_INIT + CACHE_BYTES_GROWTH * epoch;
    sz -= HASH_BYTES;
    while !is_prime(sz / HASH_BYTES) {
        sz -= 2 * HASH_BYTES;
    }
    sz
}

/// Get the full dataset size given the block number.
pub fn get_full_size(epoch: usize) -> usize {
    let mut sz = DATASET_BYTES_INIT + DATASET_BYTES_GROWTH * epoch;
    sz -= MIX_BYTES;
    while !is_prime(sz / MIX_BYTES) {
        sz -= 2 * MIX_BYTES
    }
    sz
}

fn fill_sha512(input: &[u8], a: &mut [u8], from_index: usize) {
    let mut hasher = Keccak512::default();
    hasher.update(input);
    let out = hasher.finalize();
    for i in 0..out.len() {
        a[from_index + i] = out[i];
    }
}

fn fill_sha256(input: &[u8], a: &mut [u8], from_index: usize) {
    let mut hasher = Keccak256::default();
    hasher.update(input);
    let out = hasher.finalize();
    for i in 0..out.len() {
        a[from_index + i] = out[i];
    }
}

/// Make an Ethash cache using the given seed.
pub fn make_cache(cache: &mut [u8], seed: [u8; 32]) {
    assert!(cache.len() % HASH_BYTES == 0);
    let n = cache.len() / HASH_BYTES;

    fill_sha512(&seed[..], cache, 0);

    for i in 1..n {
        let (last, next) = cache.split_at_mut(i * 64);
        fill_sha512(&last[(last.len()-64)..], next, 0);
    }

    for _ in 0..CACHE_ROUNDS {
        for i in 0..n {
            let v = (LittleEndian::read_u32(&cache[(i * 64)..]) as usize) % n;

            let mut r = [0u8; 64];
            for j in 0..64 {
                let a = cache[((n + i - 1) % n) * 64 + j];
                let b = cache[v * 64 + j];
                r[j] = a.bitxor(b);
            }
            fill_sha512(&r, cache, i * 64);
        }
    }
}

pub fn get_seed_hash(epoch: usize) -> [u8; 32] {
    let mut s = [0u8; 32];
    for _ in 0..epoch {
        fill_sha256(&s.clone(), &mut s, 0);
    }
    s
}

pub fn epoch_from_seed_hash(seed_hash: [u8; 32], limit: usize) -> Option<usize> {
    let mut s = [0u8; 32];
    for epoch in 1..=limit {
        fill_sha256(&s.clone(), &mut s, 0);
        if s == seed_hash {
            return Some(epoch);
        }
    }
    None
}
