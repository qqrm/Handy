use super::OutputBackend;
use std::ffi::{c_char, c_void, CStr};

pub(super) struct Backend;

pub(super) fn backend() -> Backend {
    Backend
}

pub(super) struct Target {
    id: u32,
    uid: String,
}

// Public Core Audio HAL properties. A global AppleScript "output muted"
// command would resolve a different device after the default output changes.
const SYSTEM_OBJECT: u32 = 1;
const GLOBAL_SCOPE: u32 = u32::from_be_bytes(*b"glob");
const OUTPUT_SCOPE: u32 = u32::from_be_bytes(*b"outp");
const DEFAULT_OUTPUT: u32 = u32::from_be_bytes(*b"dOut");
const DEVICE_UID: u32 = u32::from_be_bytes(*b"uid ");
const MUTE: u32 = u32::from_be_bytes(*b"mute");
const UTF8_ENCODING: u32 = 0x0800_0100;

#[repr(C)]
struct PropertyAddress {
    selector: u32,
    scope: u32,
    element: u32,
}

impl PropertyAddress {
    fn new(selector: u32, scope: u32) -> Self {
        Self {
            selector,
            scope,
            element: 0,
        }
    }
}

#[link(name = "CoreAudio", kind = "framework")]
extern "C" {
    fn AudioObjectGetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: *mut u32,
        data: *mut c_void,
    ) -> i32;
    fn AudioObjectSetPropertyData(
        object: u32,
        address: *const PropertyAddress,
        qualifier_size: u32,
        qualifier: *const c_void,
        data_size: u32,
        data: *const c_void,
    ) -> i32;
    fn AudioObjectIsPropertySettable(
        object: u32,
        address: *const PropertyAddress,
        settable: *mut u8,
    ) -> i32;
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(object: *const c_void);
    fn CFStringGetLength(string: *const c_void) -> isize;
    fn CFStringGetMaximumSizeForEncoding(length: isize, encoding: u32) -> isize;
    fn CFStringGetCString(
        string: *const c_void,
        buffer: *mut c_char,
        buffer_size: isize,
        encoding: u32,
    ) -> u8;
}

impl OutputBackend for Backend {
    type Target = Target;

    fn default_target(&self) -> Result<Target, String> {
        let id = read_u32(SYSTEM_OBJECT, DEFAULT_OUTPUT, GLOBAL_SCOPE)?;
        if id == 0 {
            return Err("Core Audio has no default output device".into());
        }
        Ok(Target {
            id,
            uid: read_uid(id)?,
        })
    }

    fn is_muted(&self, target: &Target) -> Result<bool, String> {
        verify_identity(target)?;
        match read_u32(target.id, MUTE, OUTPUT_SCOPE)? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err("Core Audio returned an unknown output mute state".into()),
        }
    }

    fn set_muted(&self, target: &Target, muted: bool) -> Result<(), String> {
        verify_identity(target)?;
        let address = PropertyAddress::new(MUTE, OUTPUT_SCOPE);
        let mut settable = 0;
        let status = unsafe { AudioObjectIsPropertySettable(target.id, &address, &mut settable) };
        check_status(status, "query output mute support")?;
        if settable == 0 {
            return Err("The original Core Audio output has no writable mute control".into());
        }
        let value = u32::from(muted);
        let status = unsafe {
            AudioObjectSetPropertyData(
                target.id,
                &address,
                0,
                std::ptr::null(),
                std::mem::size_of::<u32>() as u32,
                (&value as *const u32).cast(),
            )
        };
        check_status(status, "set output mute")
    }
}

fn verify_identity(target: &Target) -> Result<(), String> {
    // Device IDs are runtime handles. The persistent UID additionally rejects
    // a different device that has reused an old handle after a HAL change.
    // https://developer.apple.com/documentation/technotes/tn3190-usb-audio-device-design-considerations
    if read_uid(target.id)? != target.uid {
        return Err("The original Core Audio output device no longer exists".into());
    }
    Ok(())
}

fn read_u32(object: u32, selector: u32, scope: u32) -> Result<u32, String> {
    let address = PropertyAddress::new(selector, scope);
    let mut value = 0_u32;
    let expected_size = std::mem::size_of::<u32>() as u32;
    let mut size = expected_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut u32).cast(),
        )
    };
    check_status(status, "read output property")?;
    if size != expected_size {
        return Err("Core Audio returned an unexpected output property size".into());
    }
    Ok(value)
}

struct CfString(*const c_void);

impl Drop for CfString {
    fn drop(&mut self) {
        unsafe { CFRelease(self.0) };
    }
}

fn read_uid(object: u32) -> Result<String, String> {
    let address = PropertyAddress::new(DEVICE_UID, GLOBAL_SCOPE);
    let mut value: *const c_void = std::ptr::null();
    let expected_size = std::mem::size_of::<*const c_void>() as u32;
    let mut size = expected_size;
    let status = unsafe {
        AudioObjectGetPropertyData(
            object,
            &address,
            0,
            std::ptr::null(),
            &mut size,
            (&mut value as *mut *const c_void).cast(),
        )
    };
    check_status(status, "read output device UID")?;
    if value.is_null() {
        return Err("Core Audio returned no output device UID".into());
    }
    // kAudioDevicePropertyDeviceUID transfers a CFString reference to the
    // caller. Convert it to owned Rust data before returning to another thread.
    let value = CfString(value);
    if size != expected_size {
        return Err("Core Audio returned an unexpected output UID size".into());
    }
    let length = unsafe { CFStringGetLength(value.0) };
    let capacity = unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8_ENCODING) }
        .checked_add(1)
        .filter(|capacity| *capacity > 0)
        .ok_or("Cannot size the Core Audio output UID")?;
    let mut bytes = vec![0_u8; capacity as usize];
    let converted =
        unsafe { CFStringGetCString(value.0, bytes.as_mut_ptr().cast(), capacity, UTF8_ENCODING) };
    if converted == 0 {
        return Err("Cannot decode the Core Audio output UID".into());
    }
    let uid = CStr::from_bytes_until_nul(&bytes)
        .map_err(|error| format!("Invalid Core Audio output UID: {error}"))?
        .to_str()
        .map_err(|error| format!("Invalid UTF-8 in Core Audio output UID: {error}"))?;
    if uid.is_empty() {
        return Err("Core Audio returned an empty output device UID".into());
    }
    Ok(uid.into())
}

fn check_status(status: i32, operation: &str) -> Result<(), String> {
    if status == 0 {
        Ok(())
    } else {
        Err(format!("Cannot {operation}: Core Audio error {status}"))
    }
}
