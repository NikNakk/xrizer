#![deny(clippy::all)]

mod applications;
mod chaperone;
mod clientcore;
mod compositor;
mod graphics_backends;
mod input;
mod misc_unknown;
mod openxr_data;
mod overlay;
mod overlayview;
mod rendermodels;
mod screenshots;
mod settings;
mod system;

#[cfg(not(test))]
mod error_dialog;

use clientcore::ClientCore;
use openvr as vr;
use std::ffi::{CStr, CString, c_char, c_void};
use std::sync::OnceLock;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

macro_rules! warn_unimplemented {
    ($function:literal) => {
        crate::warn_once!("{} unimplemented ({}:{})", $function, file!(), line!());
    };
}
use warn_unimplemented;
macro_rules! warn_once {
    ($literal:literal $(,$($tt:tt)*)?) => {{
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            log::warn!(concat!("[ONCE] ", $literal) $(,$($tt)*)?);
        });
    }}
}
use warn_once;

#[cfg(feature = "tracing")]
macro_rules! tracy_span {
    ($($tt:tt)*) => {
        let _span = tracy_client::span!($($tt)*);
    }
}

#[cfg(not(feature = "tracing"))]
macro_rules! tracy_span {
    ($($tt:tt)*) => {};
}
use tracy_span;

#[cfg(feature = "tracing")]
tracy_client::register_demangler!();

macro_rules! atomic_float {
    ($name:ident, $float:ty, $atomic:ty) => {
        #[derive(Default)]
        struct $name($atomic);

        impl $name {
            fn new(value: $float) -> Self {
                Self(value.to_bits().into())
            }

            #[allow(dead_code)]
            #[inline]
            fn load(&self) -> $float {
                <$float>::from_bits(self.0.load(Ordering::Relaxed))
            }

            #[allow(dead_code)]
            #[inline]
            fn store(&self, value: $float) {
                self.0.store(value.to_bits(), Ordering::Relaxed)
            }

            #[allow(dead_code)]
            #[inline]
            fn swap(&self, value: $float) -> $float {
                <$float>::from_bits(self.0.swap(value.to_bits(), Ordering::Relaxed))
            }
        }

        impl From<$float> for $name {
            fn from(value: $float) -> Self {
                Self::new(value)
            }
        }
    };
}

atomic_float!(AtomicF32, f32, AtomicU32);
atomic_float!(AtomicF64, f64, AtomicU64);

fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();

    ONCE.call_once(|| {
        let mut builder = env_logger::Builder::new();
        #[allow(unused_mut)]
        let mut startup_err: Option<String> = None;

        #[cfg(not(test))]
        {
            use std::path::Path;

            struct ComboWriter(std::fs::File, std::io::Stderr);

            impl std::io::Write for ComboWriter {
                fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                    let _ = self.0.write(buf)?;
                    self.1.write(buf)
                }

                fn flush(&mut self) -> std::io::Result<()> {
                    self.0.flush()?;
                    self.1.flush()
                }
            }

            let state_dir = std::env::var("XDG_STATE_HOME")
                .or_else(|_| std::env::var("HOME").map(|h| h + "/.local/state"));

            if let Ok(state) = state_dir {
                let path = Path::new(&state).join("xrizer");
                let mut setup = || {
                    let path = path.join("xrizer.txt");
                    match std::fs::File::create(path) {
                        Ok(file) => {
                            let writer = ComboWriter(file, std::io::stderr());
                            builder.target(env_logger::Target::Pipe(Box::new(writer)));
                        }
                        Err(e) => startup_err = Some(format!("Failed to create log file: {e:?}")),
                    }
                };

                match std::fs::create_dir_all(&path) {
                    Ok(_) => setup(),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => setup(),
                    err => {
                        startup_err = Some(format!(
                            "Failed to create log directory ({path:?}): {err:?}"
                        ))
                    }
                }
            }

            std::panic::set_hook(Box::new(|info| {
                log::error!("{info}");
                let backtrace = std::backtrace::Backtrace::force_capture();
                log::error!("Backtrace: \n{backtrace}");
                error_dialog::dialog(format!("{info}"), backtrace);
                std::process::abort();
            }));
        }

        builder
            .filter_level(log::LevelFilter::Info)
            .parse_default_env()
            .is_test(cfg!(test))
            .format(|buf, record| {
                use std::io::Write;
                use time::macros::format_description;

                let style = buf.default_level_style(record.level());
                let now = time::OffsetDateTime::now_local()
                    .unwrap_or_else(|_| time::OffsetDateTime::now_utc());
                let now = now
                    .format(format_description!(
                        "[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]"
                    ))
                    .unwrap();

                write!(buf, "[{now} {style}{:5}{style:#}", record.level())?;
                if let Some(path) = record.module_path() {
                    write!(buf, " {path}")?;
                }
                writeln!(buf, " {:?}] {}", std::thread::current().id(), record.args())
            })
            .init();

        let version = option_env!("XRIZER_VERSION")
            .or_else(|| {
                option_env!("VERGEN_GIT_DESCRIBE").filter(|v| *v != "VERGEN_IDEMPOTENT_OUTPUT")
            })
            .unwrap_or(env!("CARGO_PKG_VERSION"));
        log::info!("Initializing XRizer version {version}");
        if let Some(err) = startup_err {
            log::warn!("{err}");
        }
    });
}

static OPENVR_API_CORE: OnceLock<Arc<ClientCore>> = OnceLock::new();
static OPENVR_API_INITIALIZED: AtomicBool = AtomicBool::new(false);
static OPENVR_API_TOKEN: AtomicU32 = AtomicU32::new(0);
static OPENVR_RUNTIME_PATH: OnceLock<CString> = OnceLock::new();

fn openvr_api_core() -> Option<&'static Arc<ClientCore>> {
    if OPENVR_API_CORE.get().is_none() {
        let core = ClientCore::new(c"IVRClientCore_003")?;
        let _ = OPENVR_API_CORE.set(core);
    }
    OPENVR_API_CORE.get()
}

fn set_init_error(out: *mut vr::EVRInitError, error: vr::EVRInitError) {
    if let Some(out) = unsafe { out.as_mut() } {
        *out = error;
    }
}

fn runtime_path() -> &'static CStr {
    OPENVR_RUNTIME_PATH
        .get_or_init(|| {
            let path = std::env::current_exe()
                .ok()
                .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            CString::new(path.to_string_lossy().as_bytes())
                .unwrap_or_else(|_| CString::new(".").unwrap())
        })
        .as_c_str()
}

fn init_error_symbol(error: vr::EVRInitError) -> &'static CStr {
    match error {
        vr::EVRInitError::None => c"VRInitError_None",
        vr::EVRInitError::Init_NotInitialized => c"VRInitError_Init_NotInitialized",
        vr::EVRInitError::Init_FactoryNotFound => c"VRInitError_Init_FactoryNotFound",
        vr::EVRInitError::Init_InterfaceNotFound => c"VRInitError_Init_InterfaceNotFound",
        vr::EVRInitError::Init_InvalidInterface => c"VRInitError_Init_InvalidInterface",
        vr::EVRInitError::Init_InvalidApplicationType => c"VRInitError_Init_InvalidApplicationType",
        vr::EVRInitError::Init_VRServiceStartupFailed => c"VRInitError_Init_VRServiceStartupFailed",
        _ => c"VRInitError_Unknown",
    }
}

fn init_error_description(error: vr::EVRInitError) -> &'static CStr {
    match error {
        vr::EVRInitError::None => c"No Error",
        vr::EVRInitError::Init_NotInitialized => c"Not initialized",
        vr::EVRInitError::Init_FactoryNotFound => c"Factory not found",
        vr::EVRInitError::Init_InterfaceNotFound => c"Interface not found",
        vr::EVRInitError::Init_InvalidInterface => c"Invalid interface",
        vr::EVRInitError::Init_InvalidApplicationType => c"Invalid application type",
        vr::EVRInitError::Init_VRServiceStartupFailed => c"VR service startup failed",
        _ => c"Unknown OpenVR initialization error",
    }
}

/// Drop-in openvr_api.dll entry point used by current OpenVR clients.
///
/// # Safety
/// `pe_error` must be null or point to writable memory and `startup_info`
/// must be null or a valid NUL-terminated string for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VR_InitInternal2(
    pe_error: *mut vr::EVRInitError,
    application_type: vr::EVRApplicationType,
    startup_info: *const c_char,
) -> u32 {
    let Some(core) = openvr_api_core() else {
        set_init_error(pe_error, vr::EVRInitError::Init_FactoryNotFound);
        return 0;
    };

    let error = <ClientCore as vr::IVRClientCore003_Interface>::Init(
        core.as_ref(),
        application_type,
        startup_info,
    );
    set_init_error(pe_error, error);
    if error != vr::EVRInitError::None {
        return 0;
    }

    OPENVR_API_INITIALIZED.store(true, Ordering::Release);
    OPENVR_API_TOKEN.fetch_add(1, Ordering::AcqRel) + 1
}

/// # Safety
/// `pe_error` must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VR_InitInternal(
    pe_error: *mut vr::EVRInitError,
    application_type: vr::EVRApplicationType,
) -> u32 {
    unsafe { VR_InitInternal2(pe_error, application_type, std::ptr::null()) }
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_ShutdownInternal() {
    if !OPENVR_API_INITIALIZED.swap(false, Ordering::AcqRel) {
        return;
    }
    if let Some(core) = OPENVR_API_CORE.get() {
        <ClientCore as vr::IVRClientCore003_Interface>::Cleanup(core.as_ref());
    }
    OPENVR_API_TOKEN.fetch_add(1, Ordering::AcqRel);
}

/// # Safety
/// `name_and_version` must point to a valid NUL-terminated string and
/// `pe_error` must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VR_GetGenericInterface(
    name_and_version: *const c_char,
    pe_error: *mut vr::EVRInitError,
) -> *mut c_void {
    if !OPENVR_API_INITIALIZED.load(Ordering::Acquire) {
        set_init_error(pe_error, vr::EVRInitError::Init_NotInitialized);
        return std::ptr::null_mut();
    }
    let Some(core) = OPENVR_API_CORE.get() else {
        set_init_error(pe_error, vr::EVRInitError::Init_NotInitialized);
        return std::ptr::null_mut();
    };

    <ClientCore as vr::IVRClientCore003_Interface>::GetGenericInterface(
        core.as_ref(),
        name_and_version,
        pe_error,
    )
}

/// # Safety
/// `interface_version` must point to a valid NUL-terminated string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VR_IsInterfaceVersionValid(interface_version: *const c_char) -> bool {
    let Some(core) = OPENVR_API_CORE.get() else {
        return false;
    };
    if !OPENVR_API_INITIALIZED.load(Ordering::Acquire) {
        return false;
    }
    <ClientCore as vr::IVRClientCore003_Interface>::IsInterfaceVersionValid(
        core.as_ref(),
        interface_version,
    ) == vr::EVRInitError::None
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_IsHmdPresent() -> bool {
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_IsRuntimeInstalled() -> bool {
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_GetInitToken() -> u32 {
    OPENVR_API_TOKEN.load(Ordering::Acquire)
}

/// # Safety
/// `path_buffer` and `required_buffer_size` must be valid for writes when non-null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VR_GetRuntimePath(
    path_buffer: *mut c_char,
    buffer_size: u32,
    required_buffer_size: *mut u32,
) -> bool {
    let path = runtime_path().to_bytes_with_nul();
    if let Some(required) = unsafe { required_buffer_size.as_mut() } {
        *required = path.len() as u32;
    }
    if path_buffer.is_null() || buffer_size == 0 {
        return true;
    }

    let capacity = buffer_size as usize;
    if capacity < path.len() {
        unsafe { *path_buffer = 0 };
        return true;
    }

    unsafe {
        std::ptr::copy_nonoverlapping(path.as_ptr().cast::<c_char>(), path_buffer, path.len());
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_RuntimePath() -> *const c_char {
    runtime_path().as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_GetVRInitErrorAsSymbol(error: vr::EVRInitError) -> *const c_char {
    init_error_symbol(error).as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_GetVRInitErrorAsEnglishDescription(
    error: vr::EVRInitError,
) -> *const c_char {
    init_error_description(error).as_ptr()
}

#[unsafe(no_mangle)]
pub extern "C" fn VR_GetStringForHmdError(error: vr::EVRInitError) -> *const c_char {
    init_error_description(error).as_ptr()
}

/// # Safety
///
/// interface_name must be valid
#[unsafe(no_mangle)]
pub unsafe extern "C" fn VRClientCoreFactory(
    interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    let interface = unsafe { CStr::from_ptr(interface_name) };

    struct ClientCorePtr(*mut c_void);
    // SAFETY: Vtables are fine to send across threads.
    unsafe impl Send for ClientCorePtr {}
    unsafe impl Sync for ClientCorePtr {}

    static C: OnceLock<ClientCorePtr> = OnceLock::new();
    if C.get().is_none() {
        let ret = ClientCore::new(interface).map(|c| {
            if let Some(ret) = unsafe { return_code.as_mut() } {
                *ret = 0;
            }
            let vtable = match c.base.get().unwrap() {
                clientcore::Vtable::V2(v) => v as *const _ as *const vr::IVRClientCore002 as _,
                clientcore::Vtable::V3(v) => v as *const _ as *const vr::IVRClientCore003 as _,
            };
            // Leak it!
            let _ = Arc::into_raw(c);
            vtable
        });

        if let Some(c) = ret {
            C.set(ClientCorePtr(c)).unwrap_or_else(|_| unreachable!());
        }
    }

    C.get().map(|c| c.0).unwrap_or(std::ptr::null_mut())
}

/// Needed for Proton, but seems unused.
///
/// # Safety
/// `return_code` must be null or point to writable memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn HmdSystemFactory(
    _interface_name: *const c_char,
    return_code: *mut i32,
) -> *mut c_void {
    if let Some(code) = unsafe { return_code.as_mut() } {
        *code = vr::EVRInitError::Init_InterfaceNotFound as i32;
    }
    std::ptr::null_mut()
}
