//! Minimal Linux-PAM FFI: one blocking call that answers "are these
//! credentials valid for this service?".
//!
//! In-tree because the published pam crates all shell out to bindgen
//! (libclang) at build time; the four libpam entry points we need have had a
//! stable ABI for decades and are declared by hand instead. Linux-PAM only —
//! fine, Lumen is a Linux appliance.

use std::ffi::{c_char, c_int, c_void, CString};

// ── Linux-PAM ABI (security/pam_appl.h) ─────────────────────────────────────

const PAM_SUCCESS: c_int = 0;
const PAM_BUF_ERR: c_int = 5;
const PAM_CONV_ERR: c_int = 19;
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
const PAM_TEXT_INFO: c_int = 4;

#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

#[repr(C)]
struct PamConv {
    conv: extern "C" fn(
        num_msg: c_int,
        msg: *mut *const PamMessage,
        resp: *mut *mut PamResponse,
        appdata_ptr: *mut c_void,
    ) -> c_int,
    appdata_ptr: *mut c_void,
}

// PamHandle is opaque.
#[link(name = "pam")]
extern "C" {
    fn pam_start(
        service: *const c_char,
        user: *const c_char,
        conversation: *const PamConv,
        handle: *mut *mut c_void,
    ) -> c_int;
    fn pam_authenticate(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_acct_mgmt(handle: *mut c_void, flags: c_int) -> c_int;
    fn pam_end(handle: *mut c_void, status: c_int) -> c_int;
}

// ── Conversation ────────────────────────────────────────────────────────────

/// What the conversation hands to PAM's prompts. NUL-free by construction
/// (callers validate); echo-on prompts get the username, echo-off the
/// password.
struct Credentials {
    username: CString,
    password: CString,
}

/// The conversation callback libpam invokes (possibly several times) during
/// authenticate/acct_mgmt. Responses are malloc'd because libpam frees them
/// with free(3).
extern "C" fn conversation(
    num_msg: c_int,
    msg: *mut *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }
    let creds = unsafe { &*(appdata_ptr as *const Credentials) };
    let responses = unsafe {
        libc::calloc(num_msg as usize, std::mem::size_of::<PamResponse>()) as *mut PamResponse
    };
    if responses.is_null() {
        return PAM_BUF_ERR;
    }
    for i in 0..num_msg as usize {
        let message = unsafe { *msg.add(i) };
        if message.is_null() {
            unsafe { free_responses(responses, num_msg) };
            return PAM_CONV_ERR;
        }
        let answer = match unsafe { (*message).msg_style } {
            PAM_PROMPT_ECHO_OFF => creds.password.as_ptr(),
            PAM_PROMPT_ECHO_ON => creds.username.as_ptr(),
            // Informational output has no answer; the calloc'd null resp is
            // exactly what PAM expects.
            PAM_ERROR_MSG | PAM_TEXT_INFO => continue,
            _ => {
                unsafe { free_responses(responses, num_msg) };
                return PAM_CONV_ERR;
            }
        };
        let dup = unsafe { libc::strdup(answer) };
        if dup.is_null() {
            unsafe { free_responses(responses, num_msg) };
            return PAM_BUF_ERR;
        }
        unsafe { (*responses.add(i)).resp = dup };
    }
    unsafe { *resp = responses };
    PAM_SUCCESS
}

/// Free a partially/fully built response array on the error paths (on
/// success libpam owns it).
unsafe fn free_responses(responses: *mut PamResponse, num_msg: c_int) {
    for i in 0..num_msg as usize {
        let r = (*responses.add(i)).resp;
        if !r.is_null() {
            libc::free(r as *mut c_void);
        }
    }
    libc::free(responses as *mut c_void);
}

// ── Public entry point ──────────────────────────────────────────────────────

/// Why authentication didn't succeed.
#[derive(Debug)]
pub enum PamError {
    /// PAM ran and said no (wrong password, unknown/locked account, …).
    Denied,
    /// PAM itself couldn't run (bad service configuration, interior NUL, …).
    Setup(String),
}

/// Blocking: authenticate `username`/`password` against the given PAM
/// service (pam_authenticate + pam_acct_mgmt). Call from a blocking-safe
/// context — PAM modules may sleep on failure.
pub fn authenticate(service: &str, username: &str, password: &str) -> Result<(), PamError> {
    let service_c =
        CString::new(service).map_err(|_| PamError::Setup("NUL in service name".into()))?;
    let creds = Box::new(Credentials {
        username: CString::new(username).map_err(|_| PamError::Setup("NUL in username".into()))?,
        password: CString::new(password).map_err(|_| PamError::Setup("NUL in password".into()))?,
    });
    let conv = PamConv {
        conv: conversation,
        appdata_ptr: &*creds as *const Credentials as *mut c_void,
    };

    let mut handle: *mut c_void = std::ptr::null_mut();
    let rc = unsafe {
        pam_start(
            service_c.as_ptr(),
            creds.username.as_ptr(),
            &conv,
            &mut handle,
        )
    };
    if rc != PAM_SUCCESS || handle.is_null() {
        return Err(PamError::Setup(format!("pam_start failed (rc {rc})")));
    }

    let rc = unsafe { pam_authenticate(handle, 0) };
    let rc = if rc == PAM_SUCCESS {
        unsafe { pam_acct_mgmt(handle, 0) }
    } else {
        rc
    };
    unsafe { pam_end(handle, rc) };
    // `creds` (and the CStrings PAM saw) stay alive until here.
    drop(creds);

    if rc == PAM_SUCCESS {
        Ok(())
    } else {
        Err(PamError::Denied)
    }
}
