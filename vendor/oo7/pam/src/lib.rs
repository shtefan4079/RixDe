//! PAM module for oo7 Secret Service integration
//!
//! This module intercepts PAM authentication to capture and send user passwords
//! to the oo7 Secret Service daemon, enabling automatic keyring unlocking.

mod ffi;
mod protocol;
mod socket;

use std::{
    ffi::CStr,
    os::raw::{c_char, c_int},
};

use zeroize::Zeroizing;

use crate::{
    ffi::{PAM_AUTHTOK, PAM_AUTHTOK_RECOVER_ERR, PAM_SUCCESS, PAM_SYSTEM_ERR, pam_handle_t},
    protocol::PamMessage,
    socket::send_secret_to_daemon,
};

const STASHED_PASSWORD_KEY: &CStr = c"clavis_stashed_password";

/// Get the username
unsafe fn get_user(pamh: *mut pam_handle_t) -> Result<String, c_int> {
    let mut user_ptr: *const c_char = std::ptr::null();
    let ret =
        unsafe { ffi::pam_get_user(pamh, &mut user_ptr as *mut *const c_char, std::ptr::null()) };

    if ret != PAM_SUCCESS {
        return Err(ret);
    }

    if user_ptr.is_null() {
        return Err(PAM_SYSTEM_ERR);
    }

    let user_cstr = unsafe { CStr::from_ptr(user_ptr) };
    Ok(user_cstr.to_string_lossy().into_owned())
}

/// Get the authentication token from PAM
unsafe fn get_auth_token(pamh: *mut pam_handle_t) -> Result<Zeroizing<Vec<u8>>, c_int> {
    unsafe { get_auth_token_internal(pamh, PAM_AUTHTOK, "PAM_AUTHTOK") }
}

unsafe fn get_auth_token_internal(
    pamh: *mut pam_handle_t,
    item_type: c_int,
    item_name: &str,
) -> Result<Zeroizing<Vec<u8>>, c_int> {
    let mut authtok_ptr: *const std::os::raw::c_void = std::ptr::null();

    let ret = unsafe { ffi::pam_get_item(pamh, item_type, &mut authtok_ptr) };

    if ret != PAM_SUCCESS {
        tracing::debug!("pam_get_item({}) returned error: {}", item_name, ret);
        return Err(ret);
    }

    if authtok_ptr.is_null() {
        tracing::debug!("{} is null (password not available)", item_name);
        return Err(PAM_SYSTEM_ERR);
    }

    // Convert to C string
    let password_cstr = unsafe { CStr::from_ptr(authtok_ptr as *const c_char) };
    let password_bytes = password_cstr.to_bytes().to_vec();

    tracing::debug!("Captured authentication token");

    Ok(Zeroizing::new(password_bytes))
}

/// Get the UID and GID for a username using getpwnam (handles NSS/LDAP/etc)
fn get_user_credentials(username: &str) -> Option<(u32, u32)> {
    use std::ffi::CString;

    let username_c = CString::new(username).ok()?;
    let pwd = unsafe { libc::getpwnam(username_c.as_ptr()) };

    if pwd.is_null() {
        None
    } else {
        Some(unsafe { ((*pwd).pw_uid, (*pwd).pw_gid) })
    }
}

/// Cleanup function for stashed password
#[allow(clippy::missing_safety_doc)]
unsafe extern "C" fn cleanup_password(
    _pamh: *mut pam_handle_t,
    data: *mut std::os::raw::c_void,
    _status: c_int,
) {
    if !data.is_null() {
        let password = unsafe { Box::from_raw(data as *mut Zeroizing<Vec<u8>>) };
        drop(password);
    }
}

/// PAM authentication entry point
#[allow(clippy::missing_safety_doc)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pam_sm_authenticate(
    pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *mut *const c_char,
) -> c_int {
    // Initialize journald logging for PAM module
    // PAM modules should log to journald/syslog, not stdout/stderr
    if let Ok(layer) = tracing_journald::layer() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    let username = match unsafe { get_user(pamh) } {
        Ok(user) => user,
        Err(ret) => {
            tracing::error!("Failed to get username from PAM");
            return ret;
        }
    };

    tracing::debug!("PAM authentication for user: {}", username);

    let password = match unsafe { get_auth_token(pamh) } {
        Ok(pass) => pass,
        Err(_) => {
            tracing::debug!("No auth token available from PAM, skipping stash");
            return PAM_SUCCESS; // Not an error - just means password isn't available yet
        }
    };

    if password.is_empty() {
        tracing::debug!("Auth token is empty, skipping stash");
        return PAM_SUCCESS;
    }

    tracing::debug!("Stashing authentication token for initial login");

    // Stash the password for later use in pam_sm_open_session
    let boxed_password = Box::new(password.clone());
    let password_ptr = Box::into_raw(boxed_password) as *mut std::os::raw::c_void;

    let ret = unsafe {
        ffi::pam_set_data(
            pamh,
            STASHED_PASSWORD_KEY.as_ptr(),
            password_ptr,
            Some(cleanup_password),
        )
    };

    if ret != PAM_SUCCESS {
        tracing::error!("Failed to stash password for session");
        // Clean up the box ourselves since pam_set_data failed
        unsafe {
            let _ = Box::from_raw(password_ptr as *mut Zeroizing<Vec<u8>>);
        }
        return PAM_AUTHTOK_RECOVER_ERR;
    }

    tracing::info!("Stashed password to try later in open session");
    PAM_SUCCESS
}

/// PAM credential setting entry point
#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_setcred(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *mut *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// PAM account management entry point
#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_acct_mgmt(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *mut *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// PAM session opening entry point
#[unsafe(no_mangle)]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn pam_sm_open_session(
    pamh: *mut pam_handle_t,
    _flags: c_int,
    argc: c_int,
    argv: *mut *const c_char,
) -> c_int {
    // Initialize journald logging for PAM module
    // PAM modules should log to journald/syslog, not stdout/stderr
    if let Ok(layer) = tracing_journald::layer() {
        use tracing_subscriber::layer::SubscriberExt;
        let subscriber = tracing_subscriber::registry().with(layer);
        let _ = tracing::subscriber::set_global_default(subscriber);
    }

    // Parse arguments to check for auto_start
    let mut auto_start = false;
    for i in 0..argc {
        let arg_ptr = unsafe { *argv.offset(i as isize) };
        if !arg_ptr.is_null() {
            let arg_cstr = unsafe { CStr::from_ptr(arg_ptr) };
            if let Ok(arg_str) = arg_cstr.to_str()
                && arg_str == "auto_start"
            {
                auto_start = true;
                tracing::debug!("auto_start argument detected");
            }
        }
    }

    let mut password_ptr: *const std::os::raw::c_void = std::ptr::null();
    let ret = unsafe { ffi::pam_get_data(pamh, STASHED_PASSWORD_KEY.as_ptr(), &mut password_ptr) };

    if ret != PAM_SUCCESS || password_ptr.is_null() {
        tracing::debug!("No stashed password found in session");
        return PAM_SUCCESS;
    }

    let password = unsafe { &*(password_ptr as *const Zeroizing<Vec<u8>>) };
    tracing::debug!("Retrieved initial login authentication token");

    // Get username and UID
    let username = match unsafe { get_user(pamh) } {
        Ok(user) => user,
        Err(_) => {
            tracing::error!("Failed to get username in session");
            return PAM_SUCCESS;
        }
    };

    let (user_uid, user_gid) = match get_user_credentials(&username) {
        Some(creds) => creds,
        None => {
            tracing::error!("Failed to get credentials for user: {}", username);
            return PAM_SUCCESS;
        }
    };

    let message = PamMessage::unlock(username.clone(), password.to_vec());

    // The optional session hook waits at most five seconds for the helper's
    // private activation socket, so the desktop cannot race ahead of it.
    match send_secret_to_daemon(message, user_uid, user_gid, auto_start) {
        Ok(_) => {
            tracing::info!(
                "Clavis initial login helper is ready for user: {}",
                username
            );
        }
        Err(error) => tracing::warn!("Clavis automatic unlock unavailable: {error}"),
    }

    PAM_SUCCESS
}

/// PAM session closing entry point
#[unsafe(no_mangle)]
pub extern "C" fn pam_sm_close_session(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *mut *const c_char,
) -> c_int {
    PAM_SUCCESS
}

/// PAM password change entry point
#[unsafe(no_mangle)]
#[allow(clippy::missing_safety_doc)]
pub unsafe extern "C" fn pam_sm_chauthtok(
    _pamh: *mut pam_handle_t,
    _flags: c_int,
    _argc: c_int,
    _argv: *mut *const c_char,
) -> c_int {
    // No password is read, copied, logged or sent by this optional account
    // password hook. Change the vault password through Clavis itself.
    PAM_SUCCESS
}

#[cfg(test)]
mod tests {
    #[test]
    fn optional_password_change_never_reads_pam_secrets() {
        // Even an absent handle is safe: this hook reads no old/new token.
        let result =
            unsafe { super::pam_sm_chauthtok(std::ptr::null_mut(), -1, 0, std::ptr::null_mut()) };
        assert_eq!(result, super::PAM_SUCCESS);
    }
}
