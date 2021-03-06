// Copyright 2021 Tatsuyuki Ishi <ishitatsuyuki@gmail.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// https://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or https://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::io::{self, Write};
use std::num::ParseIntError;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Context;
use ash::vk::{CalibratedTimestampInfoEXT, TimeDomainEXT};
use byteorder::{ByteOrder, LittleEndian};
use clap::ValueHint;
use indicatif::{ProgressBar, ProgressStyle, WeakProgressBar};
use once_cell::sync::Lazy;
use tracing::{error, info};
use tracing_subscriber;
use tracing_subscriber::EnvFilter;
use url::Url;
use vulkano::{DeviceSize, sync, VulkanObject};
use vulkano::buffer::{BufferAccess, BufferUsage, CpuAccessibleBuffer, DeviceLocalBuffer, ImmutableBuffer};
use vulkano::command_buffer::{AutoCommandBufferBuilder, CommandBufferUsage};
use vulkano::descriptor_set::{DescriptorSetWithOffsets, layout::DescriptorSetLayout, PersistentDescriptorSet};
use vulkano::device::{Device, DeviceExtensions, Features, Queue};
use vulkano::device::physical::{PhysicalDevice, QueueFamily};
use vulkano::instance::{ApplicationInfo, Instance, InstanceExtensions};
use vulkano::pipeline::{ComputePipeline, Pipeline, PipelineBindPoint};
use vulkano::query::{QueryPool, QueryResultFlags, QueryType};
use vulkano::sync::{Fence, FenceSignalFuture, FenceWaitError, GpuFuture, PipelineStage};
use vulkano::Version;

use crate::cache::{get_cache_size, get_full_size, get_seed_hash, make_cache};
use crate::lfx::LatencyFleX;
use crate::stratum::{JobInfo, StratumClient};

mod cache;
mod stratum;
mod lfx;

const DEVICE_EXTENSIONS: DeviceExtensions = DeviceExtensions {
    khr_storage_buffer_storage_class: true,
    ext_subgroup_size_control: true,
    amd_shader_ballot: true,
    ext_calibrated_timestamps: true,
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
    epoch: usize,
}

#[allow(dead_code)]
struct SearchPipeline {
    output_buf: Arc<CpuAccessibleBuffer<search_cs::ty::Output>>,
    pipeline: Arc<ComputePipeline>,
    layout: Arc<DescriptorSetLayout>,
    set: DescriptorSetWithOffsets,
    query: Arc<QueryPool>,
}

fn build_pipeline(device: Arc<Device>, queue: Arc<Queue>, queue_family: QueueFamily, epoch: usize, count: usize, subgroup_size: u32) -> anyhow::Result<MinerPipeline> {
    let cache_size = get_cache_size(epoch);
    let full_size = get_full_size(epoch);

    let dag_quantum = 64 * 64; // Size of DAG generated by each workgroup, in bytes
    let dag_wg_count = ceil_div(full_size as u64, dag_quantum);
    let dag_words = (dag_wg_count * dag_quantum / 4) as usize;
    let light_size = (cache_size / 64) as u32;
    let dag_size_mix = (full_size / 128) as u32;

    let dag_pipeline = {
        let shader = dag_cs::load(device.clone())?;
        ComputePipeline::new(device.clone(), shader.entry_point("main").unwrap(), &dag_cs::SpecializationConstants { light_size }, None, Some(subgroup_size), |_| {})?
    };
    let dag_layout = &dag_pipeline.layout().descriptor_set_layouts()[0];

    let search_pipeline = if subgroup_size == 64 {
        let shader = search_cs_64::load(device.clone())?;
        ComputePipeline::new(device.clone(), shader.entry_point("main").unwrap(), &search_cs_64::SpecializationConstants { dag_size_mix }, None, Some(subgroup_size), |_| {})?
    } else {
        let shader = search_cs::load(device.clone())?;
        ComputePipeline::new(device.clone(), shader.entry_point("main").unwrap(), &search_cs::SpecializationConstants { dag_size_mix }, None, Some(subgroup_size), |_| {})?
    };
    let search_layout = &search_pipeline.layout().descriptor_set_layouts()[0];

    let mut config = dag_cs::ty::Config {
        dag_read: Default::default(),
        dag_write: Default::default(),
        g_header: Default::default(),
        start_nonce: 0,
        target: 0,
        ..Default::default()
    };

    let dag0_buf = DeviceLocalBuffer::<[u32]>::array(device.clone(), dag_words as DeviceSize, BUFFER_USAGE, Some(queue_family))
        .context("Failed to allocate DAG. This can mean that the amount of VRAM on your GPU is not sufficient, or you are running an unsupported driver.")?;
    config.dag_read = dag0_buf.raw_device_address()?.into();
    config.dag_write = dag0_buf.raw_device_address()?.into();

    info!("Generating light cache (CPU)...");
    let mut light = vec![0; cache_size];
    make_cache(&mut light, get_seed_hash(epoch));
    info!("Light cache generation done.");
    let (light_buf, light_upload) = ImmutableBuffer::from_iter(light.iter().cloned(), BUFFER_USAGE, queue.clone())?;
    light_upload.then_signal_fence_and_flush()?.wait(None)?;
    info!("Light cache transferred to GPU.");

    let mut dag_set_builder = PersistentDescriptorSet::start(dag_layout.clone());
    dag_set_builder.add_buffer(light_buf.clone())?;
    let dag_set = dag_set_builder.build()?;

    let mut search_pipelines = vec![];
    for _ in 0..count {
        let output_buf = {
            CpuAccessibleBuffer::from_data(device.clone(), BUFFER_USAGE_WITH_TRANSFER, true, search_cs::ty::Output::default())?
        };

        let mut search_set_builder = PersistentDescriptorSet::start(search_layout.clone());
        search_set_builder.add_buffer(output_buf.clone())?;
        let search_set = search_set_builder.build()?;

        let query = QueryPool::new(device.clone(), QueryType::Timestamp, 1)?;

        search_pipelines.push(SearchPipeline {
            output_buf,
            pipeline: search_pipeline.clone(),
            layout: search_layout.clone(),
            set: search_set.clone().into(),
            query,
        });
    }

    let progress = ProgressBar::new(dag_wg_count);
    *PROGRESS_BAR.lock().unwrap() = progress.downgrade();
    info!(epoch, "Building DAG...");
    let mut completed = 0;
    while completed != dag_wg_count {
        let to_schedule = std::cmp::min(std::cmp::min(14400, device.physical_device().properties().max_compute_work_group_count[0] as u64), dag_wg_count - completed);
        config.start_nonce = (completed * 64) as _;

        let mut builder = AutoCommandBufferBuilder::primary(
            device.clone(),
            queue.family(),
            CommandBufferUsage::OneTimeSubmit,
        )?;
        builder
            .bind_pipeline_compute(dag_pipeline.clone())
            .bind_descriptor_sets(
                PipelineBindPoint::Compute,
                dag_pipeline.layout().clone(),
                0,
                dag_set.clone(),
            )
            .push_constants(dag_pipeline.layout().clone(), 0, dag_cs::ty::PushConstants { conf: config });
        builder.dispatch([to_schedule as u32, 1, 1])?;
        let command_buffer = builder.build()?;

        let future = sync::now(device.clone())
            .then_execute(queue.clone(), command_buffer)?
            .then_signal_fence_and_flush()?;

        future.wait(None)?;
        completed += to_schedule;
        progress.set_position(completed);
    }
    progress.finish();
    info!(duration = ?progress.elapsed(), "DAG built");

    Ok(MinerPipeline {
        config,
        dag0_buf,
        search: search_pipelines,
        epoch,
    })
}

struct SuspendProgressBarWriter<T: Write>(T);

impl<T: Write> SuspendProgressBarWriter<T> {
    fn suspend<F: FnOnce(&mut Self) -> R, R>(&mut self, f: F) -> R {
        let handle = PROGRESS_BAR.lock().unwrap().upgrade();
        if let Some(p) = handle {
            p.suspend(|| f(self))
        } else {
            f(self)
        }
    }
}

static PROGRESS_BAR: Lazy<Mutex<WeakProgressBar>> = Lazy::new(|| Mutex::new(WeakProgressBar::default()));

impl<T: Write> Write for SuspendProgressBarWriter<T> {
    #[inline]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.suspend(|this| this.0.write(buf))
    }

    #[inline]
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        self.suspend(|this| this.0.write_vectored(bufs))
    }

    #[inline]
    fn flush(&mut self) -> io::Result<()> {
        self.suspend(|this| this.0.flush())
    }

    #[inline]
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.suspend(|this| this.0.write_all(buf))
    }

    #[inline]
    fn write_fmt(&mut self, fmt: std::fmt::Arguments<'_>) -> io::Result<()> {
        self.suspend(|this| this.0.write_fmt(fmt))
    }
}

fn main() -> anyhow::Result<()> {
    // FIXME: https://github.com/tokio-rs/tracing/issues/735 https://github.com/tokio-rs/tracing/issues/1697
    let log_env = std::env::var(EnvFilter::DEFAULT_ENV).map(|mut s| {
        s.insert_str(0, "info,");
        s
    }).unwrap_or_else(|_| String::from("info"));
    tracing_subscriber::fmt().with_writer(|| SuspendProgressBarWriter(std::io::stdout())).with_env_filter(EnvFilter::new(log_env)).init();
    let stop = Arc::new(AtomicBool::new(false));
    ctrlc::set_handler({
        let stop = stop.clone();
        move || {
            if !stop.swap(true, Ordering::SeqCst) {
                info!("Ctrl+C received, shutting down...");
            } else {
                error!("Ctrl+C pressed twice, force exiting.");
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
    let instance = Instance::new(Some(&app_info), Version::V1_2, &InstanceExtensions::none(), None)?;
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
            clap::Arg::new("lfx")
                .long("latencyflex")
                .about("Enable LatencyFleX (TM) latency reduction")
                .long_about("Use LatencyFlex (TM) to reduce buffering and latency, and improve system responsiveness.")
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

    let device_idx = app.value_of("device").unwrap().parse().unwrap();
    let physical: PhysicalDevice = PhysicalDevice::from_index(&instance, device_idx).ok_or_else(|| anyhow::anyhow!("Device {} not found", device_idx))?;

    let queue_family = physical
        .queue_families()
        .find(|&q| q.supports_compute() && !q.supports_graphics()) // Prefer compute-only queues which do not slow down graphic workloads
        .unwrap_or_else(|| physical
            .queue_families()
            .find(|&q| q.supports_compute()).unwrap());

    info!(
        device = %physical.properties().device_name,
        "type" = ?physical.properties().device_type
    );

    let num_pipelines: usize = app.value_of_t("buffers").unwrap();

    // Now initializing the device.
    let (device, queues) = Device::new(
        physical,
        &Features {
            shader_int64: true,
            buffer_device_address: true,
            ..Features::none()
        },
        &DEVICE_EXTENSIONS,
        std::iter::repeat((queue_family, 0.5)).take(2),
    )?;

    let queues: Vec<_> = queues.collect();

    let subgroup_size: u32 = app.value_of_t("subgroup size").unwrap_or_else(|_| physical.properties().subgroup_size.unwrap());
    info!(subgroup_size = subgroup_size);

    let mut nonce: u64 = rand::random();
    let wg_count = app.value_of_t::<u64>("workgroups").unwrap() * 64 / subgroup_size as u64;
    let hash_quantum = wg_count * subgroup_size as u64;

    let mut lfx = LatencyFleX::new();
    let mut frames = 0;

    while !stop.load(Ordering::SeqCst) {
        let job = client.current_job();
        let mut pipeline = build_pipeline(device.clone(), queues[0].clone(), queue_family, job.epoch, num_pipelines, subgroup_size)?;
        let period = physical.properties().timestamp_period;
        let mut hash_counter = 0u64;
        let mut start_time = Instant::now();
        let mut current_pipeline = 0;
        let mut futures: Vec<Option<(FenceSignalFuture<_>, u64, u64, JobInfo)>> = (0..num_pipelines).map(|_| None).collect();
        let mut cleanup_counter = 0;
        let spinner_style = ProgressStyle::default_spinner()
            .template("{wide_msg}");
        let progress = ProgressBar::new_spinner();
        progress.set_style(spinner_style);
        *PROGRESS_BAR.lock().unwrap() = progress.downgrade();
        loop {
            if let Some((future, ..)) = futures[current_pipeline].as_ref() {
                future.wait(None)?;
            }
            let infos = [
                CalibratedTimestampInfoEXT::builder().time_domain(TimeDomainEXT::CLOCK_MONOTONIC_RAW).build(),
                CalibratedTimestampInfoEXT::builder().time_domain(TimeDomainEXT::DEVICE).build(),
            ];
            let mut calib_ts = [0u64; 2];
            let mut deviation = [0u64];
            unsafe {
                device.fns().ext_calibrated_timestamps.get_calibrated_timestamps_ext(device.internal_object(), 2, infos.as_ptr(), calib_ts.as_mut_ptr(), deviation.as_mut_ptr()).result()?;
            }
            let wait_target = if app.is_present("lfx") { lfx.get_wait_target(frames) } else { 0 };
            let sleep_target = wait_target.max(calib_ts[0]);
            loop {
                let mut ts = [0u64];
                unsafe {
                    device.fns().ext_calibrated_timestamps.get_calibrated_timestamps_ext(device.internal_object(), 1, infos.as_ptr(), ts.as_mut_ptr(), deviation.as_mut_ptr()).result()?;
                }
                let mut timestamps = vec![];
                for i in 0..num_pipelines {
                    if let Some((future, frame, start_nonce, job)) = futures[i].as_mut() {
                        if future.get_fence().map(|x| x.ready().unwrap()).unwrap_or(true) {
                            future.wait(None)?;
                            let mut result = [0u64];
                            pipeline.search[i].query.queries_range(0..1).unwrap().get_results(&mut result, QueryResultFlags {
                                wait: true,
                                ..Default::default()
                            })?;
                            timestamps.push((*frame, result[0]));
                            let output_buf = pipeline.search[i].output_buf.read()?;
                            for i in 0..output_buf.output_count {
                                client.submit(&job, *start_nonce + output_buf.outputs[i as usize].gid as u64);
                            }
                            futures[i] = None;
                        }
                    }
                }
                timestamps.sort_unstable();
                if !timestamps.is_empty() {
                    for (f, t) in timestamps {
                        let t = (calib_ts[0] as i64 - ((calib_ts[1] as i64 - t as i64) as f32 * period) as i64) as u64;
                        lfx.end_frame(f, t);
                    }
                }
                let dur = Duration::from_nanos(sleep_target.saturating_sub(ts[0]));
                let wait: Vec<_> = futures.iter_mut().filter_map(|x| x.as_mut().and_then(|x| x.0.get_fence())).map(|x| &*x).collect();
                if !wait.is_empty() {
                    match Fence::multi_wait(wait, Some(dur), false) {
                        Ok(..) => {}
                        Err(FenceWaitError::Timeout) => { break; }
                        Err(x) => panic!("error waiting for fence: {}", x),
                    }
                } else {
                    std::thread::sleep(dur);
                    break;
                }
            }
            let job = client.current_job();
            if stop.load(Ordering::SeqCst) || job.epoch != pipeline.epoch
            {
                cleanup_counter += 1;
                if cleanup_counter != num_pipelines {
                    current_pipeline = (current_pipeline + 1) % num_pipelines;
                    continue;
                } else {
                    break;
                }
            }
            lfx.begin_frame(frames, wait_target, sleep_target);
            let nonce_mask = (!0u64) >> (job.pool_nonce_bytes * 8);
            if ((nonce + hash_quantum) & nonce_mask) < hash_quantum { // Wraparound
                nonce = 0;
            }

            pipeline.config.start_nonce = job.extra_nonce | (nonce & nonce_mask);
            pipeline.search[current_pipeline].output_buf.write()?.output_count = 0;
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
            )?;
            builder
                .bind_pipeline_compute(pipeline.search[current_pipeline].pipeline.clone())
                .bind_descriptor_sets(
                    PipelineBindPoint::Compute,
                    pipeline.search[current_pipeline].pipeline.layout().clone(),
                    0,
                    pipeline.search[current_pipeline].set.clone(),
                )
                .push_constants(pipeline.search[current_pipeline].pipeline.layout().clone(), 0, dag_cs::ty::PushConstants { conf: pipeline.config });
            builder.dispatch([wg_count as _, 1, 1])?;
            let command_buffer = builder.build()?;

            let future = sync::now(device.clone())
                .then_execute(queues[0].clone(), command_buffer)?
                .then_signal_semaphore_and_flush()?;

            let mut builder = AutoCommandBufferBuilder::primary(
                device.clone(),
                queue_family,
                CommandBufferUsage::OneTimeSubmit,
            )?;
            unsafe { builder.reset_query_pool(pipeline.search[current_pipeline].query.clone(), 0..1)?; }
            unsafe { builder.write_timestamp(pipeline.search[current_pipeline].query.clone(), 0, PipelineStage::AllCommands)?; }
            unsafe { builder.host_read_barrier(&*pipeline.search[current_pipeline].output_buf); }
            let command_buffer = builder.build()?;

            let future = future
                .then_execute(queues[1].clone(), command_buffer)?
                .then_signal_fence_and_flush()?;

            assert!(futures[current_pipeline].is_none());
            futures[current_pipeline] = Some((future, frames, pipeline.config.start_nonce, job.clone()));

            nonce += hash_quantum;
            hash_counter += hash_quantum;
            if start_time.elapsed() > Duration::from_secs(2) {
                let status = format!("{:.02} MH/s {:.02?}", hash_counter as f64 / start_time.elapsed().as_secs_f64() / 1000. / 1000., lfx.latency());
                progress.set_message(status);
                start_time = Instant::now();
                hash_counter = 0;
            }
            current_pipeline = (current_pipeline + 1) % num_pipelines;
            frames += 1;
        }
    }
    Ok(())
}
