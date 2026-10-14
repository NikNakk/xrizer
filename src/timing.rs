use crate::{graphics_backends::GpuTiming, vr};
use std::collections::{HashSet, VecDeque};
use std::time::Instant;

#[derive(Default)]
pub struct FrameHistory {
    frames: VecDeque<vr::Compositor_FrameTiming>,
    current: Option<vr::Compositor_FrameTiming>,
    started: Option<Instant>,
    previous_start: Option<Instant>,
    gpu_ready: HashSet<u32>,
    pub requires_gpu: bool,
}
impl FrameHistory {
    pub fn begin(
        &mut self,
        index: u32,
        called: Instant,
        ready: Instant,
        epoch: Instant,
        pose: vr::TrackedDevicePose_t,
    ) {
        let mut frame = vr::Compositor_FrameTiming::default();
        frame.m_nSize = std::mem::size_of_val(&frame) as u32;
        frame.m_nFrameIndex = index;
        frame.m_flSystemTimeInSeconds = called.duration_since(epoch).as_secs_f64();
        frame.m_flClientFrameIntervalMs = self.previous_start.map_or(0.0, |last| {
            called.duration_since(last).as_secs_f32() * 1000.0
        });
        frame.m_flNewPosesReadyMs = ready.duration_since(called).as_secs_f32() * 1000.0;
        frame.m_HmdPose = pose;
        self.current = Some(frame);
        self.started = Some(called);
        self.previous_start = Some(called);
    }
    pub fn submit(&mut self, elapsed_ms: f32, stereo_complete: bool) {
        if let Some(frame) = self.current.as_mut() {
            frame.m_flSubmitFrameMs += elapsed_ms;
            if stereo_complete {
                frame.m_flNewFrameReadyMs = self.started.unwrap().elapsed().as_secs_f32() * 1000.0;
            }
        }
    }
    pub fn complete(&mut self, compositor_ms: f32, gpu: Vec<GpuTiming>) {
        if let Some(mut frame) = self.current.take() {
            frame.m_flCompositorRenderCpuMs = compositor_ms;
            self.frames.push_back(frame);
            if self.frames.len() > 64
                && let Some(old) = self.frames.pop_front()
            {
                self.gpu_ready.remove(&old.m_nFrameIndex);
            }
        }
        for sample in gpu {
            if sample.milliseconds.is_finite()
                && sample.milliseconds >= 0.0
                && let Some(frame) = self
                    .frames
                    .iter_mut()
                    .find(|frame| frame.m_nFrameIndex == sample.frame)
            {
                self.gpu_ready.insert(sample.frame);
                frame.m_flPreSubmitGpuMs = sample.milliseconds as f32;
                frame.m_flTotalRenderGpuMs = sample.milliseconds as f32;
            }
        }
    }
    pub fn get(&self, ago: u32) -> Option<vr::Compositor_FrameTiming> {
        let mut valid =
            self.frames.iter().rev().filter(|frame| {
                !self.requires_gpu || self.gpu_ready.contains(&frame.m_nFrameIndex)
            });
        valid
            .clone()
            .nth(ago as usize)
            .or_else(|| valid.next_back())
            .copied()
    }
    pub fn latest(&self, count: u32) -> Vec<vr::Compositor_FrameTiming> {
        let valid: Vec<_> = self
            .frames
            .iter()
            .filter(|frame| !self.requires_gpu || self.gpu_ready.contains(&frame.m_nFrameIndex))
            .copied()
            .collect();
        valid[valid.len().saturating_sub(count as usize)..].to_vec()
    }
}

// The caller may use an older, shorter ABI layout. Copy only the known prefix.
pub unsafe fn write_record(
    out: *mut vr::Compositor_FrameTiming,
    size: usize,
    frame: &vr::Compositor_FrameTiming,
) {
    unsafe {
        std::ptr::copy_nonoverlapping(
            (frame as *const vr::Compositor_FrameTiming).cast::<u8>(),
            out.cast::<u8>(),
            size.min(std::mem::size_of_val(frame)),
        );
        (&raw mut (*out).m_nSize).write_unaligned(size as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn delayed_gpu_results_patch_the_matching_history_frame() {
        let mut history = FrameHistory::default();
        let epoch = Instant::now();
        for index in 0..70 {
            let called = Instant::now();
            history.begin(
                index,
                called,
                Instant::now(),
                epoch,
                vr::TrackedDevicePose_t::default(),
            );
            history.complete(0.0, Vec::new());
        }
        history.complete(
            0.0,
            vec![GpuTiming {
                frame: 68,
                milliseconds: 13.25,
            }],
        );
        assert_eq!(history.latest(100).len(), 64);
        assert_eq!(history.get(0).unwrap().m_nFrameIndex, 69);
        assert_eq!(history.get(0).unwrap().m_flTotalRenderGpuMs, 0.0);
        assert_eq!(history.get(1).unwrap().m_flTotalRenderGpuMs, 13.25);
        assert_eq!(history.get(100).unwrap().m_nFrameIndex, 6);
        history.requires_gpu = true;
        assert_eq!(history.latest(64).len(), 1);
        assert_eq!(history.get(0).unwrap().m_nFrameIndex, 68);
        history.requires_gpu = false;
        assert!(
            history
                .latest(64)
                .windows(2)
                .all(|pair| pair[0].m_nFrameIndex + 1 == pair[1].m_nFrameIndex)
        );
    }
}
