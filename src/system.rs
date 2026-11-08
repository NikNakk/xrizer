use crate::{
    clientcore::{Injected, Injector},
    input::Input,
    openxr_data::{Hand, RealOpenXrData, SessionData},
    overlay::OverlayMan,
    tracy_span,
};
use glam::{Mat3, Quat, Vec3};
use log::{debug, error, trace, warn};
use openvr as vr;
use openxr as xr;
use std::ffi::{CStr, CString};
use std::sync::{Arc, Mutex};

#[derive(Copy, Clone)]
pub struct ViewData {
    pub flags: xr::ViewStateFlags,
    pub views: [xr::View; 2],
}

#[derive(Copy, Clone)]
struct ViewDataViewSpace {
    data: ViewData,
    original_orientations: [Quat; 2],
}

#[derive(Default)]
struct ViewCache {
    view: Option<ViewDataViewSpace>,
    local: Option<ViewData>,
    stage: Option<ViewData>,
}

impl ViewCache {
    fn get_views(
        &mut self,
        session: &SessionData,
        display_time: xr::Time,
        ty: xr::ReferenceSpaceType,
    ) -> ViewData {
        match ty {
            xr::ReferenceSpaceType::VIEW => {
                self.view
                    .get_or_insert_with(|| Self::get_views_view_space(session, display_time))
                    .data
            }
            xr::ReferenceSpaceType::LOCAL | xr::ReferenceSpaceType::STAGE => {
                let view = match ty {
                    xr::ReferenceSpaceType::LOCAL => &mut self.local,
                    xr::ReferenceSpaceType::STAGE => &mut self.stage,
                    _ => unreachable!(),
                };

                *view.get_or_insert_with(|| {
                    let view_rots = self
                        .view
                        .get_or_insert_with(|| Self::get_views_view_space(session, display_time))
                        .original_orientations;

                    Self::get_views_other_space(session, display_time, ty, view_rots)
                })
            }
            other => panic!("unexpected reference space type: {other:?}"),
        }
    }

    fn get_views_view_space(session: &SessionData, display_time: xr::Time) -> ViewDataViewSpace {
        let (flags, mut views) = {
            let _api_trace = crate::api_trace::Span::new("xr_locate_views_view", u32::MAX, -1);
            session
                .session
                .locate_views(
                    xr::ViewConfigurationType::PRIMARY_STEREO,
                    display_time,
                    session.get_space_from_type(xr::ReferenceSpaceType::VIEW),
                )
                .expect("Couldn't locate views")
        };

        let original_orientations = views
            .iter_mut()
            .map(
                |xr::View {
                     pose: xr::Posef { orientation: o, .. },
                     ..
                 }| {
                    let ret = Quat::from_xyzw(o.x, o.y, o.z, o.w).inverse();
                    *o = xr::Quaternionf::IDENTITY; // parallel views
                    ret
                },
            )
            .collect::<Vec<_>>()
            .try_into()
            .unwrap();

        ViewDataViewSpace {
            data: ViewData {
                flags,
                views: views
                    .try_into()
                    .unwrap_or_else(|v: Vec<xr::View>| panic!("Expected 2 views, got {}", v.len())),
            },
            original_orientations,
        }
    }

    fn get_views_other_space(
        session: &SessionData,
        display_time: xr::Time,
        ty: xr::ReferenceSpaceType,
        view_data_orientations_inverse: [Quat; 2],
    ) -> ViewData {
        let (flags, mut views) = {
            let _api_trace = crate::api_trace::Span::new("xr_locate_views_world", u32::MAX, -1);
            session
                .session
                .locate_views(
                    xr::ViewConfigurationType::PRIMARY_STEREO,
                    display_time,
                    session.get_space_from_type(ty),
                )
                .expect("Couldn't locate views")
        };

        for (
            xr::View {
                pose: xr::Posef {
                    orientation: rot, ..
                },
                ..
            },
            view_rot,
        ) in views.iter_mut().zip(view_data_orientations_inverse)
        {
            let quat = Quat::from_xyzw(rot.x, rot.y, rot.z, rot.w);
            // rotate the inverse of the view space view rotation by this space's
            // view orientation to remove the canting from the displays in this space
            let adjusted_rot = quat * view_rot;
            *rot = xr::Quaternionf {
                x: adjusted_rot.x,
                y: adjusted_rot.y,
                z: adjusted_rot.z,
                w: adjusted_rot.w,
            };
        }

        ViewData {
            flags,
            views: views
                .try_into()
                .unwrap_or_else(|v: Vec<xr::View>| panic!("Expected 2 views, got {}", v.len())),
        }
    }
}

#[derive(macros::InterfaceImpl)]
#[interface = "IVRSystem"]
#[versions(026, 023, 022, 021, 020, 019, 017, 016, 015, 014, 012, 011, 009)]
pub struct System {
    openxr: Arc<RealOpenXrData>, // We don't need to test session restarting.
    input: Injected<Input<crate::compositor::Compositor>>,
    overlay: Injected<OverlayMan>,
    vtables: Vtables,
    views: Mutex<ViewCache>,
}

// Optional capability/gating diagnostics. Never dereference gaze outputs or
// record device strings.
pub(crate) fn foveation_trace(api: &str, details: std::fmt::Arguments<'_>) {
    use std::sync::{
        OnceLock,
        atomic::{AtomicU32, Ordering},
    };
    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| std::env::var("XRIZER_FOVEATION_TRACE").as_deref() == Ok("1")) {
        return;
    }
    static PROPERTIES: AtomicU32 = AtomicU32::new(0);
    static EYES: AtomicU32 = AtomicU32::new(0);
    static SAMPLES: AtomicU32 = AtomicU32::new(0);
    let gaze = api.contains("Foveation");
    let count = if api == "FoveationSample" {
        SAMPLES.fetch_add(1, Ordering::Relaxed)
    } else if gaze {
        EYES.fetch_add(1, Ordering::Relaxed)
    } else {
        PROPERTIES.fetch_add(1, Ordering::Relaxed)
    };
    if (gaze && (count < 16 || count.is_multiple_of(600))) || (!gaze && count < 256) {
        log::info!("OpenVR foveation trace: api={api} call={count} {details}");
    }
}

mod log_tags {
    pub const TRACKED_PROP: &str = "tracked_property";
}

impl System {
    pub fn new(openxr: Arc<RealOpenXrData>, injector: &Injector) -> Self {
        Self {
            openxr,
            input: injector.inject(),
            overlay: injector.inject(),
            vtables: Default::default(),
            views: Mutex::default(),
        }
    }

    fn application_gaze(&self) -> Option<crate::fdm_gaze::Sample> {
        if !crate::fdm_gaze::application_enabled() {
            return None;
        }
        let session = self.openxr.session_data.get();
        if matches!(
            session.state,
            xr::SessionState::STOPPING | xr::SessionState::LOSS_PENDING | xr::SessionState::EXITING
        ) {
            return None;
        }
        let sample = *session.fdm_gaze.lock().unwrap();
        sample.filter(|sample| sample.current(self.openxr.display_time.get()))
    }

    pub fn reset_views(&self) {
        std::mem::take(&mut *self.views.lock().unwrap());
        let session = self.openxr.session_data.get();
        let display_time = self.openxr.display_time.get();
        let mut views = self.views.lock().unwrap();
        views.get_views(&session, display_time, xr::ReferenceSpaceType::VIEW);
        views.get_views(
            &session,
            display_time,
            session.current_origin_as_reference_space(),
        );
    }

    pub fn get_views(&self, ty: xr::ReferenceSpaceType) -> ViewData {
        tracy_span!();
        let session = self.openxr.session_data.get();
        let mut views = self.views.lock().unwrap();
        views.get_views(&session, self.openxr.display_time.get(), ty)
    }

    pub fn recommended_render_target_size(&self) -> (u32, u32) {
        let views = self
            .openxr
            .instance
            .enumerate_view_configuration_views(
                self.openxr.system_id,
                xr::ViewConfigurationType::PRIMARY_STEREO,
            )
            .expect("Couldn't enumerate stereo view configuration");
        (
            views[0].recommended_image_rect_width,
            views[0].recommended_image_rect_height,
        )
    }

    #[cfg(target_os = "windows")]
    fn d3d_adapter_luid(&self) -> Option<u64> {
        let requirements = self
            .openxr
            .instance
            .graphics_requirements::<xr::D3D11>(self.openxr.system_id)
            .ok()?;
        let luid = requirements.adapter_luid;
        Some(((luid.HighPart as u32 as u64) << 32) | luid.LowPart as u64)
    }
}

impl vr::IVRSystem026_Interface for System {
    fn GetRecommendedRenderTargetSize(&self, width: *mut u32, height: *mut u32) {
        let (recommended_width, recommended_height) = self.recommended_render_target_size();

        if !width.is_null() {
            unsafe { *width = recommended_width };
        }

        if !height.is_null() {
            unsafe { *height = recommended_height };
        }
    }
    fn GetProjectionMatrix(&self, eye: vr::EVREye, near_z: f32, far_z: f32) -> vr::HmdMatrix44_t {
        // https://github.com/ValveSoftware/openvr/wiki/IVRSystem::GetProjectionRaw
        let [mut left, mut right, mut up, mut down] = [0.0; 4];
        self.GetProjectionRaw(eye, &mut left, &mut right, &mut down, &mut up);

        let idx = 1.0 / (right - left);
        let idy = 1.0 / (up - down);
        let idz = 1.0 / (far_z - near_z);
        let sx = right + left;
        let sy = up + down;

        vr::HmdMatrix44_t {
            m: [
                [2.0 * idx, 0.0, sx * idx, 0.0],
                [0.0, 2.0 * idy, sy * idy, 0.0],
                [0.0, 0.0, -far_z * idz, -far_z * near_z * idz],
                [0.0, 0.0, -1.0, 0.0],
            ],
        }
    }
    fn GetProjectionRaw(
        &self,
        eye: vr::EVREye,
        left: *mut f32,
        right: *mut f32,
        top: *mut f32,
        bottom: *mut f32,
    ) {
        let ty = self
            .openxr
            .session_data
            .get()
            .current_origin_as_reference_space();
        let view = self.get_views(ty).views[eye as usize];

        // Top and bottom are flipped, for some reason
        unsafe {
            *left = view.fov.angle_left.tan();
            *right = view.fov.angle_right.tan();
            *bottom = view.fov.angle_up.tan();
            *top = view.fov.angle_down.tan();
        }
    }
    fn ComputeDistortion(
        &self,
        _: vr::EVREye,
        _: f32,
        _: f32,
        _: *mut vr::DistortionCoordinates_t,
    ) -> bool {
        crate::warn_unimplemented!("ComputeDistortion");
        false
    }
    fn ComputeDistortionSet(
        &self,
        _: openvr::EVREye,
        _: openvr::EVRDistortionChannel,
        _: bool,
        _: u32,
        _: *const openvr::DistortionCoordinate_t,
        _: *mut openvr::DistortionCoordinate_t,
    ) -> bool {
        crate::warn_unimplemented!("ComputeDistortionSet");
        false
    }
    fn GetEyeToHeadTransform(&self, eye: vr::EVREye) -> vr::HmdMatrix34_t {
        let views = self.get_views(xr::ReferenceSpaceType::VIEW).views;
        let view = views[eye as usize];
        let view_rot = view.pose.orientation;

        {
            tracy_span!("conversion");
            let rot = Mat3::from_quat(Quat::from_xyzw(
                view_rot.x, view_rot.y, view_rot.z, view_rot.w,
            ))
            .transpose();

            let gen_array = |translation, rot_axis: Vec3| {
                std::array::from_fn(|i| if i == 3 { translation } else { rot_axis[i] })
            };
            vr::HmdMatrix34_t {
                m: [
                    gen_array(view.pose.position.x, rot.x_axis),
                    gen_array(view.pose.position.y, rot.y_axis),
                    gen_array(view.pose.position.z, rot.z_axis),
                ],
            }
        }
    }
    fn GetTimeSinceLastVsync(&self, _: *mut f32, _: *mut u64) -> bool {
        crate::warn_unimplemented!("GetTimeSinceLastVsync");
        false
    }
    fn GetRuntimeVersion(&self) -> *const std::os::raw::c_char {
        static VERSION: &CStr = c"2.15.6";
        VERSION.as_ptr()
    }
    fn SetSDKVersion(&self, _: u32, _: u32, _: u32) -> vr::EVRInitError {
        vr::EVRInitError::None
    }
    fn GetAppContainerFilePaths(&self, _: *mut std::os::raw::c_char, _: u32) -> u32 {
        todo!()
    }
    fn AcknowledgeQuit_Exiting(&self) {
        log::debug!("Application acknowledged quit and is exiting");
    }
    fn PerformFirmwareUpdate(&self, _: vr::TrackedDeviceIndex_t) -> vr::EVRFirmwareError {
        todo!()
    }
    fn ShouldApplicationReduceRenderingWork(&self) -> bool {
        false
    }
    fn ShouldApplicationPause(&self) -> bool {
        false
    }
    fn IsSteamVRDrawingControllers(&self) -> bool {
        todo!()
    }
    fn IsInputAvailable(&self) -> bool {
        true
    }
    fn GetControllerAxisTypeNameFromEnum(
        &self,
        _: vr::EVRControllerAxisType,
    ) -> *const std::os::raw::c_char {
        crate::warn_unimplemented!("GetControllerAxisTypeNameFromEnum");
        static NAME: &CStr = c"Unknown";
        NAME.as_ptr()
    }
    fn GetButtonIdNameFromEnum(&self, _: vr::EVRButtonId) -> *const std::os::raw::c_char {
        crate::warn_unimplemented!("GetButtonIdNameFromEnum");
        static NAME: &CStr = c"Unknown";
        NAME.as_ptr()
    }
    fn TriggerHapticPulse(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        axis_id: u32,
        duration_us: std::ffi::c_ushort,
    ) {
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .legacy_haptic(device_index, axis_id, duration_us);
    }
    fn GetControllerStateWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        state_size: u32,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        let input = self.input.force(|_| Input::new(self.openxr.clone()));

        let Some(hand) = input.device_index_to_hand(device_index) else {
            return false;
        };

        if self.GetControllerState(device_index, state, state_size) {
            unsafe {
                *pose.as_mut().unwrap() = self
                    .input
                    .get()
                    .unwrap()
                    .get_controller_pose(hand, Some(origin))
                    .unwrap_or_default();
            }
            true
        } else {
            false
        }
    }
    fn GetControllerState(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        state_size: u32,
    ) -> bool {
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .get_legacy_controller_state(device_index, state, state_size)
    }
    fn GetHiddenAreaMesh(
        &self,
        eye: vr::EVREye,
        ty: vr::EHiddenAreaMeshType,
    ) -> vr::HiddenAreaMesh_t {
        if !self.openxr.enabled_extensions.khr_visibility_mask {
            return Default::default();
        }

        debug!("GetHiddenAreaMesh: area mesh type: {ty:?}");
        let mask_ty = match ty {
            vr::EHiddenAreaMeshType::Standard => xr::VisibilityMaskTypeKHR::HIDDEN_TRIANGLE_MESH,
            vr::EHiddenAreaMeshType::Inverse => xr::VisibilityMaskTypeKHR::VISIBLE_TRIANGLE_MESH,
            vr::EHiddenAreaMeshType::LineLoop => xr::VisibilityMaskTypeKHR::LINE_LOOP,
            vr::EHiddenAreaMeshType::Max => {
                warn!("Unexpectedly got EHiddenAreaMeshType::Max - returning default area mesh");
                return Default::default();
            }
        };

        let session_data = self.openxr.session_data.get();
        let mask = session_data
            .session
            .get_visibility_mask_khr(
                xr::ViewConfigurationType::PRIMARY_STEREO,
                eye as u32,
                mask_ty,
            )
            .unwrap();

        trace!("openxr mask: {:#?} {:#?}", mask.indices, mask.vertices);

        let [mut left, mut right, mut top, mut bottom] = [0.0; 4];
        self.GetProjectionRaw(eye, &mut left, &mut right, &mut top, &mut bottom);

        // convert from indices + vertices to just vertices
        let vertices: Vec<_> = mask
            .indices
            .into_iter()
            .map(|i| {
                let v = mask.vertices[i as usize];

                // It is unclear to me why this scaling is necessary, but OpenComposite does it and
                // it seems to get games to use the mask correctly.
                let x_scaled = (v.x - left) / (right - left);
                let y_scaled = (v.y - top) / (bottom - top);
                vr::HmdVector2_t {
                    v: [x_scaled, y_scaled],
                }
            })
            .collect();

        trace!("vertices: {vertices:#?}");
        let count = vertices.len() / 3;
        // XXX: what are we supposed to do here? pVertexData is a random pointer and there's no
        // clear way for the application to deallocate it
        // fortunately it seems like applications don't call this often, so this leakage isn't a
        // huge deal.
        let vertices = Vec::leak(vertices).as_ptr();

        vr::HiddenAreaMesh_t {
            pVertexData: vertices,
            unTriangleCount: count as u32,
        }
    }

    fn GetEyeTrackedFoveationCenter(
        &self,
        left: *mut vr::HmdVector2_t,
        right: *mut vr::HmdVector2_t,
    ) -> bool {
        let centres = self.application_gaze().and_then(|sample| sample.centres());
        let valid = !left.is_null() && !right.is_null() && centres.is_some();
        if valid {
            let points = centres.unwrap();
            unsafe {
                (*left).v = points[0];
                (*right).v = points[1];
            }
        }
        foveation_trace(
            "GetEyeTrackedFoveationCenter",
            format_args!("result={valid} ndc={centres:?}"),
        );
        valid
    }

    fn GetEyeTrackedFoveationCenterForProjection(
        &self,
        matrix: *const vr::HmdMatrix44_t,
        point: *mut vr::HmdVector2_t,
    ) -> bool {
        let result = if matrix.is_null() || point.is_null() {
            None
        } else {
            self.application_gaze().and_then(|sample| unsafe {
                crate::fdm_gaze::project_matrix(sample.direction, &(*matrix).m)
            })
        };
        if let Some(value) = result {
            unsafe {
                (*point).v = value;
            }
        }
        foveation_trace(
            "GetEyeTrackedFoveationCenterForProjection",
            format_args!("result={} ndc={result:?}", result.is_some()),
        );
        result.is_some()
    }

    fn GetEventTypeNameFromEnum(&self, _: vr::EVREventType) -> *const std::os::raw::c_char {
        todo!()
    }

    fn PollNextEventWithPoseAndOverlays(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::VREvent_t,
        size: u32,
        pose: *mut vr::TrackedDevicePose_t,
        overlay_handle: *mut vr::VROverlayHandle_t,
    ) -> bool {
        if self.PollNextEventWithPose(origin, event, size, pose) {
            return true;
        }
        let Some(overlay) = self.overlay.get() else {
            return false;
        };

        if let Some(handle) = overlay.get_next_overlay_event(event) {
            if !overlay_handle.is_null() {
                unsafe {
                    overlay_handle.write(handle);
                }
            }
            true
        } else {
            false
        }
    }

    fn PollNextEventWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::VREvent_t,
        size: u32,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        let Some(input) = self.input.get() else {
            return false;
        };

        let got_event = input.get_next_event(size, event);
        if got_event && !pose.is_null() {
            unsafe {
                let index = (&raw const (*event).trackedDeviceIndex).read();
                // Global/system events use an invalid tracked-device index and
                // therefore have no corresponding device pose. PollNextEventWithPose
                // must still deliver those events rather than panicking.
                pose.write(
                    input
                        .get_device_pose(index, Some(origin))
                        .unwrap_or_default(),
                );
            }
        }
        got_event
    }

    fn PollNextEvent(&self, event: *mut vr::VREvent_t, size: u32) -> bool {
        self.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Seated,
            event,
            size,
            std::ptr::null_mut(),
        )
    }

    fn GetPropErrorNameFromEnum(
        &self,
        _: vr::ETrackedPropertyError,
    ) -> *const std::os::raw::c_char {
        c"Unknown error".as_ptr()
    }
    fn GetStringTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        value: *mut std::os::raw::c_char,
        size: u32,
        error: *mut vr::ETrackedPropertyError,
    ) -> u32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting string property: {prop:?} ({device_index})");
        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            foveation_trace(
                "property_string",
                format_args!("device={device_index} property={prop:?}"),
            );
        }

        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::Success;
        }

        let buf = if !value.is_null() && size > 0 {
            unsafe { std::slice::from_raw_parts_mut(value, size as usize) }
        } else {
            &mut []
        };

        let data = match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => match prop {
                // The Unity OpenVR sample appears to have a hard requirement on these first three properties returning
                // something to even get the game to recognize the HMD's location. However, the value
                // itself doesn't appear to be that important.
                vr::ETrackedDeviceProperty::SerialNumber_String
                | vr::ETrackedDeviceProperty::ControllerType_String => {
                    Some(CString::new("<unknown>").unwrap())
                }
                // OpenXR does not name a manufacturer: report the runtime and system
                // names, as SteamVR drivers built on OpenXR runtimes do.
                vr::ETrackedDeviceProperty::ManufacturerName_String => Some(
                    self.openxr
                        .instance
                        .properties()
                        .ok()
                        .and_then(|p| CString::new(p.runtime_name).ok())
                        .unwrap_or_else(|| CString::new("<unknown>").unwrap()),
                ),
                vr::ETrackedDeviceProperty::ModelNumber_String => self
                    .openxr
                    .instance
                    .system_properties(self.openxr.system_id)
                    .ok()
                    .and_then(|p| CString::new(p.system_name).ok()),
                _ => None,
            },
            _ => self
                .input
                .get()
                .and_then(|input| input.get_device_string_tracked_property(device_index, prop)),
        };

        let Some(data) = data else {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::UnknownProperty;
            }
            return 0;
        };

        let data =
            unsafe { std::slice::from_raw_parts(data.as_ptr(), data.to_bytes_with_nul().len()) };
        if buf.len() < data.len() {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::BufferTooSmall;
            }
        } else {
            buf[0..data.len()].copy_from_slice(data);
        }

        data.len() as u32
    }
    fn GetArrayTrackedDeviceProperty(
        &self,
        _: vr::TrackedDeviceIndex_t,
        _: vr::ETrackedDeviceProperty,
        _: vr::PropertyTypeTag_t,
        _: *mut std::os::raw::c_void,
        _: u32,
        _: *mut vr::ETrackedPropertyError,
    ) -> u32 {
        todo!()
    }
    fn GetMatrix34TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> vr::HmdMatrix34_t {
        debug!(target: log_tags::TRACKED_PROP, "requesting matrix property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return Default::default();
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::UnknownProperty;
        }
        Default::default()
    }
    fn GetUint64TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> u64 {
        debug!(target: log_tags::TRACKED_PROP, "requesting uint64 property: {prop:?} ({device_index})");
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::Success;
        }

        self.input
            .get()
            .and_then(|input| input.get_device_uint_tracked_property(device_index, prop))
            .unwrap_or_else(|| {
                if let Some(err) = unsafe { err.as_mut() } {
                    *err = vr::ETrackedPropertyError::UnknownProperty;
                }
                0
            })
    }
    fn GetInt32TrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> i32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting int32 property: {prop:?} ({device_index})");
        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            foveation_trace(
                "property_int32",
                format_args!("device={device_index} property={prop:?}"),
            );
        }
        if !self.IsTrackedDeviceConnected(device_index) {
            if let Some(err) = unsafe { err.as_mut() } {
                *err = vr::ETrackedPropertyError::InvalidDevice;
            }
            return 0;
        }

        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::Success;
        }
        self.input
            .get()
            .and_then(|input| input.get_device_int_tracked_property(device_index, prop))
            .unwrap_or_else(|| {
                if let Some(err) = unsafe { err.as_mut() } {
                    *err = vr::ETrackedPropertyError::UnknownProperty;
                }
                0
            })
    }
    fn GetFloatTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        error: *mut vr::ETrackedPropertyError,
    ) -> f32 {
        debug!(target: log_tags::TRACKED_PROP, "requesting float property: {prop:?} ({device_index})");
        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            foveation_trace(
                "property_float",
                format_args!("device={device_index} property={prop:?}"),
            );
        }
        if device_index != vr::k_unTrackedDeviceIndex_Hmd {
            if let Some(error) = unsafe { error.as_mut() } {
                *error = vr::ETrackedPropertyError::UnknownProperty;
            }
            return 0.0;
        }

        if let Some(error) = unsafe { error.as_mut() } {
            *error = vr::ETrackedPropertyError::Success;
        }
        match prop {
            vr::ETrackedDeviceProperty::UserIpdMeters_Float => {
                let views = self.get_views(xr::ReferenceSpaceType::VIEW).views;
                views[1].pose.position.x - views[0].pose.position.x
            }
            vr::ETrackedDeviceProperty::DisplayFrequency_Float => self.openxr.get_refresh_rate(),
            _ => {
                if let Some(error) = unsafe { error.as_mut() } {
                    *error = vr::ETrackedPropertyError::UnknownProperty;
                }
                0.0
            }
        }
    }
    fn GetBoolTrackedDeviceProperty(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        prop: vr::ETrackedDeviceProperty,
        err: *mut vr::ETrackedPropertyError,
    ) -> bool {
        debug!(target: log_tags::TRACKED_PROP, "requesting bool property: {prop:?} ({device_index})");
        if device_index == vr::k_unTrackedDeviceIndex_Hmd {
            foveation_trace(
                "property_bool",
                format_args!("device={device_index} property={prop:?}"),
            );
        }
        if let Some(err) = unsafe { err.as_mut() } {
            *err = vr::ETrackedPropertyError::UnknownProperty;
        }
        false
    }

    fn IsTrackedDeviceConnected(&self, device_index: vr::TrackedDeviceIndex_t) -> bool {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => true,
            _ => self
                .input
                .get()
                .is_some_and(|input| input.is_device_connected(device_index)),
        }
    }

    fn GetTrackedDeviceClass(&self, index: vr::TrackedDeviceIndex_t) -> vr::ETrackedDeviceClass {
        match index {
            vr::k_unTrackedDeviceIndex_Hmd => vr::ETrackedDeviceClass::HMD,
            _ => self
                .input
                .get()
                .and_then(|input| input.device_index_to_tracked_device_class(index))
                .unwrap_or(vr::ETrackedDeviceClass::Invalid),
        }
    }

    fn GetControllerRoleForTrackedDeviceIndex(
        &self,
        index: vr::TrackedDeviceIndex_t,
    ) -> vr::ETrackedControllerRole {
        let Some(input) = self.input.get() else {
            return vr::ETrackedControllerRole::Invalid;
        };
        input
            .device_index_to_hand(index)
            .map_or(vr::ETrackedControllerRole::Invalid, |hand| hand.into())
    }

    fn GetTrackedDeviceIndexForControllerRole(
        &self,
        role: vr::ETrackedControllerRole,
    ) -> vr::TrackedDeviceIndex_t {
        let Some(input) = self.input.get() else {
            return vr::k_unTrackedDeviceIndexInvalid;
        };

        Hand::try_from(role).map_or(vr::k_unTrackedDeviceIndexInvalid, |hand| {
            input
                .get_controller_device_index(hand)
                .unwrap_or(vr::k_unTrackedDeviceIndexInvalid)
        })
    }
    fn ApplyTransform(
        &self,
        _: *mut vr::TrackedDevicePose_t,
        _: *const vr::TrackedDevicePose_t,
        _: *const vr::HmdMatrix34_t,
    ) {
        todo!()
    }
    fn GetTrackedDeviceActivityLevel(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
    ) -> vr::EDeviceActivityLevel {
        match device_index {
            vr::k_unTrackedDeviceIndex_Hmd => vr::EDeviceActivityLevel::UserInteraction,
            x if self
                .input
                .get()
                .is_some_and(|input| input.device_index_to_hand(x).is_some()) =>
            {
                if self.IsTrackedDeviceConnected(x) {
                    vr::EDeviceActivityLevel::UserInteraction
                } else {
                    vr::EDeviceActivityLevel::Unknown
                }
            }
            _ => vr::EDeviceActivityLevel::Unknown,
        }
    }
    fn GetSortedTrackedDeviceIndicesOfClass(
        &self,
        class: vr::ETrackedDeviceClass,
        indices: *mut vr::TrackedDeviceIndex_t,
        index_count: u32,
        _relative_to: vr::TrackedDeviceIndex_t,
    ) -> u32 {
        let Some(input) = self.input.get() else {
            return 0;
        };

        let devices = input.connected_device_indices_of_class(class);
        if index_count != 0 && !indices.is_null() {
            let output = unsafe { std::slice::from_raw_parts_mut(indices, index_count as usize) };
            for (dst, src) in output.iter_mut().zip(devices.iter().copied()) {
                *dst = src;
            }
        }

        devices.len() as u32
    }
    fn GetRawZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        xr::Posef::IDENTITY.into()
    }
    fn GetSeatedZeroPoseToStandingAbsoluteTrackingPose(&self) -> vr::HmdMatrix34_t {
        xr::Posef::IDENTITY.into()
    }
    fn GetDeviceToAbsoluteTrackingPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        _seconds_to_photon_from_now: f32,
        pose_array: *mut vr::TrackedDevicePose_t,
        pose_count: u32,
    ) {
        self.input
            .force(|_| Input::new(self.openxr.clone()))
            .get_poses(
                unsafe { std::slice::from_raw_parts_mut(pose_array, pose_count as usize) },
                Some(origin),
            );
    }
    fn SetDisplayVisibility(&self, _: bool) -> bool {
        // Act as if we're limited to direct mode
        false
    }
    fn IsDisplayOnDesktop(&self) -> bool {
        // Direct mode
        false
    }
    fn GetOutputDevice(
        &self,
        device: *mut u64,
        texture_type: vr::ETextureType,
        instance: *mut vr::VkInstance_T,
    ) {
        if device.is_null() {
            return;
        }

        let output = match texture_type {
            vr::ETextureType::Vulkan if !instance.is_null() => unsafe {
                self.openxr
                    .instance
                    .vulkan_graphics_device(self.openxr.system_id, instance as _)
                    .map(|device| device as u64)
                    .unwrap_or(0)
            },
            #[cfg(target_os = "windows")]
            vr::ETextureType::DirectX | vr::ETextureType::DirectX12 => {
                self.d3d_adapter_luid().unwrap_or(0)
            }
            other => {
                log::warn!("No output-device mapping for texture type: {other:?}");
                0
            }
        };

        unsafe { *device = output };
    }
    fn GetDXGIOutputInfo(&self, adapter_index: *mut i32) {
        if let Some(adapter_index) = unsafe { adapter_index.as_mut() } {
            // Our Wine/DXMT path currently exposes a single DXGI adapter.
            *adapter_index = 0;
        }
    }
    fn GetD3D9AdapterIndex(&self) -> i32 {
        0
    }
}

impl vr::IVRSystem021On022 for System {
    fn ResetSeatedZeroPose(&self) {
        self.openxr
            .reset_tracking_space(vr::ETrackingUniverseOrigin::Seated);
    }
}

impl vr::IVRSystem020On021 for System {
    fn AcknowledgeQuit_UserPrompt(&self) {}
}

impl vr::IVRSystem019On020 for System {
    fn DriverDebugRequest(
        &self,
        _un_device_index: vr::TrackedDeviceIndex_t,
        _pch_request: *const std::os::raw::c_char,
        _pch_response_buffer: *mut std::os::raw::c_char,
        _un_response_buffer_size: u32,
    ) -> u32 {
        unimplemented!()
    }
}

impl vr::IVRSystem017On019 for System {
    fn IsInputFocusCapturedByAnotherProcess(&self) -> bool {
        false
    }
    fn ReleaseInputFocus(&self) {}
    fn CaptureInputFocus(&self) -> bool {
        true
    }
}

impl vr::IVRSystem016On017 for System {
    fn GetOutputDevice(&self, device: *mut u64, texture_type: vr::ETextureType) {
        if device.is_null() {
            return;
        }

        let output = match texture_type {
            #[cfg(target_os = "windows")]
            vr::ETextureType::DirectX | vr::ETextureType::DirectX12 => {
                self.d3d_adapter_luid().unwrap_or(0)
            }
            _ => 0,
        };
        unsafe { *device = output };
    }
}

impl vr::IVRSystem014On015 for System {
    fn GetProjectionMatrix(
        &self,
        eye: vr::EVREye,
        near_z: f32,
        far_z: f32,
        _proj_type: vr::EGraphicsAPIConvention,
    ) -> vr::HmdMatrix44_t {
        // According to this bug: https://github.com/ValveSoftware/openvr/issues/70 the projection type
        // is straight up ignored in SteamVR anyway, lol. Bug for bug compat!

        <Self as vr::IVRSystem022_Interface>::GetProjectionMatrix(self, eye, near_z, far_z)
    }
}

impl vr::IVRSystem012On014 for System {
    fn ComputeDistortion(&self, eye: vr::EVREye, u: f32, v: f32) -> vr::DistortionCoordinates_t {
        let mut ret = vr::DistortionCoordinates_t::default();
        <Self as vr::IVRSystem022_Interface>::ComputeDistortion(self, eye, u, v, &mut ret);
        ret
    }

    fn GetHiddenAreaMesh(&self, eye: vr::EVREye) -> vr::HiddenAreaMesh_t {
        <Self as vr::IVRSystem022_Interface>::GetHiddenAreaMesh(
            self,
            eye,
            vr::EHiddenAreaMeshType::Standard,
        )
    }

    fn GetControllerState(
        &self,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
    ) -> bool {
        <Self as vr::IVRSystem022_Interface>::GetControllerState(
            self,
            device_index,
            state,
            std::mem::size_of::<vr::VRControllerState_t>() as u32,
        )
    }

    fn GetControllerStateWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        device_index: vr::TrackedDeviceIndex_t,
        state: *mut vr::VRControllerState_t,
        device_pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        <Self as vr::IVRSystem022_Interface>::GetControllerStateWithPose(
            self,
            origin,
            device_index,
            state,
            std::mem::size_of::<vr::VRControllerState_t>() as u32,
            device_pose,
        )
    }
}

impl vr::IVRSystem011On012 for System {
    fn PerformanceTestEnableCapture(&self, _: bool) {
        todo!()
    }

    fn PerformanceTestReportFidelityLevelChange(&self, _: i32) {
        todo!()
    }
}

impl vr::IVRSystem009On011 for System {
    fn PollNextEvent(&self, event: *mut vr::vr_0_9_12::VREvent_t) -> bool {
        self.PollNextEventWithPose(
            vr::ETrackingUniverseOrigin::Seated,
            event,
            std::ptr::null_mut(),
        )
    }

    fn PollNextEventWithPose(
        &self,
        origin: vr::ETrackingUniverseOrigin,
        event: *mut vr::vr_0_9_12::VREvent_t,
        pose: *mut vr::TrackedDevicePose_t,
    ) -> bool {
        let mut e = vr::VREvent_t::default();
        let ret = <Self as vr::IVRSystem022_Interface>::PollNextEventWithPose(
            self,
            origin,
            &mut e,
            std::mem::size_of_val(&event) as u32,
            pose,
        );

        if ret && !event.is_null() {
            let event = unsafe { event.as_mut() }.unwrap();
            event.eventType = if let Ok(t) = vr::EVREventType::try_from(e.eventType) {
                t
            } else {
                error!("Unhandled event type for 0.9.12: {}", e.eventType);
                return false;
            };
            event.trackedDeviceIndex = e.trackedDeviceIndex;
            event.data = match e.eventType {
                x if x == vr::EVREventType::ButtonPress as u32
                    || x == vr::EVREventType::ButtonUnpress as u32
                    || x == vr::EVREventType::ButtonTouch as u32
                    || x == vr::EVREventType::ButtonUntouch as u32 =>
                {
                    vr::vr_0_9_12::VREvent_Data_t {
                        controller: unsafe { e.data.controller },
                    }
                }
                other => {
                    error!("Unhandled event type data for 0.9.12: {other:?}");
                    return false;
                }
            }
        }

        ret
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clientcore::Injector, openxr_data::OpenXrData};
    use std::ffi::CStr;
    use std::sync::atomic::Ordering;
    use vr::IVRSystem022_Interface;

    #[test]
    fn quit_acknowledgement_preserves_session_state() {
        let xr = Arc::new(OpenXrData::new(&Injector::default()).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        let state = xr.session_data.get().state;

        system.AcknowledgeQuit_Exiting();
        system.AcknowledgeQuit_Exiting();

        assert_eq!(xr.session_data.get().state, state);
    }

    #[test]
    fn unsupported_foveation_preserves_output_buffers() {
        use vr::IVRSystem026_Interface;
        crate::init_logging();
        let xr = Arc::new(OpenXrData::new(&Injector::default()).unwrap());
        let system = System::new(xr, &Injector::default());
        let mut left = vr::HmdVector2_t { v: [0.25, -0.5] };
        let mut right = vr::HmdVector2_t { v: [-0.75, 1.0] };
        assert!(!system.GetEyeTrackedFoveationCenter(&mut left, &mut right));
        assert_eq!(left.v, [0.25, -0.5]);
        assert_eq!(right.v, [-0.75, 1.0]);
        assert!(!system.GetEyeTrackedFoveationCenterForProjection(std::ptr::null(), &mut left));
        assert_eq!(left.v, [0.25, -0.5]);
        assert!(!system.GetEyeTrackedFoveationCenter(std::ptr::null_mut(), std::ptr::null_mut()));
    }

    #[test]
    #[ignore = "requires XRIZER_FDM_GAZE_MODE=application"]
    fn application_gaze_api_matches_projection_and_preserves_invalid_outputs() {
        use vr::IVRSystem026_Interface;
        assert!(crate::fdm_gaze::application_enabled());
        let xr = Arc::new(OpenXrData::new(&Injector::default()).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        let fovs = [
            xr::Fovf {
                angle_left: -1.0,
                angle_right: 0.7,
                angle_down: -0.8,
                angle_up: 0.9,
            },
            xr::Fovf {
                angle_left: -0.7,
                angle_right: 1.0,
                angle_down: -0.8,
                angle_up: 0.9,
            },
        ];
        let direction = Vec3::new(0.2, 0.1, -1.0);
        let sample = crate::fdm_gaze::Sample {
            direction,
            fovs,
            display_time: xr.display_time.get(),
            acquired: std::time::Instant::now(),
        };
        {
            let session = xr.session_data.get();
            fakexr::set_view_fovs(session.session.as_raw(), Some(fovs));
            *session.fdm_gaze.lock().unwrap() = Some(sample);
        }
        let mut left = vr::HmdVector2_t { v: [9.0, 9.0] };
        let mut right = left;
        assert!(system.GetEyeTrackedFoveationCenter(&mut left, &mut right));
        for (eye, result) in [(vr::EVREye::Left, left.v), (vr::EVREye::Right, right.v)] {
            let matrix = vr::IVRSystem026_Interface::GetProjectionMatrix(&system, eye, 0.1, 100.0);
            let expected = crate::fdm_gaze::project_matrix(direction, &matrix.m).unwrap();
            assert!(
                (expected[0] - result[0]).abs() < 1e-6 && (expected[1] - result[1]).abs() < 1e-6
            );
            let mut supplied = vr::HmdVector2_t { v: [9.0, 9.0] };
            assert!(system.GetEyeTrackedFoveationCenterForProjection(&matrix, &mut supplied));
            assert_eq!(supplied.v, expected);
        }
        let before = right.v;
        assert!(!system.GetEyeTrackedFoveationCenter(std::ptr::null_mut(), &mut right));
        assert_eq!(right.v, before);
        let invalid_matrix = vr::HmdMatrix44_t {
            m: [[f32::NAN; 4]; 4],
        };
        assert!(!system.GetEyeTrackedFoveationCenterForProjection(&invalid_matrix, &mut right));
        assert_eq!(right.v, before);
        let mut stale = sample;
        stale.acquired -= std::time::Duration::from_millis(101);
        *xr.session_data.get().fdm_gaze.lock().unwrap() = Some(stale);
        assert!(!system.GetEyeTrackedFoveationCenter(&mut left, &mut right));
        assert_eq!(right.v, before);
        *xr.session_data.get().fdm_gaze.lock().unwrap() = Some(sample);
        xr.display_time
            .set(xr::Time::from_nanos(sample.display_time.as_nanos() + 1));
        assert!(!system.GetEyeTrackedFoveationCenter(&mut left, &mut right));
        assert_eq!(right.v, before);
    }

    #[test]
    fn display_frequency_is_a_rate() {
        let xr = Arc::new(OpenXrData::new(&Injector::default()).unwrap());
        let system = System::new(xr.clone(), &Injector::default());
        xr.display_period_nanos.store(8_333_333, Ordering::Relaxed);

        // Callers such as Half-Life: Alyx treat an untouched error as a failure.
        let mut err = vr::ETrackedPropertyError::UnknownProperty;
        let rate = system.GetFloatTrackedDeviceProperty(
            vr::k_unTrackedDeviceIndex_Hmd,
            vr::ETrackedDeviceProperty::DisplayFrequency_Float,
            &mut err,
        );
        assert_eq!(err, vr::ETrackedPropertyError::Success);
        assert!((rate - 120.0).abs() < 0.01, "{rate}");
    }

    #[test]
    fn unity_required_properties() {
        let xr = Arc::new(OpenXrData::new(&Injector::default()).unwrap());
        let injector = Injector::default();
        let input = Arc::new(Input::new(xr.clone()));
        let system = System::new(xr, &injector);

        system.input.set(Arc::downgrade(&input));

        let test_prop = |property| {
            let mut err = vr::ETrackedPropertyError::Success;
            let len = system.GetStringTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                property,
                std::ptr::null_mut(),
                0,
                &mut err,
            );
            assert_eq!(err, vr::ETrackedPropertyError::BufferTooSmall);
            assert!(len > 0);
            let mut buf = vec![0; len as usize];

            let len = system.GetStringTrackedDeviceProperty(
                vr::k_unTrackedDeviceIndex_Hmd,
                property,
                buf.as_mut_ptr(),
                buf.len() as u32,
                &mut err,
            );
            assert_eq!(err, vr::ETrackedPropertyError::Success);
            assert_eq!(len, buf.len() as u32);

            let slice = unsafe { std::slice::from_raw_parts(buf.as_ptr() as _, buf.len()) };
            CStr::from_bytes_with_nul(slice)
                .expect("Failed to convert returned buffer for {property:?} to CStr");
        };

        test_prop(vr::ETrackedDeviceProperty::SerialNumber_String);
        test_prop(vr::ETrackedDeviceProperty::ManufacturerName_String);
        test_prop(vr::ETrackedDeviceProperty::ModelNumber_String);
        test_prop(vr::ETrackedDeviceProperty::ControllerType_String);
    }
}
