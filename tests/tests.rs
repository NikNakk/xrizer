use libloading::{Library, Symbol};
use std::ffi::{c_char, c_void};

#[test]
#[cfg_attr(miri, ignore)]
fn smoke_test() {
    let path = test_cdylib::build_current_project();
    let lib = unsafe { Library::new(path) }.unwrap();
    let factory: Symbol<fn(*const c_char, *mut i32) -> *mut c_void> =
        unsafe { lib.get(b"VRClientCoreFactory\0") }.unwrap();

    let i = factory(c"IVRClientCore_003".as_ptr(), std::ptr::null_mut());
    assert!(!i.is_null());
}


#[test]
#[cfg_attr(miri, ignore)]
fn drop_in_openvr_api_exports() {
    let path = test_cdylib::build_current_project();
    let lib = unsafe { Library::new(path) }.unwrap();

    for symbol in [
        b"VR_InitInternal2\0".as_slice(),
        b"VR_InitInternal\0".as_slice(),
        b"VR_ShutdownInternal\0".as_slice(),
        b"VR_GetGenericInterface\0".as_slice(),
        b"VR_IsInterfaceVersionValid\0".as_slice(),
        b"VR_IsHmdPresent\0".as_slice(),
        b"VR_IsRuntimeInstalled\0".as_slice(),
        b"VR_GetInitToken\0".as_slice(),
        b"VR_GetRuntimePath\0".as_slice(),
        b"VR_RuntimePath\0".as_slice(),
        b"VR_GetVRInitErrorAsSymbol\0".as_slice(),
        b"VR_GetVRInitErrorAsEnglishDescription\0".as_slice(),
        b"VR_GetStringForHmdError\0".as_slice(),
    ] {
        let _: Symbol<unsafe extern "C" fn()> = unsafe { lib.get(symbol) }
            .unwrap_or_else(|e| panic!("missing export {:?}: {e}", String::from_utf8_lossy(symbol)));
    }
}
