use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_SZ, RegCloseKey, RegEnumKeyExW, RegOpenKeyExW,
    RegQueryInfoKeyW, RegQueryValueExW,
};

/// A single display adapter discovered from the device-instance registry class.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuAdapter {
    /// Human-readable description, e.g. "NVIDIA GeForce RTX 4070".
    pub description: String,
    /// PCI vendor id without the `VEN_` prefix, uppercased, e.g. "10DE".
    pub vendor_id: String,
    /// PCI device id without the `DEV_` prefix, uppercased, e.g. "2684".
    pub device_id: String,
    /// The registry class subkey name, used to deduplicate entries.
    pub key_name: String,
}

const CLASS_SUBKEY: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn query_string(key: HKEY, value_name: &str) -> Option<String> {
    unsafe {
        let name = wide(value_name);
        let mut value_type: u32 = 0;
        let mut size: u32 = 0;
        let status = RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            std::ptr::null_mut(),
            &mut size,
        );
        if status != 0 || (value_type != REG_SZ && value_type != 0) || size < 2 {
            return None;
        }
        let mut buffer = vec![0u8; size as usize];
        let mut filled = size;
        if RegQueryValueExW(
            key,
            name.as_ptr(),
            std::ptr::null(),
            &mut value_type,
            buffer.as_mut_ptr(),
            &mut filled,
        ) != 0
        {
            return None;
        }
        let utf16_len = (filled as usize) / 2;
        let slice = std::slice::from_raw_parts(buffer.as_ptr() as *const u16, utf16_len);
        let end = slice.iter().position(|&u| u == 0).unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..end]);
        Some(text.trim().to_owned())
    }
}

/// Enumerate display adapters from the device-instance registry class. This is a
/// read-only metadata probe that needs no elevated privileges and ships no DLLs.
pub(crate) fn enumerate_adapters() -> Vec<GpuAdapter> {
    unsafe {
        let mut class_key: HKEY = std::ptr::null_mut();
        let class_name = wide(CLASS_SUBKEY);
        if RegOpenKeyExW(
            HKEY_LOCAL_MACHINE,
            class_name.as_ptr(),
            0,
            KEY_READ,
            &mut class_key,
        ) != 0
        {
            return Vec::new();
        }
        let mut subkey_count: u32 = 0;
        let mut max_subkey_len: u32 = 0;
        let info_status = RegQueryInfoKeyW(
            class_key,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut subkey_count,
            &mut max_subkey_len,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if info_status != 0 {
            RegCloseKey(class_key);
            return Vec::new();
        }
        let capacity = (max_subkey_len + 1).max(1) as usize;
        let mut adapters = Vec::new();
        let mut index: u32 = 0;
        loop {
            let mut name_buffer = vec![0u16; capacity];
            let mut name_len = name_buffer.len() as u32;
            let status = RegEnumKeyExW(
                class_key,
                index,
                name_buffer.as_mut_ptr(),
                &mut name_len,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut() as *mut FILETIME,
            );
            if status != 0 {
                break;
            }
            index += 1;
            let name_end = name_buffer
                .iter()
                .position(|&u| u == 0)
                .unwrap_or(name_len as usize);
            let key_name = String::from_utf16_lossy(&name_buffer[..name_end]);
            let mut sub_key: HKEY = std::ptr::null_mut();
            let sub_key_name = format!("{CLASS_SUBKEY}\\{key_name}");
            let sub_name = wide(&sub_key_name);
            if RegOpenKeyExW(
                HKEY_LOCAL_MACHINE,
                sub_name.as_ptr(),
                0,
                KEY_READ,
                &mut sub_key,
            ) != 0
            {
                continue;
            }
            let description = query_string(sub_key, "DriverDesc");
            let matching = query_string(sub_key, "MatchingDeviceId");
            RegCloseKey(sub_key);
            let Some(description) = description else {
                continue;
            };
            let (vendor_id, device_id) = parse_matching_id(&matching);
            adapters.push(GpuAdapter {
                description,
                vendor_id,
                device_id,
                key_name,
            });
        }
        RegCloseKey(class_key);
        adapters
    }
}

fn parse_matching_id(matching: &Option<String>) -> (String, String) {
    let Some(matching) = matching else {
        return (String::new(), String::new());
    };
    let ven = matching
        .split("VEN_")
        .nth(1)
        .and_then(|rest| rest.split(['&', '_']).next())
        .map(|id| id.to_uppercase());
    let dev = matching
        .split("DEV_")
        .nth(1)
        .and_then(|rest| rest.split(['&', '_']).next())
        .map(|id| id.to_uppercase());
    (ven.unwrap_or_default(), dev.unwrap_or_default())
}
