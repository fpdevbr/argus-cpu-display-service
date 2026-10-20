//! USB HID output for the cooler display.
//!
//! Finds the device through the configuration manager, then keeps a single
//! handle and a reused report buffer. Writes go out only when the caller asks.

use std::io::{self, ErrorKind};
use std::mem::{self, size_of};
use std::ptr::{null, null_mut};

use windows_sys::core::GUID;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_BUFFER_SMALL, CR_SUCCESS,
};
use windows_sys::Win32::Devices::HumanInterfaceDevice::{
    HidD_FreePreparsedData, HidD_GetAttributes, HidD_GetHidGuid, HidD_GetPreparsedData,
    HidP_GetCaps, HIDD_ATTRIBUTES, HIDP_CAPS, HIDP_STATUS_SUCCESS, PHIDP_PREPARSED_DATA,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, WriteFile, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};

pub const USB_PACKET_SIZE: usize = 65;
pub const REPORT_ID: u8 = 0x00;
pub const REPORT_COMMAND: u8 = 0x10;
pub const TEMP_MIN: i32 = 0;
pub const TEMP_MAX: i32 = 99;

pub struct CpuDisplay {
    handle: HANDLE,
    packet: Vec<u8>,
}

impl Drop for CpuDisplay {
    fn drop(&mut self) {
        if valid_handle(self.handle) {
            unsafe { CloseHandle(self.handle) };
            self.handle = null_mut();
        }
    }
}

impl CpuDisplay {
    pub fn open(vendor_id: u16, product_id: u16) -> io::Result<Self> {
        let handle = open_hid(vendor_id, product_id)?;
        let device_len = output_report_len(handle).unwrap_or(0);
        let write_len = USB_PACKET_SIZE.max(device_len);
        let mut packet = vec![0u8; write_len];
        packet[0] = REPORT_ID;
        packet[1] = REPORT_COMMAND;
        Ok(Self { handle, packet })
    }

    pub fn send_temperature(&mut self, temp: u8) -> io::Result<()> {
        self.packet[2] = temp;
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle,
                self.packet.as_ptr(),
                self.packet.len() as u32,
                &mut written,
                null_mut(),
            )
        };
        if ok == 0 {
            let code = unsafe { GetLastError() } as i32;
            return Err(io::Error::from_raw_os_error(code));
        }
        Ok(())
    }
}

/// Half away from zero, then clamped to the two-digit display range.
pub fn round_display_temp(raw: f64) -> i32 {
    if !raw.is_finite() {
        return TEMP_MIN;
    }
    let rounded = raw.round() as i32;
    rounded.clamp(TEMP_MIN, TEMP_MAX)
}

fn open_hid(vendor_id: u16, product_id: u16) -> io::Result<HANDLE> {
    let mut guid: GUID = unsafe { mem::zeroed() };
    unsafe { HidD_GetHidGuid(&mut guid) };

    let list = interface_list(&guid)?;
    let paths: Vec<&[u16]> = list
        .split(|&unit| unit == 0)
        .filter(|path| !path.is_empty())
        .collect();

    // USB HID paths embed "vid_xxxx&pid_xxxx". Trying those first avoids opening
    // every keyboard and mouse; the full scan still covers other path formats.
    let needle = format!("vid_{vendor_id:04x}&pid_{product_id:04x}");
    let (likely, others): (Vec<&[u16]>, Vec<&[u16]>) = paths
        .into_iter()
        .partition(|path| path_contains(path, &needle));

    for path in likely.iter().chain(others.iter()) {
        // Each path in the multi-string list is followed by its NUL terminator.
        let handle = open_path(path.as_ptr());
        if !valid_handle(handle) {
            continue;
        }
        if hid_matches(handle, vendor_id, product_id) {
            return Ok(handle);
        }
        unsafe { CloseHandle(handle) };
    }

    Err(io::Error::new(
        ErrorKind::NotFound,
        format!("no HID device with VID 0x{vendor_id:04X} PID 0x{product_id:04X}"),
    ))
}

fn interface_list(guid: &GUID) -> io::Result<Vec<u16>> {
    loop {
        let mut len = 0u32;
        let rc = unsafe {
            CM_Get_Device_Interface_List_SizeW(
                &mut len,
                guid,
                null(),
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        if rc != CR_SUCCESS {
            return Err(io::Error::other(format!(
                "CM_Get_Device_Interface_List_SizeW failed ({rc})"
            )));
        }

        let mut list = vec![0u16; len.max(1) as usize];
        let rc = unsafe {
            CM_Get_Device_Interface_ListW(
                guid,
                null(),
                list.as_mut_ptr(),
                len,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        };
        match rc {
            CR_SUCCESS => return Ok(list),
            CR_BUFFER_SMALL => continue,
            _ => {
                return Err(io::Error::other(format!(
                    "CM_Get_Device_Interface_ListW failed ({rc})"
                )))
            }
        }
    }
}

fn path_contains(path: &[u16], needle: &str) -> bool {
    String::from_utf16_lossy(path)
        .to_ascii_lowercase()
        .contains(needle)
}

fn open_path(path: *const u16) -> HANDLE {
    unsafe {
        let shared = FILE_SHARE_READ | FILE_SHARE_WRITE;
        let handle = CreateFileW(
            path,
            GENERIC_READ | GENERIC_WRITE,
            shared,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        );
        if valid_handle(handle) {
            return handle;
        }
        CreateFileW(
            path,
            GENERIC_WRITE,
            shared,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    }
}

fn hid_matches(handle: HANDLE, vendor_id: u16, product_id: u16) -> bool {
    let mut attrs: HIDD_ATTRIBUTES = unsafe { mem::zeroed() };
    attrs.Size = size_of::<HIDD_ATTRIBUTES>() as u32;
    let ok = unsafe { HidD_GetAttributes(handle, &mut attrs) };
    ok != 0 && attrs.VendorID == vendor_id && attrs.ProductID == product_id
}

fn valid_handle(handle: HANDLE) -> bool {
    !handle.is_null() && handle != INVALID_HANDLE_VALUE
}

fn output_report_len(handle: HANDLE) -> Option<usize> {
    unsafe {
        let mut preparsed: PHIDP_PREPARSED_DATA = 0;
        if HidD_GetPreparsedData(handle, &mut preparsed) == 0 {
            return None;
        }
        let mut caps: HIDP_CAPS = mem::zeroed();
        let status = HidP_GetCaps(preparsed, &mut caps);
        HidD_FreePreparsedData(preparsed);
        if status != HIDP_STATUS_SUCCESS {
            return None;
        }
        let len = caps.OutputReportByteLength as usize;
        if (1..=4096).contains(&len) {
            Some(len)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_half_away_and_clamps() {
        assert_eq!(round_display_temp(45.4), 45);
        assert_eq!(round_display_temp(45.5), 46);
        assert_eq!(round_display_temp(45.6), 46);
        assert_eq!(round_display_temp(99.5), 99);
        assert_eq!(round_display_temp(120.0), 99);
        assert_eq!(round_display_temp(-3.2), 0);
        assert_eq!(round_display_temp(0.5), 1);
        assert_eq!(round_display_temp(f64::NAN), 0);
    }
}
