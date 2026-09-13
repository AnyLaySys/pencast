use std::mem::size_of;

use windows::Win32::Devices::DeviceAndDriverInstallation::{
    DIGCF_DEVICEINTERFACE, DIGCF_PRESENT, HDEVINFO, SP_DEVICE_INTERFACE_DATA,
    SP_DEVICE_INTERFACE_DETAIL_DATA_W, SetupDiDestroyDeviceInfoList, SetupDiEnumDeviceInterfaces,
    SetupDiGetClassDevsW, SetupDiGetDeviceInterfaceDetailW,
};
use windows::Win32::Devices::Usb::{
    USB_INTERFACE_DESCRIPTOR, UsbdPipeTypeBulk, WINUSB_INTERFACE_HANDLE, WINUSB_PIPE_INFORMATION,
    WinUsb_AbortPipe, WinUsb_Free, WinUsb_GetOverlappedResult, WinUsb_Initialize,
    WinUsb_QueryInterfaceSettings, WinUsb_QueryPipe, WinUsb_ReadPipe, WinUsb_WritePipe,
};
use windows::Win32::Foundation::{CloseHandle, ERROR_IO_PENDING, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_OVERLAPPED, FILE_GENERIC_READ,
    FILE_GENERIC_WRITE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::IO::{CancelIoEx, OVERLAPPED};
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{GUID, HRESULT, PCWSTR};

const MIRROR_GUID: GUID = GUID::from_u128(0xd4c39b42_ba47_4e8d_83f8_da4a3b6b8f35);

struct DeviceList(HDEVINFO);

impl Drop for DeviceList {
    fn drop(&mut self) {
        unsafe {
            let _ = SetupDiDestroyDeviceInfoList(self.0);
        }
    }
}

pub(crate) struct Usb {
    pub(crate) device: HANDLE,
    pub(crate) interface: WINUSB_INTERFACE_HANDLE,
    pub(crate) input: u8,
    pub(crate) output: u8,
}

impl Usb {
    pub(crate) fn write(&self, data: &[u8]) -> Result<(), String> {
        let mut written = 0;
        transfer_write(self.interface, self.output, data, &mut written)?;
        if written as usize != data.len() {
            return Err(String::from("Short USB write"));
        }
        Ok(())
    }

    pub(crate) fn read_exact(&self, data: &mut [u8]) -> Result<(), String> {
        let mut offset = 0;
        while offset < data.len() {
            let mut read = 0;
            transfer_read(self.interface, self.input, &mut data[offset..], &mut read)?;
            if read == 0 {
                return Err(String::from("Empty USB read"));
            }
            offset += read as usize;
        }
        Ok(())
    }

    pub(crate) fn abort(&self) {
        unsafe {
            let _ = CancelIoEx(self.device, None);
            let _ = WinUsb_AbortPipe(self.interface, self.input);
        }
    }
}

impl Drop for Usb {
    fn drop(&mut self) {
        unsafe {
            let _ = WinUsb_Free(self.interface);
            let _ = CloseHandle(self.device);
        }
    }
}

fn is_pending(error: &windows::core::Error) -> bool {
    error.code() == HRESULT::from_win32(ERROR_IO_PENDING.0)
}

fn transfer_write(
    interface: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buffer: &[u8],
    transferred: &mut u32,
) -> Result<(), String> {
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| error.to_string())?;
    let overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let result = match unsafe {
        WinUsb_WritePipe(
            interface,
            endpoint,
            buffer,
            Some(transferred),
            Some(&overlapped),
        )
    } {
        Ok(()) => Ok(()),
        Err(error) if is_pending(&error) => {
            if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
                Err(String::from("USB write wait failed"))
            } else {
                unsafe {
                    WinUsb_GetOverlappedResult(interface, &overlapped, transferred, false)
                        .map_err(|error| error.to_string())
                }
            }
        }
        Err(error) => Err(error.to_string()),
    };
    unsafe {
        let _ = CloseHandle(event);
    }
    result
}

fn transfer_read(
    interface: WINUSB_INTERFACE_HANDLE,
    endpoint: u8,
    buffer: &mut [u8],
    transferred: &mut u32,
) -> Result<(), String> {
    let event = unsafe { CreateEventW(None, true, false, PCWSTR::null()) }
        .map_err(|error| error.to_string())?;
    let overlapped = OVERLAPPED {
        hEvent: event,
        ..Default::default()
    };
    let result = match unsafe {
        WinUsb_ReadPipe(
            interface,
            endpoint,
            Some(buffer),
            Some(transferred),
            Some(&overlapped),
        )
    } {
        Ok(()) => Ok(()),
        Err(error) if is_pending(&error) => {
            if unsafe { WaitForSingleObject(event, INFINITE) } != WAIT_OBJECT_0 {
                Err(String::from("USB read wait failed"))
            } else {
                unsafe {
                    WinUsb_GetOverlappedResult(interface, &overlapped, transferred, false)
                        .map_err(|error| error.to_string())
                }
            }
        }
        Err(error) => Err(error.to_string()),
    };
    unsafe {
        let _ = CloseHandle(event);
    }
    result
}

pub(crate) fn open() -> Result<Usb, String> {
    let list = unsafe {
        SetupDiGetClassDevsW(
            Some(&MIRROR_GUID),
            PCWSTR::null(),
            None,
            DIGCF_PRESENT | DIGCF_DEVICEINTERFACE,
        )
    }
    .map_err(|error| error.to_string())?;
    let list = DeviceList(list);
    let mut data = SP_DEVICE_INTERFACE_DATA {
        cbSize: size_of::<SP_DEVICE_INTERFACE_DATA>() as u32,
        ..Default::default()
    };
    unsafe {
        SetupDiEnumDeviceInterfaces(list.0, None, &MIRROR_GUID, 0, &mut data)
            .map_err(|_| String::from("PenCast USB interface not found"))?;
    }
    let mut bytes = 0;
    unsafe {
        let _ = SetupDiGetDeviceInterfaceDetailW(list.0, &data, None, 0, Some(&mut bytes), None);
    }
    if bytes == 0 {
        return Err(String::from("PenCast USB path is unavailable"));
    }
    let mut path = vec![0u8; bytes as usize];
    let detail = path
        .as_mut_ptr()
        .cast::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>();
    unsafe {
        (*detail).cbSize = size_of::<SP_DEVICE_INTERFACE_DETAIL_DATA_W>() as u32;
    }
    unsafe {
        SetupDiGetDeviceInterfaceDetailW(list.0, &data, Some(detail), bytes, None, None)
            .map_err(|error| error.to_string())?;
    }
    let device = unsafe {
        CreateFileW(
            PCWSTR((*detail).DevicePath.as_ptr()),
            FILE_GENERIC_READ.0 | FILE_GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|error| error.to_string())?;
    let mut interface = WINUSB_INTERFACE_HANDLE::default();
    if let Err(error) = unsafe { WinUsb_Initialize(device, &mut interface) } {
        unsafe {
            let _ = CloseHandle(device);
        }
        return Err(error.to_string());
    }
    let mut descriptor = USB_INTERFACE_DESCRIPTOR::default();
    if let Err(error) = unsafe { WinUsb_QueryInterfaceSettings(interface, 0, &mut descriptor) } {
        unsafe {
            let _ = WinUsb_Free(interface);
            let _ = CloseHandle(device);
        }
        return Err(error.to_string());
    }
    let mut input = 0;
    let mut output = 0;
    for index in 0..descriptor.bNumEndpoints {
        let mut pipe = WINUSB_PIPE_INFORMATION::default();
        if unsafe { WinUsb_QueryPipe(interface, 0, index, &mut pipe) }.is_err()
            || pipe.PipeType != UsbdPipeTypeBulk
        {
            continue;
        }
        if pipe.PipeId & 0x80 != 0 {
            input = pipe.PipeId;
        } else {
            output = pipe.PipeId;
        }
    }
    if input == 0 || output == 0 {
        unsafe {
            let _ = WinUsb_Free(interface);
            let _ = CloseHandle(device);
        }
        return Err(String::from("PenCast bulk endpoints are unavailable"));
    }
    Ok(Usb {
        device,
        interface,
        input,
        output,
    })
}
