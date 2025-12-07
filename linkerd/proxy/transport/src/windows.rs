//! Windows-specific transparent proxy support via kernel driver.
//!
//! This module provides integration with a Windows kernel driver for transparent
//! proxying. It uses Windows device I/O control operations which inherently require
//! unsafe code for FFI calls.

#![allow(non_snake_case, non_camel_case_types, unsafe_code)]

use std::ffi::c_void;
use std::io;
use std::mem;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::ptr;
use windows_sys::Win32::Devices::DeviceAndDriverInstallation::{
    SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces, SetupDiGetClassDevsW,
    SetupDiGetDeviceInterfaceDetailW, DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO,
    SP_DEVICE_INTERFACE_DATA, SP_DEVICE_INTERFACE_DETAIL_DATA_W,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::IO::DeviceIoControl;

// GUID: {60f5a74b-dfb2-42f0-a4f0-35226367999c}
const LTR_GUID_DEVINTERFACE: windows_sys::core::GUID = windows_sys::core::GUID {
    data1: 0x60f5a74b,
    data2: 0xdfb2,
    data3: 0x42f0,
    data4: [0xa4, 0xf0, 0x35, 0x22, 0x63, 0x67, 0x99, 0x9c],
};

const LTR_FILE_DEVICE: u32 = 0x8000;
const METHOD_BUFFERED: u32 = 0;
const FILE_WRITE_DATA: u32 = 0x0002;
const FILE_READ_DATA: u32 = 0x0001;

macro_rules! CTL_CODE {
    ($DeviceType:expr, $Function:expr, $Method:expr, $Access:expr) => {
        ($DeviceType << 16) | ($Access << 14) | ($Function << 2) | $Method
    };
}

const LTR_IOCTL_REGISTER_PROXY: u32 =
    CTL_CODE!(LTR_FILE_DEVICE, 0x801, METHOD_BUFFERED, FILE_WRITE_DATA);
const LTR_IOCTL_QUERY_ORIGINAL_DEST: u32 =
    CTL_CODE!(LTR_FILE_DEVICE, 0x802, METHOD_BUFFERED, FILE_READ_DATA);

#[repr(C)]
struct LTR_REGISTER_PROXY_INPUT {
    ProxyPid: u32,
}

#[repr(C)]
struct LTR_ORIGINAL_TUPLE_QUERY_V1 {
    RemoteAddress: u32,
    RemotePort: u16,
    LocalAddress: u32,
    ProxyPort: u16,
}

#[repr(C)]
struct LTR_ORIGINAL_TUPLE_RESULT_V1 {
    Status: i32,
    RemoteAddress: u32,
    RemotePort: u16,
    LocalAddress: u32,
    OriginalDestinationPort: u16,
    ProxyPort: u16,
    Reserved: u16,
    CreatedTicks: u64,
    Flags: u32,
    Reserved2: u32,
}

/// RAII wrapper for device info handle.
struct DeviceInfo(HDEVINFO);

impl DeviceInfo {
    fn new() -> io::Result<Self> {
        let handle = unsafe {
            SetupDiGetClassDevsW(
                &LTR_GUID_DEVINTERFACE,
                ptr::null(),
                0,
                DIGCF_DEVICEINTERFACE | DIGCF_PRESENT,
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(DeviceInfo(handle))
        }
    }

    fn handle(&self) -> HDEVINFO {
        self.0
    }
}

impl Drop for DeviceInfo {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                SetupDiDestroyDeviceInfoList(self.0);
            }
        }
    }
}

/// RAII wrapper for device handle.
struct DeviceHandle(HANDLE);

impl DeviceHandle {
    fn from_path(path: &str) -> io::Result<Self> {
        let path_u16: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();

        let handle = unsafe {
            CreateFileW(
                path_u16.as_ptr(),
                GENERIC_READ | GENERIC_WRITE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                ptr::null(),
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                0,
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            Err(io::Error::last_os_error())
        } else {
            Ok(DeviceHandle(handle))
        }
    }

    fn handle(&self) -> HANDLE {
        self.0
    }
}

impl Drop for DeviceHandle {
    fn drop(&mut self) {
        if self.0 != INVALID_HANDLE_VALUE {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn get_device_path() -> io::Result<String> {
    let dev_info = DeviceInfo::new()?;

    let mut device_interface_data: SP_DEVICE_INTERFACE_DATA = unsafe { mem::zeroed() };
    device_interface_data.cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DATA>() as u32;

    let enum_result = unsafe {
        SetupDiEnumDeviceInterfaces(
            dev_info.handle(),
            ptr::null(),
            &LTR_GUID_DEVINTERFACE,
            0,
            &mut device_interface_data,
        )
    };

    if enum_result == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut required_size = 0;

    unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info.handle(),
            &device_interface_data,
            ptr::null_mut(),
            0,
            &mut required_size,
            ptr::null_mut(),
        );
    }

    if required_size == 0 {
        return Err(io::Error::last_os_error());
    }

    let mut detail_data_buffer = vec![0u8; required_size as usize];
    let p_detail_data = detail_data_buffer.as_mut_ptr() as *mut SP_DEVICE_INTERFACE_DETAIL_DATA_W;

    unsafe {
        (*p_detail_data).cbSize = mem::size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    }

    let detail_result = unsafe {
        SetupDiGetDeviceInterfaceDetailW(
            dev_info.handle(),
            &device_interface_data,
            p_detail_data,
            required_size,
            ptr::null_mut(),
            ptr::null_mut(),
        )
    };

    if detail_result == 0 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: DevicePath is a valid null-terminated UTF-16 string.
    unsafe {
        let p_device_path = &(*p_detail_data).DevicePath as *const u16;
        let len = (0..)
            .take_while(|&i| *p_device_path.offset(i) != 0)
            .count();
        let slice = std::slice::from_raw_parts(p_device_path, len);
        Ok(String::from_utf16_lossy(slice))
    }
}

fn open_device() -> io::Result<DeviceHandle> {
    let path = get_device_path()?;
    DeviceHandle::from_path(&path)
}

/// Registers the current process with the transparent proxy driver.
///
/// This notifies the driver to track connections for this proxy process,
/// enabling original destination queries.
pub fn register_proxy(pid: u32) -> io::Result<()> {
    let device = open_device()?;
    let input = LTR_REGISTER_PROXY_INPUT { ProxyPid: pid };
    let mut bytes_returned = 0;

    let result = unsafe {
        DeviceIoControl(
            device.handle(),
            LTR_IOCTL_REGISTER_PROXY,
            &input as *const _ as *const c_void,
            mem::size_of::<LTR_REGISTER_PROXY_INPUT>() as u32,
            ptr::null_mut(),
            0,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Queries the original destination address before transparent redirection.
///
/// Given the local and remote addresses of an accepted connection, queries
/// the driver to determine the original destination the client intended to reach.
pub fn query_original_dst(local: SocketAddr, remote: SocketAddr) -> io::Result<SocketAddr> {
    let (local_ip, local_port) = match local {
        SocketAddr::V4(v4) => (v4.ip().to_bits(), v4.port()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv6 not supported",
            ))
        }
    };

    let (remote_ip, remote_port) = match remote {
        SocketAddr::V4(v4) => (v4.ip().to_bits(), v4.port()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "IPv6 not supported",
            ))
        }
    };

    let input = LTR_ORIGINAL_TUPLE_QUERY_V1 {
        RemoteAddress: remote_ip,
        RemotePort: remote_port,
        LocalAddress: local_ip,
        ProxyPort: local_port,
    };

    let mut output: LTR_ORIGINAL_TUPLE_RESULT_V1 = unsafe { mem::zeroed() };
    let mut bytes_returned = 0;
    let device = open_device()?;

    let result = unsafe {
        DeviceIoControl(
            device.handle(),
            LTR_IOCTL_QUERY_ORIGINAL_DEST,
            &input as *const _ as *const c_void,
            mem::size_of::<LTR_ORIGINAL_TUPLE_QUERY_V1>() as u32,
            &mut output as *mut _ as *mut c_void,
            mem::size_of::<LTR_ORIGINAL_TUPLE_RESULT_V1>() as u32,
            &mut bytes_returned,
            ptr::null_mut(),
        )
    };

    if result == 0 {
        return Err(io::Error::last_os_error());
    }

    if output.Status != 0 {
        return Err(io::Error::from_raw_os_error(output.Status));
    }

    let ip = Ipv4Addr::from_bits(output.LocalAddress);
    let port = output.OriginalDestinationPort;

    Ok(SocketAddr::V4(SocketAddrV4::new(ip, port)))
}
