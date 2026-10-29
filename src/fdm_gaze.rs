//! Gaze for gaze-centred fragment density maps.
//!
//! With `XRIZER_FDM_GAZE=1`, xrizer reads the user's gaze through
//! `XR_EXT_eye_gaze_interaction`, projects it into each eye's field of view and
//! hands it to the KosmicKrisp Vulkan driver in the same process
//! (`kk_fdm_set_gaze`), which centres its fragment density maps on it.
//! `XRIZER_FDM_GAZE_MODE=application` instead exposes projection-consistent NDC
//! through OpenVR's foveation-centre helpers and disables private driver publication.
//! Gaze is approximated as a head-space ray at infinity: combined OpenXR gaze
//! does not supply independent per-eye fixation or convergence distances.
//! `XRIZER_FAKE_GAZE=yaw,pitch` (degrees, right and up positive) replaces the
//! eye tracker with a fixed direction, for testing without one;
//! `XRIZER_FAKE_GAZE=sweep:yaw,pitch,period_s` moves it continuously.

use glam::{Quat, Vec3};
use log::{info, warn};
use openxr as xr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Whether gaze-centred density maps were requested.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        application_enabled()
            || std::env::var("XRIZER_FDM_GAZE").is_ok_and(|v| v != "0")
            || fake_direction().is_some()
    })
}

/// A fake gaze direction in view space from `XRIZER_FAKE_GAZE`: either fixed
/// `yaw,pitch`, or `sweep:yaw_amplitude,pitch_amplitude,period_seconds`, which
/// moves the gaze along a Lissajous path (yaw at the period, pitch at 0.7x).
pub fn fake_direction() -> Option<Vec3> {
    #[derive(Clone, Copy)]
    enum Fake {
        Fixed(f32, f32),
        Sweep(f32, f32, f32, Instant),
    }
    static FAKE: OnceLock<Option<Fake>> = OnceLock::new();
    let fake = (*FAKE.get_or_init(|| {
        let value = std::env::var("XRIZER_FAKE_GAZE").ok()?;
        let (sweep, rest) = match value.strip_prefix("sweep:") {
            Some(rest) => (true, rest.to_owned()),
            None => (false, value),
        };
        let parts: Vec<f32> = rest
            .split(',')
            .map(|p| p.trim().parse::<f32>())
            .collect::<Result<_, _>>()
            .ok()?;
        if parts.iter().any(|v| !v.is_finite()) {
            return None;
        }
        match (sweep, parts.as_slice()) {
            (false, [yaw, pitch]) => Some(Fake::Fixed(yaw.to_radians(), pitch.to_radians())),
            (true, [yaw, pitch, period]) if *period > 0.0 => Some(Fake::Sweep(
                yaw.to_radians(),
                pitch.to_radians(),
                *period,
                Instant::now(),
            )),
            _ => None,
        }
    }))?;
    let (yaw, pitch) = match fake {
        Fake::Fixed(yaw, pitch) => (yaw, pitch),
        Fake::Sweep(yaw, pitch, period, start) => {
            let phase = start.elapsed().as_secs_f32() / period * std::f32::consts::TAU;
            (yaw * phase.sin(), pitch * (0.7 * phase).sin())
        }
    };
    Some(Quat::from_euler(glam::EulerRot::YXZ, -yaw, pitch, 0.0) * Vec3::NEG_Z)
}

/// Direction the gaze pose looks along.
pub fn pose_direction(pose: &xr::Posef) -> Vec3 {
    let o = pose.orientation;
    Quat::from_xyzw(o.x, o.y, o.z, o.w) * Vec3::NEG_Z
}

/// Where a view-space direction lands across an eye's field of view, as
/// (0, 0) at the top left and (1, 1) at the bottom right of the eye image.
pub fn project(direction: Vec3, view: &xr::View) -> Option<(f32, f32)> {
    let o = view.pose.orientation;
    let eye = Quat::from_xyzw(o.x, o.y, o.z, o.w);
    let d = eye.inverse() * direction;
    if d.z > -1e-3 {
        return None;
    }
    let tx = d.x / -d.z;
    let ty = d.y / -d.z;
    let left = view.fov.angle_left.tan();
    let right = view.fov.angle_right.tan();
    let up = view.fov.angle_up.tan();
    let down = view.fov.angle_down.tan();
    if right <= left || up <= down {
        return None;
    }
    Some((
        ((tx - left) / (right - left)).clamp(0.0, 1.0),
        ((up - ty) / (up - down)).clamp(0.0, 1.0),
    ))
}

type SetGaze = unsafe extern "C" fn(u32, f32, f32, bool);

/// The driver's `kk_fdm_set_gaze`, found among the loaded images. The Vulkan
/// loader opens the driver privately, so it is looked up by image name.
fn kk_set_gaze() -> Option<SetGaze> {
    static FUNCTION: AtomicUsize = AtomicUsize::new(0);
    static ATTEMPTS: AtomicU32 = AtomicU32::new(0);
    let f = FUNCTION.load(Ordering::Relaxed);
    if f != 0 {
        return Some(unsafe { std::mem::transmute::<usize, SetGaze>(f) });
    }
    // Retry now and then: the driver may load after the first frames.
    if ATTEMPTS.fetch_add(1, Ordering::Relaxed) % 90 != 0 {
        return None;
    }
    let found = find_kk_set_gaze();
    if let Some(f) = found {
        FUNCTION.store(f as usize, Ordering::Relaxed);
        info!("FDM gaze: found kk_fdm_set_gaze in the KosmicKrisp driver");
    }
    found
}

#[cfg(target_os = "macos")]
fn find_kk_set_gaze() -> Option<SetGaze> {
    use std::ffi::{CStr, c_char};
    unsafe extern "C" {
        fn _dyld_image_count() -> u32;
        fn _dyld_get_image_name(index: u32) -> *const c_char;
    }
    unsafe {
        for i in 0.._dyld_image_count() {
            let name = _dyld_get_image_name(i);
            if name.is_null() {
                continue;
            }
            let name = CStr::from_ptr(name);
            if !name.to_bytes().ends_with(b"libvulkan_kosmickrisp.dylib") {
                continue;
            }
            let handle = libc::dlopen(name.as_ptr(), libc::RTLD_NOLOAD | libc::RTLD_LAZY);
            if handle.is_null() {
                continue;
            }
            let symbol = libc::dlsym(handle, c"kk_fdm_set_gaze".as_ptr());
            if !symbol.is_null() {
                return Some(std::mem::transmute::<*mut libc::c_void, SetGaze>(symbol));
            }
        }
    }
    None
}

#[cfg(not(target_os = "macos"))]
fn find_kk_set_gaze() -> Option<SetGaze> {
    None
}

/// Hands the gaze point of each view to the driver; `None` turns it off.
pub fn publish(points: &[Option<(f32, f32)>]) {
    if application_enabled() {
        return;
    }
    let Some(set_gaze) = kk_set_gaze() else {
        return;
    };
    for (view, point) in points.iter().enumerate() {
        let (x, y) = point.unwrap_or((0.5, 0.5));
        unsafe { set_gaze(view as u32, x, y, point.is_some()) };
    }

    static FRAMES: AtomicU32 = AtomicU32::new(0);
    let frame = FRAMES.fetch_add(1, Ordering::Relaxed);
    if std::env::var_os("XRIZER_FDM_GAZE_LOG").is_some() && frame % 90 == 0 {
        info!("FDM gaze: {points:?}");
    }
}

/// Logs once that the runtime does not offer eye gaze.
pub fn warn_unsupported() {
    static WARNED: OnceLock<()> = OnceLock::new();
    WARNED.get_or_init(|| {
        warn!("FDM gaze requested, but the runtime does not support XR_EXT_eye_gaze_interaction")
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(left: f32, right: f32, up: f32, down: f32) -> xr::View {
        xr::View {
            pose: xr::Posef::IDENTITY,
            fov: xr::Fovf {
                angle_left: left.to_radians(),
                angle_right: right.to_radians(),
                angle_up: up.to_radians(),
                angle_down: down.to_radians(),
            },
        }
    }

    #[test]
    fn straight_ahead_in_symmetric_view_is_centre() {
        let (x, y) = project(Vec3::NEG_Z, &view(-45.0, 45.0, 45.0, -45.0)).unwrap();
        assert!((x - 0.5).abs() < 1e-5 && (y - 0.5).abs() < 1e-5);
    }

    #[test]
    fn straight_ahead_in_asymmetric_view_is_off_centre() {
        // PS VR2 left eye: wider to the left than to the right.
        let (x, _) = project(Vec3::NEG_Z, &view(-61.5, 43.5, 53.0, -53.0)).unwrap();
        let expected =
            61.5f32.to_radians().tan() / (61.5f32.to_radians().tan() + 43.5f32.to_radians().tan());
        assert!((x - expected).abs() < 1e-5);
    }

    #[test]
    fn looking_right_and_up_moves_right_and_up() {
        let d = Quat::from_euler(
            glam::EulerRot::YXZ,
            -20f32.to_radians(),
            10f32.to_radians(),
            0.0,
        ) * Vec3::NEG_Z;
        let (x, y) = project(d, &view(-45.0, 45.0, 45.0, -45.0)).unwrap();
        assert!(x > 0.5 && y < 0.5);
    }

    #[test]
    fn looking_backwards_has_no_point() {
        assert!(project(Vec3::Z, &view(-45.0, 45.0, 45.0, -45.0)).is_none());
    }
}

pub fn application_enabled() -> bool {
    static APPLICATION: OnceLock<bool> = OnceLock::new();
    *APPLICATION
        .get_or_init(|| std::env::var("XRIZER_FDM_GAZE_MODE").as_deref() == Ok("application"))
}

#[derive(Clone, Copy)]
pub struct Sample {
    pub direction: Vec3,
    pub fovs: [xr::Fovf; 2],
    pub display_time: xr::Time,
    pub acquired: Instant,
}

impl Sample {
    pub fn current(&self, display_time: xr::Time) -> bool {
        self.display_time == display_time && self.acquired.elapsed() <= Duration::from_millis(100)
    }

    pub fn centres(&self) -> Option<[[f32; 2]; 2]> {
        Some([
            project_ndc(self.direction, self.fovs[0])?,
            project_ndc(self.direction, self.fovs[1])?,
        ])
    }
}

pub fn project_ndc(direction: Vec3, fov: xr::Fovf) -> Option<[f32; 2]> {
    let left = fov.angle_left.tan();
    let right = fov.angle_right.tan();
    let down = fov.angle_down.tan();
    let up = fov.angle_up.tan();
    if !direction.is_finite()
        || direction.z >= -1e-6
        || ![left, right, down, up].iter().all(|v| v.is_finite())
        || right <= left
        || up <= down
    {
        return None;
    }
    let point = [
        2.0 * (direction.x / -direction.z - left) / (right - left) - 1.0,
        2.0 * (direction.y / -direction.z - down) / (up - down) - 1.0,
    ];
    point.iter().all(|v| v.is_finite()).then_some(point)
}

pub fn project_matrix(direction: Vec3, matrix: &[[f32; 4]; 4]) -> Option<[f32; 2]> {
    if !direction.is_finite()
        || direction.z >= -1e-6
        || !matrix.iter().flatten().all(|v| v.is_finite())
    {
        return None;
    }
    let clip = matrix.map(|r| r[0] * direction.x + r[1] * direction.y + r[2] * direction.z);
    if clip[3] <= 1e-6 {
        return None;
    }
    let point = [clip[0] / clip[3], clip[1] / clip[3]];
    point.iter().all(|v| v.is_finite()).then_some(point)
}

#[cfg(test)]
mod application_tests {
    use super::*;

    fn fov(left: f32, right: f32, down: f32, up: f32) -> xr::Fovf {
        xr::Fovf {
            angle_left: left.to_radians(),
            angle_right: right.to_radians(),
            angle_down: down.to_radians(),
            angle_up: up.to_radians(),
        }
    }

    #[test]
    fn ndc_signs_and_no_image_space_clamp() {
        let symmetric = fov(-45.0, 45.0, -45.0, 45.0);
        assert_eq!(project_ndc(Vec3::NEG_Z, symmetric), Some([0.0, 0.0]));
        let p = project_ndc(Vec3::new(0.4, 0.2, -1.0), symmetric).unwrap();
        assert!((p[0] - 0.4).abs() < 1e-6 && (p[1] - 0.2).abs() < 1e-6);
        assert!(project_ndc(Vec3::new(2.0, 0.0, -1.0), symmetric).unwrap()[0] > 1.0);
    }

    #[test]
    fn asymmetric_ndc_agrees_with_openvr_matrix() {
        let f = fov(-61.5, 43.5, -48.0, 53.0);
        let [l, r, d, u] = [
            f.angle_left.tan(),
            f.angle_right.tan(),
            f.angle_down.tan(),
            f.angle_up.tan(),
        ];
        let m = [
            [2.0 / (r - l), 0.0, (r + l) / (r - l), 0.0],
            [0.0, 2.0 / (u - d), (u + d) / (u - d), 0.0],
            [0.0, 0.0, -1.01, -0.1],
            [0.0, 0.0, -1.0, 0.0],
        ];
        for direction in [Vec3::NEG_Z, Vec3::new(0.4, -0.2, -1.0)] {
            let a = project_ndc(direction, f).unwrap();
            let b = project_matrix(direction, &m).unwrap();
            assert!((a[0] - b[0]).abs() < 1e-6 && (a[1] - b[1]).abs() < 1e-6);
        }
    }

    #[test]
    fn supplied_matrix_uses_rows_and_perspective_divide() {
        let m = [
            [4.0, 0.0, 0.3, 100.0],
            [0.0, 3.0, -0.2, 200.0],
            [0.0, 0.0, -1.0, -1.0],
            [0.0, 0.0, -2.0, 0.0],
        ];
        let p = project_matrix(Vec3::new(0.5, 0.25, -1.0), &m).unwrap();
        assert!((p[0] - 0.85).abs() < 1e-6 && (p[1] - 0.475).abs() < 1e-6);
    }

    #[test]
    fn invalid_directions_and_projections_fail() {
        let f = fov(-45.0, 45.0, -45.0, 45.0);
        for direction in [
            Vec3::Z,
            Vec3::ZERO,
            Vec3::splat(f32::NAN),
            Vec3::splat(f32::INFINITY),
        ] {
            assert!(project_ndc(direction, f).is_none());
        }
        assert!(project_ndc(Vec3::NEG_Z, fov(45.0, -45.0, -45.0, 45.0)).is_none());
        assert!(project_matrix(Vec3::NEG_Z, &[[0.0; 4]; 4]).is_none());
        assert!(project_matrix(Vec3::NEG_Z, &[[f32::NAN; 4]; 4]).is_none());
    }

    #[test]
    fn cache_expires_and_rejects_a_different_frame() {
        let mut sample = Sample {
            direction: Vec3::NEG_Z,
            fovs: [fov(-45.0, 45.0, -45.0, 45.0); 2],
            display_time: xr::Time::from_nanos(42),
            acquired: Instant::now(),
        };
        assert!(sample.current(xr::Time::from_nanos(42)));
        assert!(!sample.current(xr::Time::from_nanos(43)));
        sample.acquired -= Duration::from_millis(101);
        assert!(!sample.current(xr::Time::from_nanos(42)));
    }
}
