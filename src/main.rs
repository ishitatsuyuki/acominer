// Copyright 2021 Tatsuyuki Ishi <ishitatsuyuki@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::convert::TryInto;
use std::num::ParseIntError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use byteorder::{ByteOrder, LittleEndian};
use clap::ValueHint;
use indicatif::{HumanDuration, ProgressBar};
use url::Url;
use vulkano::buffer::{BufferAccess, BufferUsage, CpuAccessibleBuffer, DeviceLocalBuffer, ImmutableBuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage};
use vulkano::descriptor::descriptor_set::{PersistentDescriptorSet, UnsafeDescriptorSetLayout};
use vulkano::descriptor::DescriptorSet;
use vulkano::device::{Device, DeviceExtensions, Features, Queue};
use vulkano::instance::{ApplicationInfo, DriverId, Instance, InstanceExtensions, PhysicalDevice, QueueFamily};
use vulkano::pipeline::ComputePipeline;
use vulkano::pipeline::ComputePipelineAbstract;
use vulkano::sync;
use vulkano::sync::{Fence, FenceSignalFuture, FenceWaitError, GpuFuture};
use vulkano::Version;

use crate::cache::{epoch_from_seed_hash, get_cache_size, get_full_size, get_seed_hash, make_cache};
use crate::stratum::{JobInfo, StratumClient};

mod cache;
mod stratum;

const DEVICE_EXTENSIONS: DeviceExtensions = DeviceExtensions {
    khr_storage_buffer_storage_class: true,
    ext_subgroup_size_control: true,
    ..DeviceExtensions::none()
};

const BUFFER_USAGE: BufferUsage = BufferUsage {
    storage_buffer: true,
    device_address: true,
    ..BufferUsage::none()
};

const BUFFER_USAGE_WITH_TRANSFER: BufferUsage = BufferUsage {
    storage_buffer: true,
    transfer_destination: true,
    transfer_source: true,
    ..BufferUsage::none()
};

fn ceil_div(a: u64, b: u64) -> u64 {
    (a + b - 1) / b
}

// Calculate the constant for fast remainder. See shader/util.h.
fn lkk_cvalue(d: u32) -> u64 {
    0xFFFFFFFFFFFFFFFFu64 / d as u64 + 1
}

mod dag_cs {
    vulkano_shaders::shader! {
        ty: "compute",
        path: "src/shader/dag.comp",
        include: ["src/shader"],
        vulkan_version: "1.2",
        types_meta: {
            #[derive(Copy, Clone, Default, Debug)]
        }
    }
}

mod search_cs {
    vulkano_shaders::shader! {
        ty: "compute",
        path: "src/shader/search.comp",
        include: ["src/shader"],
        vulkan_version: "1.2",
        types_meta: {
            #[derive(Copy, Clone, Default, Debug)]
        }
    }
}

mod search_cs_64 {
    vulkano_shaders::shader! {
        ty: "compute",
        path: "src/shader/search.comp",
        include: ["src/shader"],
        vulkan_version: "1.2",
        define: [("WAVE64", "1")],
        types_meta: {
            #[derive(Copy, Clone, Default, Debug)]
        }
    }
}

#[allow(dead_code)]
struct MinerPipeline {
    config: dag_cs::ty::Config,
    dag0_buf: Arc<DeviceLocalBuffer<[u32]>>,
    search: Vec<SearchPipeline>,
}

#[allow(dead_code)]
struct SearchPipeline {
    output_buf: Arc<CpuAccessibleBuffer<search_cs::ty::Output>>,
    pipeline: Arc<ComputePipeline>,
    layout: Arc<UnsafeDescriptorSetLayout>,
    set: Arc<dyn DescriptorSet + Send + Sync>,
}

fn build_pipeline(device: Arc<Device>, queue: Arc<Queue>, queue_family: QueueFamily, seed_hash: &[u8], count: usize, subgroup_size: u32) -> MinerPipeline {
    println!("Generating light cache (CPU)...");
    let epoch = epoch_from_seed_hash(seed_hash.try_into().unwrap(), 65536).unwrap(); // FIXME: hardcoded limit
    let cache_size = get_cache_size(epoch);
    let full_size = get_full_size(epoch);
    let mut light = vec![0; cache_size];
    make_cache(&mut light, get_seed_hash(epoch));
    println!("Light cache generation done.");

    let dag_quantum = 64 * 64; // Size of DAG generated by each workgroup, in bytes
    let dag_wg_count = ceil_div(full_size as u64, dag_quantum);
    let dag_words = (dag_wg_count * dag_quantum / 4) as usize;
    let light_size = (cache_size / 64) as u32;
    let dag_size = (full_size / 64) as u32;
    let dag_size_mix = (full_size / 128) as u32;

    let mut config = dag_cs::ty::Config {
        dag_read: Default::default(),
        dag_write: Default::default(),
        light_size,
        light_size_c: lkk_cvalue(light_size),
        dag_size,
        dag_size_mix,
        dag_size_mix_c: lkk_cvalue(dag_size_mix),
        g_header: Default::default(),
        start_nonce: 0,
        target: 0,
        zero: 0,
        ..Default::default()
    };

    let dag0_buf = DeviceLocalBuffer::<[u32]>::array(device.clone(), dag_words, BUFFER_USAGE, Some(queue_family)).unwrap();
    config.dag_read = dag0_buf.raw_device_address().unwrap().into();
    config.dag_write = dag0_buf.raw_device_address().unwrap().into();

    let (light_buf, light_upload) = ImmutableBuffer::from_iter(light.iter().cloned(), BUFFER_USAGE, queue.clone()).unwrap();
    light_upload.then_signal_fence_and_flush().unwrap().wait(None).unwrap();
    println!("Light cache transferred to GPU.");

    let dag_pipeline = Arc::new({
        let shader = dag_cs::Shader::load(device.clone()).unwrap();
        ComputePipeline::new(device.clone(), &shader.main_entry_point(), &(), None, Some(subgroup_size)).unwrap()
    });

    let dag_layout = dag_pipeline.layout().descriptor_set_layout(0).unwrap();
    let dag_set = Arc::new(
        PersistentDescriptorSet::start(dag_layout.clone())
            .add_buffer(light_buf.clone()).unwrap()
            .build().unwrap(),
    );

    let mut search_pipelines = vec![];
    for _ in 0..count {
        let output_buf = {
            CpuAccessibleBuffer::from_data(device.clone(), BUFFER_USAGE_WITH_TRANSFER, false, search_cs::ty::Output::default()).unwrap()
        };

        let search_pipeline = Arc::new({
            if subgroup_size == 64 {
                let shader = search_cs_64::Shader::load(device.clone()).unwrap();
                ComputePipeline::new(device.clone(), &shader.main_entry_point(), &(), None, Some(subgroup_size)).unwrap()
            } else {
                let shader = search_cs::Shader::load(device.clone()).unwrap();
                ComputePipeline::new(device.clone(), &shader.main_entry_point(), &(), None, Some(subgroup_size)).unwrap()
            }
        });

        let search_layout = search_pipeline.layout().descriptor_set_layout(0).unwrap();
        let search_set = Arc::new(
            PersistentDescriptorSet::start(search_layout.clone())
                .add_buffer(output_buf.clone()).unwrap()
                .build().unwrap(),
        );

        search_pipelines.push(SearchPipeline {
            output_buf,
            pipeline: search_pipeline.clone(),
            layout: search_layout.clone(),
            set: search_set.clone(),
        });
    }

    let progress = ProgressBar::new(dag_wg_count);
    println!("Building DAG...");
    let mut completed = 0;
    while completed != dag_wg_count {
        let to_schedule = std::cmp::min(std::cmp::min(14400, device.physical_device().properties().max_compute_work_group_count.unwrap()[0] as u64), dag_wg_count - completed);
        config.start_nonce = (completed * 64) as _;

        let mut builder = AutoCommandBufferBuilder::primary(
            device.clone(),
            queue.family(),
            CommandBufferUsage::OneTimeSubmit,
        ).unwrap();
        builder.dispatch([to_schedule as u32, 1, 1], dag_pipeline.clone(), dag_set.clone(), dag_cs::ty::PushConstants { conf: config }, vec![]).unwrap();
        let command_buffer = builder.build().unwrap();

        let future = sync::now(device.clone())
            .then_execute(queue.clone(), command_buffer)
            .unwrap()
            .then_signal_fence_and_flush()
            .unwrap();

        future.wait(None).unwrap();
        completed += to_schedule;
        progress.set_position(completed);
    }
    progress.finish();
    println!("DAG built in {}", HumanDuration(progress.elapsed()));

    MinerPipeline {
        config,
        dag0_buf,
        search: search_pipelines,
    }
}

fn main() -> anyhow::Result<()> {
    let stop = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let stop = stop.clone();
        move || {
            if !stop.swap(true, Ordering::SeqCst) {
                println!("Ctrl+C received, shutting down...");
            } else {
                println!("Ctrl+C pressed twice, force exiting.");
                std::process::exit(130);
            }
        }
    }).unwrap();
    let app_info = ApplicationInfo {
        application_name: Some(env!("CARGO_PKG_NAME").into()),
        application_version: Some(Version {
            major: env!("CARGO_PKG_VERSION_MAJOR").parse().unwrap(),
            minor: env!("CARGO_PKG_VERSION_MINOR").parse().unwrap(),
            patch: env!("CARGO_PKG_VERSION_PATCH").parse().unwrap(),
        }),
        engine_name: None,
        engine_version: None,
    };
    let instance = Instance::new(Some(&app_info), Version::V1_2, &InstanceExtensions::none(), None).unwrap();
    let app = clap::App::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .arg(
            clap::Arg::new("pool")
                .required(true)
                .validator(|s| Url::parse(s))
                .about("The URL of the pool to connect to")
                .value_hint(ValueHint::Url)
        )
        .arg(
            clap::Arg::new("device")
                .short('d')
                .long("device")
                .about("The index of GPU device to use")
                .validator(|s| -> Result<_, ParseIntError> { Ok(PhysicalDevice::from_index(&instance, s.parse()?)) })
                .default_value("0")
        )
        .arg(
            clap::Arg::new("workgroups")
                .short('w')
                .long("workgroups")
                .about("Workgroup count")
                .long_about(r"Specify the amount of work to launch at once to the GPU, in number of threads divided by 64.
A large value increases efficiency, but also increases latency (therefore increasing the risk of stale shares) and might make the system less responsive as well.
Popular wisdoms say that this should be a multiple of your CU count.")
                .validator(|s| s.parse::<u64>())
                .default_value("5760")
        )
        .arg(
            clap::Arg::new("buffers")
                .long("buffers")
                .about("Amount of buffers")
                .long_about(r"Setting this option to 2 (default) enables double buffering which helps keeping the GPU busy. You should not need to modify this value.")
                .validator(|s| s.parse::<usize>())
                .default_value("2")
        )
        .arg(
            clap::Arg::new("subgroup size")
                .long("subgroup-size")
                .about("Subgroup size")
                .takes_value(true)
                .long_about("Control the subgroup size to be used on RDNA or later. On Mesa drivers, this defaults to 64.")
        )
        .arg(
            clap::Arg::new("target hashrate")
                .long("target-hashrate")
                .about("Target hashrate for latency reduction")
                .takes_value(true)
                .long_about("Set a target hashrate (hash/second) to pace work submission, reducing buffering and latency.")
        )
        .get_matches();
    let url: Url = app.value_of_t("pool").unwrap();
    let mut client = StratumClient::new(
        concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION")),
        url.host_str().ok_or_else(|| anyhow::anyhow!("URL must include a hostname"))?,
        url.port().unwrap_or(4444),
        url.username(),
        url.password().unwrap_or("X"),
    )?;

    let physical: PhysicalDevice = PhysicalDevice::from_index(&instance, app.value_of("device").unwrap().parse().unwrap()).unwrap();
    assert_eq!(physical.properties().driver_id, Some(DriverId::MesaRADV));
    if physical.properties().driver_id == Some(DriverId::AMDOpenSource) {
        panic!("AMDVLK will corrupt GPU state when allocating 4GB+ of memory. DO NOT USE!");
    }

    let queue_family = physical
        .queue_families()
        .find(|&q| q.supports_compute() && !q.supports_graphics()) // Prefer compute-only queues which do not slow down graphic workloads
        .unwrap_or_else(|| physical
            .queue_families()
            .find(|&q| q.supports_compute()).unwrap());

    println!(
        "Using device: {} (type: {:?})",
        physical.properties().device_name.as_ref().unwrap(),
        physical.properties().device_type.unwrap()
    );

    let pipeline_count: usize = app.value_of_t("buffers").unwrap();

    // Now initializing the device.
    let (device, mut queues) = Device::new(
        physical,
        &Features {
            shader_int64: true,
            buffer_device_address: true,
            ..Features::none()
        },
        &DEVICE_EXTENSIONS,
        std::iter::once((queue_family, 0.5)),
    )
        .unwrap();

    let queue = queues.next().unwrap();

    let subgroup_size: u32 = app.value_of_t("subgroup size").unwrap_or_else(|_| physical.properties().subgroup_size.unwrap());
    println!("Using subgroup size: {}", subgroup_size);

    let mut nonce: u64 = rand::random();
    let wg_count = app.value_of_t::<u64>("workgroups").unwrap() * 64 / subgroup_size as u64;
    let hash_quantum = wg_count * subgroup_size as u64;

    let target_hashrate: Option<u64> = app.value_of_t("target hashrate").ok();
    let pacing = target_hashrate.map(|x| {
        Duration::from_secs_f64(hash_quantum as f64 / x as f64)
    });
    if let Some(pace) = pacing {
        println!("Using pacing: {:?} per submission", pace);
    }
    while !stop.load(Ordering::SeqCst) {
        let job = client.current_job();
        let mut pipeline = build_pipeline(device.clone(), queue.clone(), queue_family, &job.seed_hash, pipeline_count, subgroup_size);
        let current_seed_hash = job.seed_hash;
        let mut hash_counter = 0u64;
        let mut start_time = Instant::now();
        let mut current_pipeline = 0;
        let mut futures: Vec<Option<(FenceSignalFuture<_>, u64, JobInfo)>> = (0..pipeline_count).map(|_| None).collect();
        let mut cleanup_counter = 0;
        let mut last_time = Instant::now();
        loop {
            let target = pacing.map(|pacing| {
                let now = Instant::now();
                let target = (last_time + pacing).max(now);
                last_time = target;
                target
            });
            if let Some((future, ..)) = futures[current_pipeline].as_ref() {
                future.wait(None).unwrap();
            }
            loop {
                for i in 0..pipeline_count {
                    if let Some((future, start_nonce, job)) = futures[i].as_mut() {
                        if future.get_fence().map(|x| x.ready().unwrap()).unwrap_or(true) {
                            future.wait(None).unwrap();
                            let output_buf = pipeline.search[i].output_buf.read().unwrap();
                            for i in 0..output_buf.output_count {
                                client.submit(&job, *start_nonce + output_buf.outputs[i as usize].gid as u64);
                            }
                            futures[i] = None;
                        }
                    }
                }
                let wait: Vec<_> = futures.iter_mut().filter_map(|x| x.as_mut().and_then(|x| x.0.get_fence())).map(|x| &*x).collect();
                if !wait.is_empty() {
                    match Fence::multi_wait(wait, target.map(|t| t.saturating_duration_since(Instant::now())), false) {
                        Ok(..) => {}
                        Err(FenceWaitError::Timeout) => { break; }
                        Err(x) => panic!("error waiting for fence: {}", x),
                    }
                } else {
                    if let Some(target) = target {
                        std::thread::sleep(target.saturating_duration_since(Instant::now()));
                    }
                    break;
                }
            }
            let job = client.current_job();
            if stop.load(Ordering::SeqCst) || job.seed_hash != current_seed_hash
            {
                cleanup_counter += 1;
                if cleanup_counter != pipeline_count {
                    current_pipeline = (current_pipeline + 1) % pipeline_count;
                    continue;
                } else {
                    break;
                }
            }
            let nonce_mask = (!0u64) >> (job.pool_nonce_bytes * 8);
            if ((nonce + hash_quantum) & nonce_mask) < hash_quantum { // Wraparound
                nonce = 0;
            }

            pipeline.config.start_nonce = job.extra_nonce | (nonce & nonce_mask);
            pipeline.search[current_pipeline].output_buf.write().unwrap().output_count = 0;
            for i in 0..4 {
                let mut target = [0; 2];
                LittleEndian::read_u32_into(&job.header_hash[i * 8..i * 8 + 8], &mut target);
                pipeline.config.g_header[i] = target;
            }
            pipeline.config.target = job.difficulty;

            let mut builder = AutoCommandBufferBuilder::primary(
                device.clone(),
                queue_family,
                CommandBufferUsage::OneTimeSubmit,
            ).unwrap();
            builder.dispatch([wg_count as _, 1, 1], pipeline.search[current_pipeline].pipeline.clone(), pipeline.search[current_pipeline].set.clone(), dag_cs::ty::PushConstants { conf: pipeline.config }, vec![]).unwrap();
            let command_buffer = builder.build().unwrap();

            let future = sync::now(device.clone())
                .then_execute(queue.clone(), command_buffer)
                .unwrap()
                .then_signal_fence_and_flush()
                .unwrap();
            assert!(futures[current_pipeline].is_none());
            futures[current_pipeline] = Some((future, pipeline.config.start_nonce, job.clone()));

            nonce += hash_quantum;
            hash_counter += hash_quantum;
            if start_time.elapsed() > Duration::from_secs(5) {
                println!("{:.02} MH/s", hash_counter as f64 / start_time.elapsed().as_secs_f64() / 1000. / 1000.);
                start_time = Instant::now();
                hash_counter = 0;
            }
            current_pipeline = (current_pipeline + 1) % pipeline_count;
        }
    }
    Ok(())
}
