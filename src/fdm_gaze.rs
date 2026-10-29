//! Gaze for gaze-centred fragment density maps.
//!
//! With `XRIZER_FDM_GAZE=1`, xrizer reads the user's gaze through
//! `XR_EXT_eye_gaze_interaction`, projects it into each eye's field of view and
//! hands it to the KosmicKrisp Vulkan driver in the same process
//! (`kk_fdm_set_gaze`), which centres its fragment density maps on it.
//! `XRIZER_FAKE_GAZE=yaw,pitch` (degrees, right and up positive) replaces the
//! eye tracker with a fixed direction, for testing without one.

use glam::{Quat, Vec3};
use log::{info, warn};
use openxr as xr;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

/// Whether gaze-centred density maps were requested.
pub fn enabled() -> bool {
    static ENABLED: OnceLock<bool> = OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("XRIZER_FDM_GAZE").is_ok_and(|v| v != "0") || fake_direction().is_some()
    })
}

/// A fixed gaze direction in view space from `XRIZER_FAKE_GAZE`.
pub fn fake_direction() -> Option<Vec3> {
    static FAKE: OnceLock<Option<Vec3>> = OnceLock::new();
    *FAKE.get_or_init(|| {
        let value = std::env::var("XRIZER_FAKE_GAZE").ok()?;
        let mut parts = value.split(',').map(|p| p.trim().parse::<f32>());
        let yaw = parts.next()?.ok()?.to_radians();
        let pitch = parts.next()?.ok()?.to_radians();
        Some(Quat::from_euler(glam::EulerRot::YXZ, -yaw, pitch, 0.0) * Vec3::NEG_Z)
    })
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
