# acominer

An experimental ETH miner powered by Vulkan.

## IMPORTANT

acominer requires an AMD graphics card running Linux to run.

**acominer does not support other setups.**

## Running

Binaries are available from [releases](https://github.com/ishitatsuyuki/acominer/releases). Ubuntu 20.04 or later is supported.

If you are running a headless Ubuntu-based setup, make sure `vulkan-utils libvulkan1 libxcb-shm0` is installed.

Download, extract and execute `./run.sh`. See also [Usage](#Usage).

```
./run.sh stratum://0x1234567890123456789012345678901234567890.Worker:password@pool.example.com:4444
```

## Usage

```
acominer stratum://0x1234567890123456789012345678901234567890.Worker:password@pool.example.com:4444
```

- Supported protocol: Stratum v1, unencrypted only

See `acominer --help` for more information such as choosing a specific GPU or changing tuning parameters.

If acominers fails with "device out of memory", then you need to override the Vulkan driver used with `VK_ICD_FILENAMES`.
(See above.) You might also need to specify the device index explicitly with `-d` .

## Features

- Zero mining fee
- Minimal interference with graphics workload through the use of async compute
- Dependencies kept to a small amount to make audit easy

## Building

### Building the Mesa fork

You need the Meson build system and ninja installed along with C/C++ compilers. See
[Mesa Documentation](https://docs.mesa3d.org/install.html) for a list of dependencies.

```shell
git clone -b acominer https://gitlab.freedesktop.org/ishitatsuyuki/mesa.git # Clone the Mesa fork
cd mesa
meson build -Ddri-drivers= -Dgallium-drivers= -Dvulkan-drivers=amd -Dprefix=$PWD/build/install # Generate build files
cd build
ninja # Run the build
ninja install # Copy built drivers to prefix
```

Override the Vulkan driver as follows (every time) when you run acominer.

```shell
export VK_ICD_FILENAMES=/path/to/mesa/build/install/share/vulkan/icd.d/radeon_icd.x86_64.json
```

### Building the project

In addition to the Rust compiler, acominer needs Vulkan SDK to build. See
[here](https://github.com/vulkano-rs/vulkano#linux-specific-setup) for distro specific instructions.

Afterwards, just run `cargo build --release` and you should get a working binary as `target/release/acominer`.

## License

Original acominer code: Apache V2 or MIT License, at your option

Keccak hash code: GPLv3 (derived from ethminer)

## Tipping

You can do some mining on my behalf to show your appreciation:

```
stratum://0x76cB31Fdb28Ddf887AF3cA4A0f7786D02C86032a.[YOUR WORKER NAME]@asia1.ethermine.org:4444
```
