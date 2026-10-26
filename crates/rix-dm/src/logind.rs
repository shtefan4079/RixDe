use anyhow::{ensure, Context, Result};
use std::{
    ffi::{CStr, CString},
    io, ptr,
};

#[link(name = "systemd")]
unsafe extern "C" {
    fn sd_pid_get_session(pid: libc::pid_t, session: *mut *mut libc::c_char) -> libc::c_int;
    fn sd_pid_get_unit(pid: libc::pid_t, unit: *mut *mut libc::c_char) -> libc::c_int;
    fn sd_session_get_uid(session: *const libc::c_char, uid: *mut libc::uid_t) -> libc::c_int;
    fn sd_session_get_leader(session: *const libc::c_char, leader: *mut libc::pid_t)
        -> libc::c_int;
    fn sd_session_get_seat(
        session: *const libc::c_char,
        seat: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sd_session_get_vt(session: *const libc::c_char, vt: *mut libc::c_uint) -> libc::c_int;
    fn sd_session_get_class(
        session: *const libc::c_char,
        class: *mut *mut libc::c_char,
    ) -> libc::c_int;
    fn sd_session_get_type(
        session: *const libc::c_char,
        kind: *mut *mut libc::c_char,
    ) -> libc::c_int;
}

struct SessionIdentity {
    id: String,
    uid: u32,
    leader: i32,
    seat: String,
    vt: u32,
    class: String,
    kind: String,
}

fn result(status: i32) -> Result<()> {
    if status < 0 {
        return Err(io::Error::from_raw_os_error(-status).into());
    }
    Ok(())
}
fn string(call: impl FnOnce(*mut *mut libc::c_char) -> i32) -> Result<String> {
    let mut value = ptr::null_mut();
    let status = call(&mut value);
    if status < 0 {
        return Err(io::Error::from_raw_os_error(-status).into());
    }
    ensure!(!value.is_null(), "logind returned an empty identity field");
    let text = unsafe { CStr::from_ptr(value) }.to_str().map(str::to_owned);
    unsafe {
        libc::free(value.cast());
    }
    Ok(text?)
}
fn identity(pid: i32) -> Result<SessionIdentity> {
    let id = string(|out| unsafe { sd_pid_get_session(pid, out) })
        .context("Worker has no logind session")?;
    let session = CString::new(id.as_str())?;
    let mut uid = 0;
    let mut leader = 0;
    let mut vt = 0;
    result(unsafe { sd_session_get_uid(session.as_ptr(), &mut uid) })?;
    result(unsafe { sd_session_get_leader(session.as_ptr(), &mut leader) })?;
    result(unsafe { sd_session_get_vt(session.as_ptr(), &mut vt) })?;
    Ok(SessionIdentity {
        id,
        uid,
        leader,
        vt,
        seat: string(|out| unsafe { sd_session_get_seat(session.as_ptr(), out) })?,
        class: string(|out| unsafe { sd_session_get_class(session.as_ptr(), out) })?,
        kind: string(|out| unsafe { sd_session_get_type(session.as_ptr(), out) })?,
    })
}
fn validate(
    identity: SessionIdentity,
    expected_uid: u32,
    expected_pid: i32,
    expected_vt: u16,
    expected_class: &str,
) -> Result<String> {
    ensure!(
        super::valid_name(&identity.id)
            && identity
                .id
                .as_bytes()
                .first()
                .is_some_and(u8::is_ascii_alphanumeric),
        "logind returned an invalid session identifier"
    );
    ensure!(
        identity.uid == expected_uid,
        "logind session belongs to another account"
    );
    ensure!(
        identity.leader == expected_pid,
        "Worker does not own the logind session"
    );
    ensure!(
        identity.seat == "seat0",
        "logind session belongs to another seat"
    );
    ensure!(
        identity.vt == expected_vt as u32,
        "logind session belongs to another VT"
    );
    ensure!(
        identity.class == expected_class,
        "Unexpected logind session class"
    );
    ensure!(identity.kind == "wayland", "Unexpected logind session type");
    Ok(identity.id)
}
pub(super) fn verified_session(
    expected_uid: u32,
    expected_pid: i32,
    expected_vt: u16,
    expected_class: &str,
) -> Result<String> {
    validate(
        identity(expected_pid)?,
        expected_uid,
        expected_pid,
        expected_vt,
        expected_class,
    )
}

pub(super) fn verified_session_scope(
    expected_uid: u32,
    expected_pid: i32,
    expected_vt: u16,
    expected_class: &str,
    expected_session: &str,
) -> Result<String> {
    let id = verified_session(expected_uid, expected_pid, expected_vt, expected_class)?;
    ensure!(id == expected_session, "Worker's logind identity changed");
    let scope = string(|out| unsafe { sd_pid_get_unit(expected_pid, out) })?;
    validate_scope(&id, scope)
}

fn validate_scope(id: &str, scope: String) -> Result<String> {
    ensure!(
        super::valid_name(id) && id.as_bytes().first().is_some_and(u8::is_ascii_alphanumeric),
        "Invalid logind session identifier"
    );

    let escaped = id.replace('-', "\\x2d");
    ensure!(
        scope == format!("session-{escaped}.scope"),
        "Worker is outside its verified logind session scope"
    );
    Ok(scope)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> SessionIdentity {
        SessionIdentity {
            id: "fixture".into(),
            uid: 1000,
            leader: 4242,
            seat: "seat0".into(),
            vt: 1,
            class: "user".into(),
            kind: "wayland".into(),
        }
    }
    #[test]
    fn accepts_only_owned_configured_wayland_session() {
        assert_eq!(
            validate(fixture(), 1000, 4242, 1, "user").unwrap(),
            "fixture"
        );
    }
    #[test]
    fn rejects_each_foreign_or_wrong_session_field() {
        let mut changed = fixture();
        changed.uid = 1001;
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.leader = 42;
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.seat = "seat1".into();
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.vt = 2;
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.class = "greeter".into();
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.kind = "tty".into();
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
        let mut changed = fixture();
        changed.id = "--all".into();
        assert!(validate(changed, 1000, 4242, 1, "user").is_err());
    }

    #[test]
    fn resource_changes_require_exact_owned_scope() {
        assert_eq!(
            validate_scope("42", "session-42.scope".into()).unwrap(),
            "session-42.scope"
        );
        assert!(validate_scope("42", "session-43.scope".into()).is_err());
        assert!(validate_scope("42", "user@1000.service".into()).is_err());
        assert!(validate_scope("42", "session-42.scope CPUWeight=10000".into()).is_err());
        assert!(validate_scope("--all", "session---all.scope".into()).is_err());
        assert_eq!(
            validate_scope(
                "fixture-session",
                "session-fixture\\x2dsession.scope".into()
            )
            .unwrap(),
            "session-fixture\\x2dsession.scope"
        );
    }
}
