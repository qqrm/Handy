use super::OutputBackend;
use windows::core::{Error, PCWSTR, PWSTR};
use windows::Win32::{
    Foundation::RPC_E_CHANGED_MODE,
    Media::Audio::{
        eMultimedia, eRender, Endpoints::IAudioEndpointVolume, IMMDeviceEnumerator,
        MMDeviceEnumerator,
    },
    System::Com::{
        CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
        COINIT_MULTITHREADED,
    },
};

pub(super) struct Backend;

pub(super) fn backend() -> Backend {
    Backend
}

// No COM interface crosses threads: the lease holds only the persistent
// endpoint ID, and each operation creates/releases its interfaces locally.
impl OutputBackend for Backend {
    type Target = String;

    fn default_target(&self) -> Result<String, String> {
        let _apartment = Apartment::new()?;
        let enumerator = enumerator()?;
        let device = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia) }
            .map_err(|error| format!("Cannot find the default output: {error}"))?;
        let id = TaskMemString(
            unsafe { device.GetId() }
                .map_err(|error| format!("Cannot identify the default output: {error}"))?,
        );
        let value = unsafe { id.0.to_string() }
            .map_err(|error| format!("Cannot decode the output endpoint ID: {error}"))?;
        if value.is_empty() {
            return Err("The output endpoint ID is empty".into());
        }
        Ok(value)
    }

    fn is_muted(&self, target: &String) -> Result<bool, String> {
        with_volume(target, |volume| {
            unsafe { volume.GetMute() }
                .map(|muted| muted.as_bool())
                .map_err(|error| format!("Cannot read output mute: {error}"))
        })
    }

    fn set_muted(&self, target: &String, muted: bool) -> Result<(), String> {
        with_volume(target, |volume| {
            unsafe { volume.SetMute(muted, std::ptr::null()) }
                .map_err(|error| format!("Cannot set output mute to {muted}: {error}"))
        })
    }
}

fn enumerator() -> Result<IMMDeviceEnumerator, String> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .map_err(|error| format!("Cannot open Windows audio devices: {error}"))
}

fn with_volume<T>(
    target: &str,
    operation: impl FnOnce(&IAudioEndpointVolume) -> Result<T, String>,
) -> Result<T, String> {
    let _apartment = Apartment::new()?;
    let enumerator = enumerator()?;
    let id: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();
    // GetDevice reopens the ORIGINAL endpoint, never the current default.
    // https://learn.microsoft.com/windows/win32/api/mmdeviceapi/nf-mmdeviceapi-immdeviceenumerator-getdevice
    let device = unsafe { enumerator.GetDevice(PCWSTR(id.as_ptr())) }
        .map_err(|error| format!("Cannot reopen the original output endpoint: {error}"))?;
    let volume = unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
        .map_err(|error| format!("Cannot access the original output volume: {error}"))?;
    operation(&volume)
}

struct TaskMemString(PWSTR);

impl Drop for TaskMemString {
    fn drop(&mut self) {
        unsafe { CoTaskMemFree(Some(self.0.as_ptr().cast())) };
    }
}

struct Apartment {
    initialized: bool,
}

impl Apartment {
    fn new() -> Result<Self, String> {
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result.is_ok() {
            // Both S_OK and S_FALSE increment COM's per-thread reference count.
            Ok(Self { initialized: true })
        } else if result == RPC_E_CHANGED_MODE {
            // Use the caller's existing apartment without changing its lifetime.
            Ok(Self { initialized: false })
        } else {
            Err(format!(
                "Cannot initialize COM for output mute: {}",
                Error::from_hresult(result)
            ))
        }
    }
}

impl Drop for Apartment {
    fn drop(&mut self) {
        if self.initialized {
            unsafe { CoUninitialize() };
        }
    }
}
