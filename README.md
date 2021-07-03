# acominer

An experimental ETH miner powered by Vulkan.

## IMPORTANT

acominer requires an AMD graphics card running Linux and **a patched driver** to run. Follow the instructions below to
install a modified Mesa branch to your machine.

**acominer does not support other setups.**

### Installing the modified Mesa branch

You need the Meson build system and ninja installed along with C/C++ compilers. See
[Mesa Documentation](https://docs.mesa3d.org/install.html) for a list of dependencies.

```shell
git clone -b acominer https://gitlab.freedesktop.org/ishitatsuyuki/mesa.git # Clone the Mesa fork
cd mesa
meson build # Generate build files
cd build
ninja # Run the build
sudo ninja install # Install built drivers
```

Override the Vulkan driver as follows (every time) when you run acominer.

```shell
export VK_ICD_FILENAMES=/usr/local/share/vulkan/icd.d/radeon_icd.x86_64.json
```

If you ever need to remove the modified Mesa drivers from your system, run `sudo ninja uninstall` in the `build`
directory.

## Building

In addition to the Rust compiler, acominer needs Vulkan SDK to build. See
[here](https://github.com/vulkano-rs/vulkano#linux-specific-setup) for distro specific instructions.

Afterwards, just run `cargo build --release` and you should get a working binary as `target/release/acominer`.

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

## License

Original acominer code: Apache V2 or MIT License, at your option

Keccak hash code: GPLv3 (derived from ethminer)

## Tipping

You can do some mining on my behalf to show your appreciation:

```
stratum://0x76cB31Fdb28Ddf887AF3cA4A0f7786D02C86032a.[YOUR WORKER NAME]@asia1.ethermine.org:4444
```
