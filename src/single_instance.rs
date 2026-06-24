use core::ffi::c_void;

use windows::core::{Error, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, ERROR_SUCCESS, HANDLE};

#[link(name = "kernel32")]
extern "system" {
    #[link_name = "CreateMutexW"]
    fn create_mutex_w(mutex_attributes: *const c_void, initial_owner: i32, name: PCWSTR) -> HANDLE;

    #[link_name = "GetLastError"]
    fn get_last_error() -> u32;

    #[link_name = "SetLastError"]
    fn set_last_error(error: u32);
}

pub struct SingleInstance {
    handle: HANDLE,
}

impl SingleInstance {
    pub unsafe fn acquire() -> windows::core::Result<Option<Self>> {
        let name: Vec<u16> = "Local\\FeatherDock.SingleInstance"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        set_last_error(ERROR_SUCCESS.0);
        let handle = create_mutex_w(core::ptr::null(), 1, PCWSTR(name.as_ptr()));
        if handle.is_invalid() {
            return Err(Error::from_win32());
        }
        let status = get_last_error();
        if status == ERROR_ALREADY_EXISTS.0 {
            let _ = CloseHandle(handle);
            Ok(None)
        } else {
            Ok(Some(Self { handle }))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.handle);
        }
    }
}
