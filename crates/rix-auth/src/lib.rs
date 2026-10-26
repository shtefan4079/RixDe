use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{
    ffi::{CStr, CString},
    fmt,
    path::PathBuf,
    ptr,
};

pub struct SecretString(String);
impl SecretString {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for SecretString {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SecretString([REDACTED])")
    }
}
impl Drop for SecretString {
    fn drop(&mut self) {
        unsafe {
            wipe(self.0.as_mut_vec());
        }
    }
}
impl Serialize for SecretString {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.expose())
    }
}
impl<'de> Deserialize<'de> for SecretString {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

pub fn wipe(bytes: &mut [u8]) {
    for byte in bytes {
        unsafe {
            ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("Invalid account or PAM input")]
    InvalidInput,
    #[error("Account not found")]
    UnknownUser,
    #[error("Root login is disabled")]
    RootDenied,
    #[error("Authentication or account policy refused access")]
    Rejected,
    #[error("The password has expired; change it from a console login")]
    PasswordExpired,
    #[error("PAM operation failed (code {0})")]
    Pam(i32),
    #[error("Account lookup failed")]
    Lookup,
}

#[derive(Clone, Debug)]
pub struct UnixUser {
    pub username: String,
    pub display_name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
}
pub fn current_user() -> Result<UnixUser, AuthError> {
    user_by_uid(unsafe { libc::getuid() })
}
pub fn user_by_name(name: &str) -> Result<UnixUser, AuthError> {
    let name = CString::new(name).map_err(|_| AuthError::InvalidInput)?;
    lookup(|entry, buffer, length, result| unsafe {
        libc::getpwnam_r(name.as_ptr(), entry, buffer, length, result)
    })
}
pub fn user_by_uid(uid: u32) -> Result<UnixUser, AuthError> {
    lookup(|entry, buffer, length, result| unsafe {
        libc::getpwuid_r(uid, entry, buffer, length, result)
    })
}
fn lookup<F>(mut get: F) -> Result<UnixUser, AuthError>
where
    F: FnMut(*mut libc::passwd, *mut libc::c_char, usize, *mut *mut libc::passwd) -> i32,
{
    let mut size = 16 * 1024;
    loop {
        let mut data = vec![0u8; size];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut result = ptr::null_mut();
        let code = get(
            &mut entry,
            data.as_mut_ptr().cast(),
            data.len(),
            &mut result,
        );
        if code == libc::ERANGE && size < 1024 * 1024 {
            size *= 2;
            continue;
        }
        if code != 0 {
            return Err(AuthError::Lookup);
        }
        if result.is_null() {
            return Err(AuthError::UnknownUser);
        }
        let string = |value: *const libc::c_char| {
            if value.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(value) }
                    .to_string_lossy()
                    .into_owned()
            }
        };
        let username = string(entry.pw_name);
        let display_name = string(entry.pw_gecos)
            .split(',')
            .next()
            .unwrap_or_default()
            .to_owned();
        return Ok(UnixUser {
            username,
            display_name,
            uid: entry.pw_uid,
            gid: entry.pw_gid,
            home: string(entry.pw_dir).into(),
            shell: string(entry.pw_shell).into(),
        });
    }
}

pub fn authenticate_current_user(password: SecretString) -> Result<(), AuthError> {

    let user = current_user()?;
    if user.uid == 0 {
        return Err(AuthError::RootDenied);
    }
    let mut transaction = PamTransaction::start("rix-lock", &user.username, password)?;
    transaction.authenticate()?;
    transaction.account()?;
    if user_by_name(&transaction.canonical_user()?)?.uid != user.uid {
        return Err(AuthError::Rejected);
    }
    Ok(())
}

const PAM_SUCCESS: i32 = 0;
const PAM_CONV_ERR: i32 = 19;
const PAM_NEW_AUTHTOK_REQD: i32 = 12;
const PAM_PROMPT_ECHO_OFF: i32 = 1;
const PAM_PROMPT_ECHO_ON: i32 = 2;
const PAM_ERROR_MSG: i32 = 3;
const PAM_TEXT_INFO: i32 = 4;
const PAM_ESTABLISH_CRED: i32 = 0x2;
const PAM_DELETE_CRED: i32 = 0x4;
pub const PAM_TTY: i32 = 3;
const PAM_USER: i32 = 2;
pub const PAM_RUSER: i32 = 8;

#[repr(C)]
struct PamHandle {
    _opaque: [u8; 0],
}
#[repr(C)]
struct PamMessage {
    style: i32,
    text: *const libc::c_char,
}
#[repr(C)]
struct PamResponse {
    response: *mut libc::c_char,
    code: i32,
}
#[repr(C)]
struct PamConversation {
    callback: Option<
        unsafe extern "C" fn(
            i32,
            *mut *const PamMessage,
            *mut *mut PamResponse,
            *mut libc::c_void,
        ) -> i32,
    >,
    data: *mut libc::c_void,
}
#[link(name = "pam")]
unsafe extern "C" {
    fn pam_start(
        service: *const libc::c_char,
        user: *const libc::c_char,
        conversation: *const PamConversation,
        handle: *mut *mut PamHandle,
    ) -> i32;
    #[cfg(test)]
    fn pam_start_confdir(
        service: *const libc::c_char,
        user: *const libc::c_char,
        conversation: *const PamConversation,
        confdir: *const libc::c_char,
        handle: *mut *mut PamHandle,
    ) -> i32;
    fn pam_end(handle: *mut PamHandle, status: i32) -> i32;
    fn pam_authenticate(handle: *mut PamHandle, flags: i32) -> i32;
    fn pam_acct_mgmt(handle: *mut PamHandle, flags: i32) -> i32;
    fn pam_setcred(handle: *mut PamHandle, flags: i32) -> i32;
    fn pam_open_session(handle: *mut PamHandle, flags: i32) -> i32;
    fn pam_close_session(handle: *mut PamHandle, flags: i32) -> i32;
    fn pam_set_item(handle: *mut PamHandle, item: i32, value: *const libc::c_void) -> i32;
    fn pam_get_item(handle: *const PamHandle, item: i32, value: *mut *const libc::c_void) -> i32;
    fn pam_putenv(handle: *mut PamHandle, value: *const libc::c_char) -> i32;
    fn pam_getenv(handle: *mut PamHandle, name: *const libc::c_char) -> *const libc::c_char;
    fn pam_getenvlist(handle: *mut PamHandle) -> *mut *mut libc::c_char;
}
struct ConversationData {
    username: CString,
    password: SecretString,
    answered: bool,
}

unsafe extern "C" fn converse(
    count: i32,
    messages: *mut *const PamMessage,
    responses: *mut *mut PamResponse,
    opaque: *mut libc::c_void,
) -> i32 {

    if !(1..=32).contains(&count) || messages.is_null() || responses.is_null() || opaque.is_null() {
        return PAM_CONV_ERR;
    }
    *responses = ptr::null_mut();
    let data = &mut *opaque.cast::<ConversationData>();
    let out =
        libc::calloc(count as usize, std::mem::size_of::<PamResponse>()).cast::<PamResponse>();
    if out.is_null() {
        return PAM_CONV_ERR;
    }
    for index in 0..count as usize {
        let message = *messages.add(index);
        if message.is_null() {
            free_responses(out, index);
            return PAM_CONV_ERR;
        }
        let value = match (*message).style {
            PAM_PROMPT_ECHO_OFF if !data.answered => {
                data.answered = true;
                Some(data.password.expose().as_bytes())
            }
            PAM_PROMPT_ECHO_ON => Some(data.username.as_bytes()),
            PAM_ERROR_MSG | PAM_TEXT_INFO => None,
            _ => {
                free_responses(out, index);
                return PAM_CONV_ERR;
            }
        };
        if let Some(value) = value {
            let buffer = libc::malloc(value.len() + 1).cast::<u8>();
            if buffer.is_null() {
                free_responses(out, index);
                return PAM_CONV_ERR;
            }
            ptr::copy_nonoverlapping(value.as_ptr(), buffer, value.len());
            *buffer.add(value.len()) = 0;
            (*out.add(index)).response = buffer.cast();
        }
    }
    *responses = out;
    PAM_SUCCESS
}
unsafe fn free_responses(responses: *mut PamResponse, count: usize) {
    for index in 0..count {
        let value = (*responses.add(index)).response;
        if !value.is_null() {
            wipe(std::slice::from_raw_parts_mut(
                value.cast(),
                libc::strlen(value),
            ));
            libc::free(value.cast());
        }
    }
    libc::free(responses.cast());
}

pub struct PamTransaction {
    handle: *mut PamHandle,
    conversation: Box<ConversationData>,
    credentials: bool,
    opened: bool,
    last_status: i32,
}
impl PamTransaction {
    pub fn start(service: &str, username: &str, password: SecretString) -> Result<Self, AuthError> {
        if password.expose().len() > 4096 || password.expose().contains('\0') {
            return Err(AuthError::InvalidInput);
        }
        let service = CString::new(service).map_err(|_| AuthError::InvalidInput)?;
        let mut conversation = Box::new(ConversationData {
            username: CString::new(username).map_err(|_| AuthError::InvalidInput)?,
            password,
            answered: false,
        });
        let callback = PamConversation {
            callback: Some(converse),
            data: (&mut *conversation as *mut ConversationData).cast(),
        };
        let mut handle = ptr::null_mut();
        let status = unsafe {
            pam_start(
                service.as_ptr(),
                conversation.username.as_ptr(),
                &callback,
                &mut handle,
            )
        };
        if status != PAM_SUCCESS {
            return Err(AuthError::Pam(status));
        }
        Ok(Self {
            handle,
            conversation,
            credentials: false,
            opened: false,
            last_status: PAM_SUCCESS,
        })
    }
    #[cfg(test)]
    fn start_with_config(
        directory: &std::path::Path,
        password: SecretString,
    ) -> Result<Self, AuthError> {
        let service = CString::new("rix-test").unwrap();
        let directory = CString::new(directory.as_os_str().as_encoded_bytes()).unwrap();
        let mut conversation = Box::new(ConversationData {
            username: CString::new("fixture-account").unwrap(),
            password,
            answered: false,
        });
        let callback = PamConversation {
            callback: Some(converse),
            data: (&mut *conversation as *mut ConversationData).cast(),
        };
        let mut handle = ptr::null_mut();
        let status = unsafe {
            pam_start_confdir(
                service.as_ptr(),
                conversation.username.as_ptr(),
                &callback,
                directory.as_ptr(),
                &mut handle,
            )
        };
        if status != PAM_SUCCESS {
            return Err(AuthError::Pam(status));
        }
        Ok(Self {
            handle,
            conversation,
            credentials: false,
            opened: false,
            last_status: PAM_SUCCESS,
        })
    }
    fn check(&mut self, status: i32) -> Result<(), AuthError> {
        self.last_status = status;
        match status {
            PAM_SUCCESS => Ok(()),
            PAM_NEW_AUTHTOK_REQD => Err(AuthError::PasswordExpired),
            _ => Err(AuthError::Pam(status)),
        }
    }
    pub fn authenticate(&mut self) -> Result<(), AuthError> {
        self.conversation.answered = false;
        let status = unsafe { pam_authenticate(self.handle, 0) };

        self.conversation.password = SecretString::new(String::new());
        self.check(status).map_err(|_| AuthError::Rejected)
    }
    pub fn account(&mut self) -> Result<(), AuthError> {
        let status = unsafe { pam_acct_mgmt(self.handle, 0) };
        self.check(status)
    }
    pub fn canonical_user(&self) -> Result<String, AuthError> {
        let mut value = ptr::null();
        let status = unsafe { pam_get_item(self.handle, PAM_USER, &mut value) };
        if status != PAM_SUCCESS || value.is_null() {
            return Err(AuthError::Pam(status));
        }
        Ok(unsafe { CStr::from_ptr(value.cast()) }
            .to_string_lossy()
            .into_owned())
    }
    pub fn set_item(&mut self, item: i32, value: &str) -> Result<(), AuthError> {
        if ![PAM_TTY, PAM_RUSER].contains(&item) {
            return Err(AuthError::InvalidInput);
        }
        let value = CString::new(value).map_err(|_| AuthError::InvalidInput)?;
        let status = unsafe { pam_set_item(self.handle, item, value.as_ptr().cast()) };
        self.check(status)
    }
    pub fn put_env(&mut self, value: &str) -> Result<(), AuthError> {
        let value = CString::new(value).map_err(|_| AuthError::InvalidInput)?;
        let status = unsafe { pam_putenv(self.handle, value.as_ptr()) };
        self.check(status)
    }

    pub fn remove_env(&mut self, name: &str) -> Result<(), AuthError> {
        if name.is_empty() || name.contains('=') {
            return Err(AuthError::InvalidInput);
        }
        let name = CString::new(name).map_err(|_| AuthError::InvalidInput)?;
        if unsafe { pam_getenv(self.handle, name.as_ptr()) }.is_null() {
            return Ok(());
        }
        let status = unsafe { pam_putenv(self.handle, name.as_ptr()) };
        self.check(status)
    }
    pub fn open_session(&mut self) -> Result<(), AuthError> {
        let status = unsafe { pam_setcred(self.handle, PAM_ESTABLISH_CRED) };
        self.check(status)?;
        self.credentials = true;
        self.open_session_stack()
    }

    pub fn open_greeter_session(&mut self) -> Result<(), AuthError> {
        self.open_session_stack()
    }
    fn open_session_stack(&mut self) -> Result<(), AuthError> {

        self.opened = true;
        let status = unsafe { pam_open_session(self.handle, 0) };
        self.check(status)?;
        Ok(())
    }
    pub fn environment(&self) -> Vec<(String, String)> {
        let list = unsafe { pam_getenvlist(self.handle) };
        let mut result = Vec::new();
        if list.is_null() {
            return result;
        }
        unsafe {
            let mut index = 0;
            while !(*list.add(index)).is_null() {
                let entry = *list.add(index);
                if let Some((key, value)) = CStr::from_ptr(entry).to_string_lossy().split_once('=')
                {
                    result.push((key.to_owned(), value.to_owned()));
                }
                libc::free(entry.cast());
                index += 1;
            }
            libc::free(list.cast());
        }
        result
    }
}
impl Drop for PamTransaction {
    fn drop(&mut self) {
        unsafe {
            if self.opened {
                pam_close_session(self.handle, 0);
            }
            if self.credentials {
                pam_setcred(self.handle, PAM_DELETE_CRED);
            }
            pam_end(self.handle, self.last_status);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_redacted_and_buffers_are_wiped() {
        let secret = SecretString::new("never-log-this".into());
        assert!(!format!("{secret:?}").contains(secret.expose()));
        let mut buffer = b"never-log-this".to_vec();
        wipe(&mut buffer);
        assert!(buffer.iter().all(|b| *b == 0));
    }
    #[test]
    fn conversation_refuses_repeated_hidden_challenges() {
        let mut data = ConversationData {
            username: CString::new("test").unwrap(),
            password: SecretString::new("secret".into()),
            answered: false,
        };
        let message = PamMessage {
            style: PAM_PROMPT_ECHO_OFF,
            text: ptr::null(),
        };
        let mut messages = [&message as *const PamMessage; 2];
        let mut out = ptr::null_mut();
        let status = unsafe {
            converse(
                2,
                messages.as_mut_ptr(),
                &mut out,
                (&mut data as *mut ConversationData).cast(),
            )
        };
        assert_eq!(status, PAM_CONV_ERR);
        assert!(out.is_null());
    }
    #[test]
    fn account_lookup_uses_process_identity() {
        let user = current_user().unwrap();
        assert_eq!(user.uid, unsafe { libc::getuid() });
        assert_eq!(user_by_name(&user.username).unwrap().uid, user.uid);
    }
    #[test]
    fn rejects_nul_without_invoking_pam() {
        assert!(matches!(
            PamTransaction::start("rix-lock", "test", SecretString::new("a\0b".into())),
            Err(AuthError::InvalidInput)
        ));
    }
    fn pam_fixture() -> tempfile::TempDir {

        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("fixture.c");
        let module = directory.path().join("fixture.so");
        std::fs::write(&source, include_str!("../tests/pam_fixture.c")).unwrap();
        let status = std::process::Command::new("cc")
            .args(["-shared", "-fPIC", "-Wall", "-Werror"])
            .arg(&source)
            .arg("-o")
            .arg(&module)
            .arg("-lpam")
            .status()
            .unwrap();
        assert!(
            status.success(),
            "PAM fixture requires a C compiler and libpam headers"
        );
        let policy = format!(
            "auth required {}\naccount required {}\nsession required {}\n",
            module.display(),
            module.display(),
            module.display()
        );
        std::fs::write(directory.path().join("rix-test"), policy).unwrap();
        directory
    }
    #[test]
    fn real_pam_fixture_checks_password_and_closes_session() {
        let directory = pam_fixture();
        let trace = directory.path().join("trace");
        let mut wrong = PamTransaction::start_with_config(
            directory.path(),
            SecretString::new("wrong-password".into()),
        )
        .unwrap();
        assert!(wrong.authenticate().is_err());
        drop(wrong);
        let mut pam = PamTransaction::start_with_config(
            directory.path(),
            SecretString::new("fixture-password".into()),
        )
        .unwrap();
        pam.authenticate().unwrap();
        pam.account().unwrap();
        assert_eq!(pam.canonical_user().unwrap(), "fixture-account");
        pam.put_env(&format!("RIX_TEST_TRACE={}", trace.display()))
            .unwrap();
        pam.open_session().unwrap();
        assert!(pam
            .environment()
            .contains(&("XDG_SESSION_ID".into(), "fixture".into())));
        assert_eq!(
            std::fs::read_to_string(&trace).unwrap(),
            "credentials-established\nsession-open\n"
        );
        drop(pam);
        assert_eq!(
            std::fs::read_to_string(&trace).unwrap(),
            "credentials-established\nsession-open\nsession-close\ncredentials-deleted\n"
        );
        std::fs::write(&trace, "").unwrap();
        let mut partial = PamTransaction::start_with_config(
            directory.path(),
            SecretString::new("fixture-password".into()),
        )
        .unwrap();
        partial.authenticate().unwrap();
        partial.account().unwrap();
        partial
            .put_env(&format!("RIX_TEST_TRACE={}", trace.display()))
            .unwrap();
        partial.put_env("RIX_TEST_FAIL_OPEN=1").unwrap();
        assert!(partial.open_session().is_err());

        partial.remove_env("XDG_SESSION_ID").unwrap();
        drop(partial);
        assert_eq!(
            std::fs::read_to_string(&trace).unwrap(),
            "credentials-established\nsession-open\nsession-close\ncredentials-deleted\n"
        );
    }
    #[test]
    fn optional_pam_environment_removal_is_idempotent_and_handles_empty_values() {
        let directory = pam_fixture();
        let mut pam =
            PamTransaction::start_with_config(directory.path(), SecretString::new(String::new()))
                .unwrap();
        pam.remove_env("XDG_SESSION_ID").unwrap();
        pam.put_env("XDG_SESSION_ID=unexpected").unwrap();
        pam.remove_env("XDG_SESSION_ID").unwrap();
        pam.remove_env("XDG_SESSION_ID").unwrap();
        pam.put_env("XDG_SESSION_ID=").unwrap();
        pam.remove_env("XDG_SESSION_ID").unwrap();
        assert!(!pam
            .environment()
            .iter()
            .any(|(key, _)| key == "XDG_SESSION_ID"));
        for invalid in ["", "XDG_SESSION_ID=value", "XDG_SESSION_ID\0"] {
            assert!(matches!(
                pam.remove_env(invalid),
                Err(AuthError::InvalidInput)
            ));
        }
    }
    #[test]
    fn trusted_greeter_opens_session_while_authentication_and_user_credentials_stay_denied() {
        let directory = pam_fixture();
        let module = directory.path().join("fixture.so");
        let trace = directory.path().join("trace");

        std::fs::write(
            directory.path().join("rix-test"),
            format!(
                "auth requisite pam_deny.so\naccount required {}\nsession required {}\n",
                module.display(),
                module.display()
            ),
        )
        .unwrap();
        let mut denied =
            PamTransaction::start_with_config(directory.path(), SecretString::new(String::new()))
                .unwrap();
        assert!(denied.authenticate().is_err());
        assert!(matches!(denied.open_session(), Err(AuthError::Pam(17))));
        denied.remove_env("XDG_SESSION_ID").unwrap();
        assert_eq!(
            denied.last_status, 17,
            "Cleanup must retain the original error"
        );
        drop(denied);

        let mut greeter =
            PamTransaction::start_with_config(directory.path(), SecretString::new(String::new()))
                .unwrap();
        greeter.account().unwrap();
        greeter
            .put_env(&format!("RIX_TEST_TRACE={}", trace.display()))
            .unwrap();
        greeter.open_greeter_session().unwrap();
        assert!(greeter
            .environment()
            .contains(&("XDG_SESSION_ID".into(), "fixture".into())));
        drop(greeter);
        assert_eq!(
            std::fs::read_to_string(trace).unwrap(),
            "session-open\nsession-close\n"
        );
    }
    #[test]
    fn session_failure_before_registration_retains_error_and_runs_session_cleanup() {
        let directory = pam_fixture();
        let trace = directory.path().join("trace");
        let mut pam =
            PamTransaction::start_with_config(directory.path(), SecretString::new(String::new()))
                .unwrap();
        pam.account().unwrap();
        pam.put_env(&format!("RIX_TEST_TRACE={}", trace.display()))
            .unwrap();
        pam.put_env("RIX_TEST_FAIL_OPEN_BEFORE_ID=1").unwrap();
        assert!(matches!(
            pam.open_greeter_session(),
            Err(AuthError::Pam(14))
        ));
        pam.remove_env("XDG_SESSION_ID").unwrap();
        assert_eq!(pam.last_status, 14);
        drop(pam);
        assert_eq!(
            std::fs::read_to_string(trace).unwrap(),
            "session-open\nsession-close\n"
        );
    }
}
