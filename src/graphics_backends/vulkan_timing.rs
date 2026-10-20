// Copyright 2026, Nick Kennedy
// SPDX-License-Identifier: GPL-3.0-or-later
//! Application-queue timestamps. No query WAIT flag, fence wait or queue idle
//! on the frame path. The owner must idle the device before dropping this data.
use super::ApplicationGpuTiming;
use ash::vk;
use std::{
    collections::VecDeque,
    fs::File,
    io::{BufWriter, Write},
};

const SLOTS: usize = 16;
const HISTORY: usize = 120;
struct Slot {
    fence: vk::Fence,
    stamp: Option<(u32, f64, f32)>,
    submitted: bool,
    discard: bool,
}

pub(super) struct VulkanGpuTiming {
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    queries: vk::QueryPool,
    buffers: Vec<vk::CommandBuffer>,
    slots: Vec<Slot>,
    active: Option<usize>,
    period: f32,
    bits: u32,
    previous_start: Option<f64>,
    history: VecDeque<ApplicationGpuTiming>,
    failed: bool,
    trace: Option<BufWriter<File>>,
    samples: u64,
}

fn milliseconds(start: u64, end: u64, bits: u32, period: f32) -> Option<f32> {
    if !(1..=64).contains(&bits) || !period.is_finite() || period <= 0.0 {
        return None;
    }
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    let ms = ((end.wrapping_sub(start) & mask) as f64 * period as f64 / 1_000_000.0) as f32;
    ms.is_finite().then_some(ms)
}

impl VulkanGpuTiming {
    pub(super) unsafe fn new(
        instance: &ash::Instance,
        physical: vk::PhysicalDevice,
        device: &ash::Device,
        queue: vk::Queue,
        family: u32,
    ) -> Result<Self, String> {
        let properties = unsafe { instance.get_physical_device_properties(physical) };
        let families = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let bits = families
            .get(family as usize)
            .ok_or("invalid timestamp queue family")?
            .timestamp_valid_bits;
        let period = properties.limits.timestamp_period;
        milliseconds(0, 1, bits, period).ok_or("unsupported timestamp bits/period")?;
        let mut data = Self {
            device: device.clone(),
            queue,
            pool: vk::CommandPool::null(),
            queries: vk::QueryPool::null(),
            buffers: Vec::new(),
            slots: Vec::new(),
            active: None,
            period,
            bits,
            previous_start: None,
            history: VecDeque::new(),
            failed: false,
            trace: None,
            samples: 0,
        };
        unsafe {
            data.pool = device
                .create_command_pool(
                    &vk::CommandPoolCreateInfo::default().queue_family_index(family),
                    None,
                )
                .map_err(|e| e.to_string())?;
            data.queries = device
                .create_query_pool(
                    &vk::QueryPoolCreateInfo::default()
                        .query_type(vk::QueryType::TIMESTAMP)
                        .query_count((2 * SLOTS) as u32),
                    None,
                )
                .map_err(|e| e.to_string())?;
            data.buffers = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(data.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count((2 * SLOTS) as u32),
                )
                .map_err(|e| e.to_string())?;
            for i in 0..SLOTS {
                let fence = device
                    .create_fence(&vk::FenceCreateInfo::default(), None)
                    .map_err(|e| e.to_string())?;
                data.slots.push(Slot {
                    fence,
                    stamp: None,
                    submitted: false,
                    discard: false,
                });
                for end in [false, true] {
                    let q = (2 * i + usize::from(end)) as u32;
                    let buffer = data.buffers[q as usize];
                    device
                        .begin_command_buffer(buffer, &vk::CommandBufferBeginInfo::default())
                        .map_err(|e| e.to_string())?;
                    if !end {
                        device.cmd_reset_query_pool(buffer, data.queries, q, 2);
                    }
                    // BOTTOM waits for earlier queue work at both boundaries, so
                    // a start marker does not include a previous frame's tail.
                    device.cmd_write_timestamp(
                        buffer,
                        vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                        data.queries,
                        q,
                    );
                    device
                        .end_command_buffer(buffer)
                        .map_err(|e| e.to_string())?;
                }
            }
        }
        if let Some(path) = std::env::var_os("XRIZER_GPU_TIMING_TRACE").filter(|p| !p.is_empty()) {
            match File::options().write(true).create_new(true).open(path) {
                Ok(file) => {
                    let mut writer = BufWriter::new(file);
                    writeln!(writer, "frame,system_seconds,client_interval_ms,pre_submit_gpu_ms,start_ticks,end_ticks,timestamp_period_ns,valid_bits")
                        .map_err(|e| e.to_string())?;
                    data.trace = Some(writer);
                }
                Err(e) => log::warn!("Cannot create GPU timing trace: {e}"),
            }
        }
        log::info!(
            "Vulkan application GPU timing enabled: period_ns={period}, valid_bits={bits}, slots={SLOTS}; nonblocking results"
        );
        Ok(data)
    }

    fn fail(&mut self, error: vk::Result) {
        if !self.failed {
            log::warn!("Vulkan GPU timing disabled after {error:?}; no fabricated timing fallback");
        }
        self.failed = true;
    }

    fn collect(&mut self) {
        if self.failed {
            return;
        }
        for i in 0..self.slots.len() {
            if !self.slots[i].submitted {
                continue;
            }
            match unsafe { self.device.get_fence_status(self.slots[i].fence) } {
                Ok(false) => continue,
                Err(e) => {
                    self.fail(e);
                    return;
                }
                Ok(true) => {}
            }
            let mut results = [[0u64; 2]; 2];
            match unsafe {
                self.device.get_query_pool_results(
                    self.queries,
                    (2 * i) as u32,
                    &mut results,
                    vk::QueryResultFlags::TYPE_64 | vk::QueryResultFlags::WITH_AVAILABILITY,
                )
            } {
                Err(vk::Result::NOT_READY) => continue,
                Err(e) => {
                    self.fail(e);
                    return;
                }
                Ok(()) => {}
            }
            if results.iter().any(|q| q[1] == 0) {
                continue;
            }
            let stamp = self.slots[i].stamp.take();
            self.slots[i].submitted = false;
            if self.slots[i].discard {
                continue;
            }
            let Some((frame, system_seconds, client_interval_ms)) = stamp else {
                continue;
            };
            let Some(pre_submit_gpu_ms) =
                milliseconds(results[0][0], results[1][0], self.bits, self.period)
            else {
                continue;
            };
            let sample = ApplicationGpuTiming {
                frame,
                system_seconds,
                client_interval_ms,
                pre_submit_gpu_ms,
            };
            // Slots complete in queue order, but the scan can cross the ring's wrap.
            let at = self
                .history
                .iter()
                .position(|s| s.system_seconds > system_seconds)
                .unwrap_or(self.history.len());
            self.history.insert(at, sample);
            while self.history.len() > HISTORY {
                self.history.pop_front();
            }
            if self.samples.is_multiple_of(120) {
                log::info!(
                    "Vulkan application GPU sample: frame={frame}, pre_submit_ms={pre_submit_gpu_ms:.3}, client_interval_ms={client_interval_ms:.3}"
                );
            }
            self.samples += 1;
            if let Some(writer) = &mut self.trace {
                let _ = writeln!(
                    writer,
                    "{frame},{system_seconds:.9},{client_interval_ms:.6},{pre_submit_gpu_ms:.6},{},{},{},{}",
                    results[0][0], results[1][0], self.period, self.bits
                );
                // Flush only on the optional profiling path, after GPU results are collected.
                if self.samples.is_multiple_of(60) {
                    let _ = writer.flush();
                }
            }
        }
    }

    pub(super) fn begin(&mut self, frame: u32, seconds: f64) {
        if self.failed {
            return;
        }
        // Duplicate explicit-timing calls must not replace the frame's start.
        if self
            .active
            .is_some_and(|i| self.slots[i].stamp.is_some_and(|s| s.0 == frame))
        {
            return;
        }
        if let Some(i) = self.active {
            self.slots[i].discard = true;
            self.end();
        }
        self.collect();
        if self.failed {
            return;
        }
        let interval = self
            .previous_start
            .map_or(0.0, |last| ((seconds - last) * 1000.0) as f32);
        self.previous_start = Some(seconds);
        let Some(i) = self
            .slots
            .iter()
            .position(|s| s.stamp.is_none() && !s.submitted)
        else {
            return; // GPU behind: bounded storage; skip a sample, never wait or overwrite.
        };
        let commands = [self.buffers[2 * i]];
        match unsafe {
            self.device
                .reset_fences(&[self.slots[i].fence])
                .and_then(|_| {
                    self.device.queue_submit(
                        self.queue,
                        &[vk::SubmitInfo::default().command_buffers(&commands)],
                        vk::Fence::null(),
                    )
                })
        } {
            Ok(()) => {
                self.slots[i].stamp = Some((frame, seconds, interval));
                self.slots[i].discard = false;
                self.active = Some(i);
            }
            Err(e) => self.fail(e),
        }
    }

    pub(super) fn end(&mut self) {
        if self.failed {
            return;
        }
        let Some(i) = self.active.take() else {
            return;
        };
        let commands = [self.buffers[2 * i + 1]];
        match unsafe {
            self.device.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&commands)],
                self.slots[i].fence,
            )
        } {
            Ok(()) => self.slots[i].submitted = true,
            Err(e) => self.fail(e),
        }
    }

    pub(super) fn sample(&mut self, frames_ago: u32) -> Option<ApplicationGpuTiming> {
        self.collect();
        if self.failed {
            return None;
        }
        // Match Wine FrameHistory: offsets count completed valid records and
        // requests beyond retained history clamp to its oldest valid record.
        self.history
            .iter()
            .rev()
            .nth(frames_ago as usize)
            .or_else(|| self.history.front())
            .copied()
    }
}

impl Drop for VulkanGpuTiming {
    fn drop(&mut self) {
        // VulkanData idles its device before dropping us, including an unfinished start.
        unsafe {
            for slot in &self.slots {
                self.device.destroy_fence(slot.fence, None);
            }
            if self.pool != vk::CommandPool::null() {
                self.device.destroy_command_pool(self.pool, None);
            }
            if self.queries != vk::QueryPool::null() {
                self.device.destroy_query_pool(self.queries, None);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn timestamp_conversion_handles_valid_bits_and_wrap() {
        assert_eq!(milliseconds(250, 4, 8, 1_000_000.0), Some(10.0));
        assert_eq!(milliseconds(u64::MAX - 2, 2, 64, 1_000_000.0), Some(5.0));
        assert_eq!(milliseconds(0, 1000, 64, 1.0), Some(0.001));
        for (bits, period) in [
            (0, 1.0),
            (65, 1.0),
            (8, 0.0),
            (8, f32::NAN),
            (8, f32::INFINITY),
        ] {
            assert_eq!(milliseconds(0, 1, bits, period), None);
        }
    }
    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "requires explicit native Vulkan loader/ICD and Metal access"]
    fn real_gpu_timestamp_ring() {
        unsafe {
            let path = std::env::var("XRIZER_VULKAN_LIBRARY").expect("select a real Vulkan loader");
            let entry = ash::Entry::load_from(path).unwrap();
            let names = [ash::khr::portability_enumeration::NAME.as_ptr()];
            let instance = entry
                .create_instance(
                    &vk::InstanceCreateInfo::default()
                        .application_info(
                            &vk::ApplicationInfo::default().api_version(vk::API_VERSION_1_2),
                        )
                        .flags(vk::InstanceCreateFlags::ENUMERATE_PORTABILITY_KHR)
                        .enabled_extension_names(&names),
                    None,
                )
                .unwrap();
            let physical = instance.enumerate_physical_devices().unwrap()[0];
            let family = instance
                .get_physical_device_queue_family_properties(physical)
                .iter()
                .position(|p| p.queue_flags.contains(vk::QueueFlags::GRAPHICS))
                .unwrap() as u32;
            let available = instance
                .enumerate_device_extension_properties(physical)
                .unwrap();
            let mut extensions = Vec::new();
            if available.iter().any(|p| {
                std::ffi::CStr::from_ptr(p.extension_name.as_ptr())
                    == ash::khr::portability_subset::NAME
            }) {
                extensions.push(ash::khr::portability_subset::NAME.as_ptr());
            }
            let device = instance
                .create_device(
                    physical,
                    &vk::DeviceCreateInfo::default()
                        .queue_create_infos(&[vk::DeviceQueueCreateInfo::default()
                            .queue_family_index(family)
                            .queue_priorities(&[1.0])])
                        .enabled_extension_names(&extensions),
                    None,
                )
                .unwrap();
            let queue = device.get_device_queue(family, 0);
            let mut timing =
                VulkanGpuTiming::new(&instance, physical, &device, queue, family).unwrap();
            assert!(timing.sample(0).is_none());
            let origin = std::time::Instant::now();
            timing.begin(0, origin.elapsed().as_secs_f64());
            assert!(timing.sample(0).is_none()); // A begin marker alone is not a sample.
            timing.end();
            device.device_wait_idle().unwrap(); // Test-only collection boundary.
            let first = timing.sample(0).unwrap();
            assert_eq!(first.frame, 0);
            assert!(first.pre_submit_gpu_ms.is_finite() && first.pre_submit_gpu_ms >= 0.0);
            for frame in 1..200 {
                timing.begin(frame, origin.elapsed().as_secs_f64());
                timing.end();
                let _ = timing.sample(0);
            }
            device.device_wait_idle().unwrap();
            let _ = timing.sample(0);
            // Skip an incomplete frame and exercise duplicate explicit starts.
            timing.begin(200, origin.elapsed().as_secs_f64());
            assert!(timing.active.is_some());
            timing.begin(200, origin.elapsed().as_secs_f64());
            timing.begin(201, origin.elapsed().as_secs_f64());
            timing.end();
            device.device_wait_idle().unwrap();
            let latest = timing.sample(0).unwrap();
            assert_eq!(latest.frame, 201);
            assert!(timing.history.len() <= HISTORY && timing.slots.len() == SLOTS);
            assert!(
                timing
                    .history
                    .iter()
                    .all(|s| s.frame != 200 && s.pre_submit_gpu_ms.is_finite())
            );
            assert!(
                timing
                    .history
                    .iter()
                    .zip(timing.history.iter().skip(1))
                    .all(|(a, b)| a.frame < b.frame)
            );
            assert_eq!(
                timing.sample(u32::MAX).unwrap().frame,
                timing.history.front().unwrap().frame
            );
            eprintln!(
                "REAL GPU PASS: samples={}, retained={}, latest_frame={}, latest_ms={}, period_ns={}, valid_bits={}",
                timing.samples,
                timing.history.len(),
                latest.frame,
                latest.pre_submit_gpu_ms,
                timing.period,
                timing.bits
            );
            // A real transfer workload must produce a larger interval than an
            // empty pair. Build commands before the start to exclude recording.
            let buffer = device
                .create_buffer(
                    &vk::BufferCreateInfo::default()
                        .size(8 * 1024 * 1024)
                        .usage(vk::BufferUsageFlags::TRANSFER_DST),
                    None,
                )
                .unwrap();
            let requirements = device.get_buffer_memory_requirements(buffer);
            let memory_type = requirements.memory_type_bits.trailing_zeros();
            let memory = device
                .allocate_memory(
                    &vk::MemoryAllocateInfo::default()
                        .allocation_size(requirements.size)
                        .memory_type_index(memory_type),
                    None,
                )
                .unwrap();
            device.bind_buffer_memory(buffer, memory, 0).unwrap();
            let commands = device
                .allocate_command_buffers(
                    &vk::CommandBufferAllocateInfo::default()
                        .command_pool(timing.pool)
                        .level(vk::CommandBufferLevel::PRIMARY)
                        .command_buffer_count(1),
                )
                .unwrap();
            device
                .begin_command_buffer(commands[0], &vk::CommandBufferBeginInfo::default())
                .unwrap();
            for value in 0..128 {
                device.cmd_fill_buffer(commands[0], buffer, 0, vk::WHOLE_SIZE, value);
                device.cmd_pipeline_barrier(
                    commands[0],
                    vk::PipelineStageFlags::TRANSFER,
                    vk::PipelineStageFlags::TRANSFER,
                    vk::DependencyFlags::empty(),
                    &[vk::MemoryBarrier::default()
                        .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                        .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)],
                    &[],
                    &[],
                );
            }
            device.end_command_buffer(commands[0]).unwrap();
            timing.begin(300, origin.elapsed().as_secs_f64());
            device
                .queue_submit(
                    queue,
                    &[vk::SubmitInfo::default().command_buffers(&commands)],
                    vk::Fence::null(),
                )
                .unwrap();
            timing.end();
            device.device_wait_idle().unwrap();
            let busy = timing.sample(0).unwrap();
            assert_eq!(busy.frame, 300);
            assert!(busy.pre_submit_gpu_ms > first.pre_submit_gpu_ms);
            eprintln!(
                "REAL WORKLOAD PASS: empty_ms={}, transfer_ms={}",
                first.pre_submit_gpu_ms, busy.pre_submit_gpu_ms
            );
            device.destroy_buffer(buffer, None);
            device.free_memory(memory, None);
            drop(timing);
            device.destroy_device(None);
            instance.destroy_instance(None);
        }
    }
}
