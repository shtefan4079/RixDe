use super::Settings;
use anyhow::{bail, Context, Result};
use gio::glib::{
    self,
    translate::{from_glib_full, ToGlibPtr},
};
use std::{
    ffi::{c_char, c_void, CString},
    io::Read,
    os::unix::{ffi::OsStrExt, fs::OpenOptionsExt},
    path::Path,
    sync::OnceLock,
};
use zbus::zvariant::{Array, Dict, Signature, StructureBuilder, Value};

type KeyfileRead = unsafe extern "C" fn(
    *mut glib::ffi::GKeyFile,
    *const c_char,
    u32,
    *const c_void,
    *mut c_void,
    *mut *mut glib::ffi::GError,
) -> *mut c_void;
type Normalize =
    unsafe extern "C" fn(*mut c_void, *mut c_void, *mut i32, *mut *mut glib::ffi::GError) -> i32;
type ToDbus = unsafe extern "C" fn(*mut c_void, u32) -> *mut glib::ffi::GVariant;

struct Library(usize);
static LIBRARY: OnceLock<Option<Library>> = OnceLock::new();
impl Library {
    fn open() -> Result<&'static Self> {

        LIBRARY
            .get_or_init(|| {
                let handle = unsafe {
                    libc::dlopen(c"libnm.so.0".as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL)
                };
                (!handle.is_null()).then_some(Self(handle as usize))
            })
            .as_ref()
            .context("Install libnm to import NetworkManager connection files")
    }
    unsafe fn symbol<T: Copy>(&self, name: &std::ffi::CStr) -> Result<T> {

        let pointer = unsafe { libc::dlsym(self.0 as *mut c_void, name.as_ptr()) };
        anyhow::ensure!(
            !pointer.is_null(),
            "NetworkManager keyfile import requires libnm 1.30 or newer"
        );
        Ok(unsafe { std::mem::transmute_copy(&pointer) })
    }
}
struct ConnectionObject(*mut c_void);
impl Drop for ConnectionObject {
    fn drop(&mut self) {

        unsafe {
            glib::gobject_ffi::g_object_unref(self.0.cast());
        }
    }
}
fn discard_error(error: *mut glib::ffi::GError) {
    if !error.is_null() {

        unsafe {
            glib::ffi::g_error_free(error);
        }
    }
}

pub(super) fn read(path: &str) -> Result<Settings> {
    let path = Path::new(path)
        .canonicalize()
        .context("The import file could not be read")?;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .context("The import file could not be read")?;
    let metadata = file.metadata()?;
    anyhow::ensure!(
        metadata.is_file() && metadata.len() <= 1024 * 1024,
        "The import file must be a regular file no larger than 1 MiB"
    );
    let mut bytes = Vec::new();
    file.take(1024 * 1024 + 1).read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "The import file must be a regular file no larger than 1 MiB"
    );
    let data =
        std::str::from_utf8(&bytes).context("NetworkManager connection files must use UTF-8")?;
    let library = Library::open()?;
    let parser: KeyfileRead = unsafe { library.symbol(c"nm_keyfile_read")? };
    let normalize: Normalize = unsafe { library.symbol(c"nm_connection_normalize")? };
    let serialize: ToDbus = unsafe { library.symbol(c"nm_connection_to_dbus")? };
    let keyfile = glib::KeyFile::new();
    if keyfile
        .load_from_data(data, glib::KeyFileFlags::NONE)
        .is_err()
    {
        bail!("The NetworkManager connection file is not valid");
    }
    let base = CString::new(
        path.parent()
            .context("The import path has no parent")?
            .as_os_str()
            .as_bytes(),
    )?;
    let mut error = std::ptr::null_mut();

    let pointer = unsafe {
        parser(
            keyfile.to_glib_none().0,
            base.as_ptr(),
            0,
            std::ptr::null(),
            std::ptr::null_mut(),
            &mut error,
        )
    };
    discard_error(error);
    anyhow::ensure!(
        !pointer.is_null(),
        "The NetworkManager connection file is not valid"
    );
    let connection = ConnectionObject(pointer);
    error = std::ptr::null_mut();

    let valid = unsafe {
        normalize(
            connection.0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut error,
        )
    };
    discard_error(error);
    anyhow::ensure!(
        valid != 0,
        "The NetworkManager connection settings are not valid"
    );

    let pointer = unsafe { serialize(connection.0, 0) };
    anyhow::ensure!(
        !pointer.is_null(),
        "Could not convert the imported connection settings"
    );

    let variant: glib::Variant = unsafe { from_glib_full(glib::ffi::g_variant_take_ref(pointer)) };
    let mut settings = Settings::new();
    for entry in variant.iter() {
        let group = entry
            .child_value(0)
            .str()
            .context("Invalid imported setting name")?
            .to_owned();
        let mut fields = std::collections::HashMap::new();
        for field in entry.child_value(1).iter() {
            let name = field
                .child_value(0)
                .str()
                .context("Invalid imported property name")?
                .to_owned();
            let wrapped = field.child_value(1);
            let value = wrapped
                .as_variant()
                .context("Invalid imported property value")?;
            fields.insert(name, convert(&value, 0)?.try_to_owned()?);
        }
        settings.insert(group, fields);
    }
    Ok(settings)
}

fn dbus_signature(value: &str) -> Result<Signature> {
    Signature::try_from(value).map_err(|_| anyhow::anyhow!("Invalid imported signature"))
}

fn convert(value: &glib::Variant, depth: u8) -> Result<Value<'static>> {
    anyhow::ensure!(
        depth < 16,
        "Imported connection settings are nested too deeply"
    );
    let signature = value.type_().as_str();
    macro_rules! primitive {
        ($type:ty) => {
            Value::from(
                value
                    .get::<$type>()
                    .context("Invalid imported property value")?,
            )
        };
    }
    let result = match signature {
        "b" => primitive!(bool),
        "y" => primitive!(u8),
        "n" => primitive!(i16),
        "q" => primitive!(u16),
        "i" => primitive!(i32),
        "u" => primitive!(u32),
        "x" => primitive!(i64),
        "t" => primitive!(u64),
        "d" => primitive!(f64),
        "s" => Value::from(
            value
                .str()
                .context("Invalid imported property value")?
                .to_owned(),
        ),
        "o" => Value::from(zbus::zvariant::OwnedObjectPath::try_from(
            value.str().context("Invalid imported object path")?,
        )?),
        "g" => Value::from(dbus_signature(
            value.str().context("Invalid imported signature")?,
        )?),
        "v" => Value::Value(Box::new(convert(
            &value
                .as_variant()
                .context("Invalid imported property value")?,
            depth + 1,
        )?)),
        _ if signature.starts_with("a{") => {
            let entry_type = value.type_().element();
            let key = dbus_signature(entry_type.key().as_str())?;
            let field = dbus_signature(entry_type.value().as_str())?;
            let mut dictionary = Dict::new(&key, &field);
            for entry in value.iter() {
                dictionary.append(
                    convert(&entry.child_value(0), depth + 1)?,
                    convert(&entry.child_value(1), depth + 1)?,
                )?;
            }
            Value::Dict(dictionary)
        }
        _ if signature.starts_with('a') => {
            let element = dbus_signature(value.type_().element().as_str())?;
            let mut array = Array::new(&element);
            for entry in value.iter() {
                array.append(convert(&entry, depth + 1)?)?;
            }
            Value::Array(array)
        }
        _ if signature.starts_with('(') => {
            let mut structure = StructureBuilder::new();
            for field in value.iter() {
                structure = structure.add_field(convert(&field, depth + 1)?);
            }
            Value::Structure(structure.build()?)
        }
        _ => bail!("The imported connection contains an unsupported property type"),
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gio::glib::variant::ToVariant;
    use zbus::zvariant::OwnedValue;
    #[test]
    fn import_rejects_a_fifo_without_waiting_for_a_writer() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-keyfile");
        let name = CString::new(file.as_os_str().as_bytes()).unwrap();

        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(read(file.to_str().unwrap()).is_err());
    }
    #[test]
    fn nested_typed_values_survive_import() {
        let mut peer = std::collections::HashMap::<String, glib::Variant>::new();
        peer.insert("public-key".into(), "fixture-only".to_variant());
        peer.insert("persistent-keepalive".into(), 25u32.to_variant());
        let peers = vec![peer].to_variant();
        let converted = convert(&peers, 0).unwrap().try_to_owned().unwrap();
        let values: Vec<std::collections::HashMap<String, OwnedValue>> =
            converted.try_into().unwrap();
        assert_eq!(
            u32::try_from(&values[0]["persistent-keepalive"]).unwrap(),
            25
        );
        assert_eq!(
            <&str>::try_from(&values[0]["public-key"]).unwrap(),
            "fixture-only"
        );
    }
    #[test]
    fn native_keyfile_import_uses_schema_and_never_contacts_network_manager() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("fixture.nmconnection");
        std::fs::write(&file, "[connection]\nid=Private import fixture\nuuid=4f2d6fa4-ae83-40ca-943b-1c18f4cb019a\ntype=wifi\n[ wifi ]\n").unwrap();

        assert!(read(file.to_str().unwrap()).is_err());
        std::fs::write(&file, "[connection]\nid=Private import fixture\nuuid=4f2d6fa4-ae83-40ca-943b-1c18f4cb019a\ntype=wifi\n[wifi]\nssid=Fixture\nmode=infrastructure\n[wifi-security]\nkey-mgmt=wpa-psk\npsk=synthetic-import-only\n[ipv4]\nmethod=auto\n[ipv6]\nmethod=auto\n").unwrap();
        let settings = read(file.to_str().unwrap()).unwrap();
        assert_eq!(
            <&str>::try_from(&settings["connection"]["id"]).unwrap(),
            "Private import fixture"
        );
        assert_eq!(
            <&str>::try_from(&settings["802-11-wireless-security"]["psk"]).unwrap(),
            "synthetic-import-only"
        );
        assert_eq!(
            Vec::<u8>::try_from(settings["802-11-wireless"]["ssid"].try_clone().unwrap()).unwrap(),
            b"Fixture"
        );
    }
}
