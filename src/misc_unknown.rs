// The interfaces in this file are missing in openvr.h and any other form of OpenVR documentation,
// but are used by games (typically Half Life Alyx.)

use log::{debug, trace};
use openvr::InterfaceImpl;
use seq_macro::seq;
use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct UnknownInterfaces {
    mailbox: Wrapper<Mailbox>,
    control_panel: Wrapper<ControlPanel>,
}

impl InterfaceImpl for UnknownInterfaces {
    fn supported_versions() -> &'static [&'static CStr] {
        &[c"IVRMailbox_001", c"IVRControlPanel_006"]
    }

    fn get_version(version: &CStr) -> Option<Box<dyn FnOnce(&Arc<Self>) -> *mut c_void>> {
        #[allow(
            clippy::redundant_guards,
            reason = "https://github.com/rust-lang/rust-clippy/issues/13681"
        )]
        match version {
            x if x == c"IVRMailbox_001" => Some(Box::new(|this| &this.mailbox as *const _ as _)),
            x if x == c"IVRControlPanel_006" => {
                Some(Box::new(|this| &this.control_panel as *const _ as _))
            }
            _ => None,
        }
    }
}

/// Wraps our unknown vtables.
#[repr(C)]
struct Wrapper<T: 'static> {
    vtable: &'static T,
}

impl<T> Default for Wrapper<T>
where
    T: 'static,
    &'static T: Default,
{
    fn default() -> Self {
        Self {
            vtable: Default::default(),
        }
    }
}

const UNKNOWN_TAG: &str = "unknown_interfaces";

macro_rules! gen_vtable {
    (struct $name:ident {
        $(
            fn $fn_name:ident($($arg:ident: $ty:ty),*$(,)?) $(-> $output:ty)? {$($tt:tt)*}
        )+
    }) => {
        #[repr(C)]
        struct $name {
            $(
                $fn_name: extern "C" fn(*mut Wrapper<$name> $(,$ty)*) $(-> $output)?
            ),*
        }

        impl $name {
            $(
            extern "C" fn $fn_name(_: *mut Wrapper<$name> $(,$arg:$ty)*) $(-> $output)? {$($tt)*}
            )*
        }

        impl Default for &'static $name {
            fn default() -> Self {
                &$name {
                    $(
                        $fn_name: $name::$fn_name
                    ),*
                }
            }
        }
    }
}

#[repr(transparent)]
#[derive(Debug)]
struct MailboxHandle(u64);

#[derive(Default)]
struct MailboxState {
    ready_delivered: bool,
}

impl MailboxState {
    // Size queries and failed copies must not consume the pending message.
    unsafe fn read(&mut self, out: *mut c_char, capacity: u32, length: *mut u32) -> c_int {
        if length.is_null() {
            return 2;
        }
        unsafe {
            *length = 0;
        }
        if self.ready_delivered {
            return 1;
        }
        let message = c"{ \"type\": \"ready\", }";
        let payload = message.to_bytes().len() as u32;
        unsafe {
            *length = payload;
        }
        if out.is_null() || capacity <= payload {
            return 2;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(message.as_ptr(), out, payload as usize + 1);
        }
        self.ready_delivered = true;
        0
    }
}

static MAILBOX: Mutex<MailboxState> = Mutex::new(MailboxState {
    ready_delivered: false,
});

// IVRMailbox - used by Alyx, only seems to be documnted here:
// https://github.com/ValveSoftware/Proton/blob/proton_9.0/vrclient_x64/openvr_v1.10.30/openvr.h#L5108
// This implementation is adapted from OpenComposite:
// https://gitlab.com/znixian/OpenOVR/-/blob/34311dabf430d6051d7e97f6081842a5394d2a67/OpenOVR/Reimpl/BaseMailbox.cpp
gen_vtable! {
    struct Mailbox {
        fn undoc1(a: *const c_char, b: *mut MailboxHandle) -> c_int {
            let a = unsafe { CStr::from_ptr(a) };
            debug!(target: UNKNOWN_TAG, "Entered IVRMailbox::undoc1 with arguments a: {a:?}, b: {b:?}");
            if b.is_null() { return 2; }
            *MAILBOX.lock().unwrap() = MailboxState::default();
            unsafe { *b = MailboxHandle(24) };
            0
        }
        fn undoc2(handle: MailboxHandle) -> c_int {
            debug!(target: UNKNOWN_TAG, "Entered IVRMailbox::undoc2 with arugments handle: {handle:?}");
            trace!("[alyx-mailbox] UnregisterMailbox handle={handle:?}");
            *MAILBOX.lock().unwrap() = MailboxState::default();
            0
        }
        fn undoc3(
            handle: MailboxHandle,
            a: *const c_char,
            b: *const c_char,
        ) -> c_int {
            let a = if a.is_null() {
                None
            } else {
                Some(unsafe { CStr::from_ptr(a) })
            };
            let b = if b.is_null() {
                None
            } else {
                Some(unsafe { CStr::from_ptr(b) })
            };
            debug!(target: UNKNOWN_TAG, "Entered IVRMailbox::undoc3 with arguments handle: {handle:?}, a: {a:?}, b: {b:?}");
            trace!("[alyx-mailbox] SendMessage handle={handle:?} type={a:?} message={b:?}");
            0
        }
        // Borrowed from OpenComposite
        fn undoc4(
            handle: MailboxHandle,
            out_buf: *mut c_char,
            out_len: u32,
            len: *mut u32,
        ) -> c_int {
            debug!(target: UNKNOWN_TAG, "Entered IVRMailbox::undoc4 with arguments handle: {handle:?}, out_buf: {out_buf:?}, out_len: {out_len:?}, c: {len:?}");

            let result = unsafe { MAILBOX.lock().unwrap().read(out_buf, out_len, len) };
            trace!("[alyx-mailbox] ReadMessage handle={handle:?} result={result}");
            result
        }
    }
}

// IVRControlPanel - used by Alyx.
// Without this interface, Alyx complains about "No response from Mongoose".
seq!(N in 0..=25 {
    gen_vtable!(struct ControlPanel {
        #(fn unknown_func_~N() {
            debug!(target: UNKNOWN_TAG, "Entered ControlPanel::unknown_func_{}", stringify!(N));
        })*
        // This function gets called at startup
        fn unknown_func_26() -> c_int {
            debug!(target: UNKNOWN_TAG, "Entered ControlPanel::unknown_func_26");
            0
        }
    });
});

#[cfg(test)]
mod mailbox_tests {
    use super::*;
    #[test]
    fn size_query_and_short_buffer_preserve_ready() {
        let mut mailbox = MailboxState::default();
        let mut length = 0;
        assert_eq!(
            unsafe { mailbox.read(std::ptr::null_mut(), 0, &mut length) },
            2
        );
        let required = length;
        let mut buffer = vec![0 as c_char; required as usize + 1];
        assert_eq!(
            unsafe { mailbox.read(buffer.as_mut_ptr(), required, &mut length) },
            2
        );
        assert_eq!(
            unsafe { mailbox.read(buffer.as_mut_ptr(), required + 1, &mut length) },
            0
        );
        assert_eq!(length, required);
        assert_eq!(
            unsafe { CStr::from_ptr(buffer.as_ptr()) },
            c"{ \"type\": \"ready\", }"
        );
        assert_eq!(
            unsafe { mailbox.read(buffer.as_mut_ptr(), required + 1, &mut length) },
            1
        );
        assert_eq!(length, 0);
    }
    #[test]
    fn invalid_length_does_not_consume_ready() {
        let mut mailbox = MailboxState::default();
        let mut buffer = [0 as c_char; 64];
        assert_eq!(
            unsafe { mailbox.read(buffer.as_mut_ptr(), 64, std::ptr::null_mut()) },
            2
        );
        assert!(!mailbox.ready_delivered);
    }
}
