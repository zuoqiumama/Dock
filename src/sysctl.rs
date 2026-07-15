//! System integration for the control center: real master-volume control (Core
//! Audio), battery + clock readouts, and helpers to hand off to the native Windows
//! panels for things Win11 no longer lets a process drive directly (network,
//! Bluetooth, input-method switch). Pure-ish wrappers, all failures degrade quietly.

use core::ffi::c_void;
use std::slice;

use windows::core::*;
use windows::Win32::Devices::Bluetooth::*;
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, LPARAM, SYSTEMTIME, WPARAM};
use windows::Win32::Globalization::{
    GetLocaleInfoEx, LCIDToLocaleName, LOCALE_SLOCALIZEDDISPLAYNAME,
};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::NetworkManagement::WiFi::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
use windows::Win32::System::Registry::{RegGetValueW, HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Variant::VT_LPWSTR;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    ActivateKeyboardLayout, GetKeyboardLayout, GetKeyboardLayoutList, SendInput, HKL, INPUT,
    INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KLF_ACTIVATE,
    KLF_SETFORPROCESS, VIRTUAL_KEY, VK_LWIN, VK_SPACE,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    PostMessageW, HWND_BROADCAST, SW_SHOWNORMAL, WM_INPUTLANGCHANGEREQUEST,
};

/// Master volume of the default playback endpoint. Held open while the panel is up
/// so dragging the slider doesn't re-create COM objects on every mouse move.
pub struct AudioControl {
    endpoint: IAudioEndpointVolume,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AudioFlow {
    Output,
    Input,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioDeviceInfo {
    pub id: String,
    pub name: String,
    pub is_default: bool,
}

impl AudioDeviceInfo {
    #[cfg(test)]
    pub fn test(id: &str, name: &str, is_default: bool) -> Self {
        Self {
            id: id.to_string(),
            name: name.to_string(),
            is_default,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct AudioDevices {
    pub outputs: Vec<AudioDeviceInfo>,
    pub inputs: Vec<AudioDeviceInfo>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WifiNetworkInfo {
    pub ssid: String,
    pub secure: bool,
    pub has_profile: bool,
    pub signal: u8,
    pub connected: bool,
    pub connectable: bool,
    pub profile_name: String,
    pub auth: String,
    pub cipher: String,
}

impl WifiNetworkInfo {
    #[cfg(test)]
    pub fn test(ssid: &str, secure: bool, has_profile: bool, signal: u8, connected: bool) -> Self {
        Self {
            ssid: ssid.to_string(),
            secure,
            has_profile,
            signal,
            connected,
            connectable: true,
            profile_name: ssid.to_string(),
            auth: if secure { "WPA2PSK" } else { "open" }.to_string(),
            cipher: if secure { "AES" } else { "none" }.to_string(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WifiNetworks {
    pub available: bool,
    pub networks: Vec<WifiNetworkInfo>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BluetoothDeviceInfo {
    pub name: String,
    pub address: String,
    pub connected: bool,
    pub remembered: bool,
    pub authenticated: bool,
}

impl BluetoothDeviceInfo {
    #[cfg(test)]
    pub fn test(name: &str, connected: bool, remembered: bool) -> Self {
        Self {
            name: name.to_string(),
            address: String::new(),
            connected,
            remembered,
            authenticated: remembered,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct BluetoothDevices {
    pub available: bool,
    pub radio_name: Option<String>,
    pub devices: Vec<BluetoothDeviceInfo>,
    pub status: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InputMethodInfo {
    pub id: String,
    pub label: String,
    pub active: bool,
    pub hkl: isize,
}

impl InputMethodInfo {
    #[cfg(test)]
    pub fn test(id: &str, label: &str, active: bool) -> Self {
        let hkl = isize::from_str_radix(id, 16).unwrap_or_default();
        Self {
            id: id.to_string(),
            label: label.to_string(),
            active,
            hkl,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct InputMethods {
    pub methods: Vec<InputMethodInfo>,
    pub status: Option<String>,
}

type WifiResult<T> = std::result::Result<T, String>;
type BluetoothResult<T> = std::result::Result<T, String>;

impl AudioControl {
    /// Bind to the current default render endpoint. `None` if there is no output
    /// device (or COM isn't ready) — the panel simply hides the volume row then.
    pub fn open() -> Option<AudioControl> {
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
            let endpoint: IAudioEndpointVolume = device.Activate(CLSCTX_ALL, None).ok()?;
            Some(AudioControl { endpoint })
        }
    }

    /// Current level, 0.0..=1.0.
    pub fn level(&self) -> f32 {
        unsafe { self.endpoint.GetMasterVolumeLevelScalar().unwrap_or(0.0) }
    }

    pub fn muted(&self) -> bool {
        unsafe {
            self.endpoint
                .GetMute()
                .map(|b| b.as_bool())
                .unwrap_or(false)
        }
    }

    /// Set the level (clamped). Setting a non-zero level also unmutes, matching the
    /// system slider, so dragging up from a muted state actually produces sound.
    pub fn set_level(&self, level: f32) {
        unsafe {
            let level = level.clamp(0.0, 1.0);
            let _ = self
                .endpoint
                .SetMasterVolumeLevelScalar(level, std::ptr::null());
            if level > 0.0 && self.muted() {
                let _ = self.endpoint.SetMute(false, std::ptr::null());
            }
        }
    }

    pub fn set_muted(&self, muted: bool) {
        unsafe {
            let _ = self.endpoint.SetMute(muted, std::ptr::null());
        }
    }
}

impl AudioFlow {
    fn data_flow(self) -> EDataFlow {
        match self {
            AudioFlow::Output => eRender,
            AudioFlow::Input => eCapture,
        }
    }
}

const PKEY_DEVICE_FRIENDLY_NAME: PROPERTYKEY = PROPERTYKEY {
    fmtid: GUID::from_u128(0xa45c254e_df1c_4efd_8020_67d146a850e0),
    pid: 14,
};

pub fn audio_devices() -> AudioDevices {
    unsafe {
        let Ok(enumerator) =
            CoCreateInstance::<_, IMMDeviceEnumerator>(&MMDeviceEnumerator, None, CLSCTX_ALL)
        else {
            return AudioDevices::default();
        };

        let default_output = default_audio_id(&enumerator, AudioFlow::Output);
        let default_input = default_audio_id(&enumerator, AudioFlow::Input);

        AudioDevices {
            outputs: enumerate_audio_devices(
                &enumerator,
                AudioFlow::Output,
                default_output.as_deref(),
            ),
            inputs: enumerate_audio_devices(
                &enumerator,
                AudioFlow::Input,
                default_input.as_deref(),
            ),
        }
    }
}

unsafe fn default_audio_id(enumerator: &IMMDeviceEnumerator, flow: AudioFlow) -> Option<String> {
    let device = enumerator
        .GetDefaultAudioEndpoint(flow.data_flow(), eConsole)
        .ok()?;
    device_id(&device)
}

unsafe fn enumerate_audio_devices(
    enumerator: &IMMDeviceEnumerator,
    flow: AudioFlow,
    default_id: Option<&str>,
) -> Vec<AudioDeviceInfo> {
    let Ok(collection) = enumerator.EnumAudioEndpoints(flow.data_flow(), DEVICE_STATE_ACTIVE)
    else {
        return Vec::new();
    };
    let count = collection.GetCount().unwrap_or(0).min(8);
    let mut devices = Vec::with_capacity(count as usize);
    for i in 0..count {
        let Ok(device) = collection.Item(i) else {
            continue;
        };
        let Some(id) = device_id(&device) else {
            continue;
        };
        let name =
            friendly_device_name(&device).unwrap_or_else(|| fallback_device_name(&id, flow, i));
        devices.push(AudioDeviceInfo {
            is_default: default_id == Some(id.as_str()),
            id,
            name,
        });
    }
    devices
}

unsafe fn device_id(device: &IMMDevice) -> Option<String> {
    let raw = device.GetId().ok()?;
    let text = pwstr_to_string(raw);
    CoTaskMemFree(Some(raw.0 as *const c_void));
    text
}

unsafe fn pwstr_to_string(raw: PWSTR) -> Option<String> {
    if raw.0.is_null() {
        return None;
    }
    let mut len = 0usize;
    while *raw.0.add(len) != 0 {
        len += 1;
    }
    Some(String::from_utf16_lossy(slice::from_raw_parts(raw.0, len)))
}

unsafe fn friendly_device_name(device: &IMMDevice) -> Option<String> {
    let store = device.OpenPropertyStore(STGM_READ).ok()?;
    let property_value = store.GetValue(&PKEY_DEVICE_FRIENDLY_NAME).ok()?;
    let prop_variant = &property_value.as_raw().Anonymous.Anonymous;
    if prop_variant.vt != VT_LPWSTR.0 {
        return None;
    }
    let ptr_utf16 = *(&prop_variant.Anonymous as *const _ as *const *const u16);
    if ptr_utf16.is_null() {
        return None;
    }
    let mut len = 0isize;
    while *ptr_utf16.offset(len) != 0 {
        len += 1;
    }
    let name = String::from_utf16_lossy(slice::from_raw_parts(ptr_utf16, len as usize));
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

fn fallback_device_name(id: &str, flow: AudioFlow, index: u32) -> String {
    let prefix = match flow {
        AudioFlow::Output => "输出设备",
        AudioFlow::Input => "输入设备",
    };
    let tail = id
        .rsplit(['#', '{', '}'])
        .find(|part| !part.is_empty())
        .unwrap_or("");
    if tail.is_empty() {
        format!("{} {}", prefix, index + 1)
    } else {
        format!("{} {}", prefix, tail)
    }
}

#[repr(C)]
struct PolicyConfigVTable {
    query_interface:
        unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    get_mix_format: usize,
    get_device_format: usize,
    reset_device_format: usize,
    set_device_format: usize,
    get_processing_period: usize,
    set_processing_period: usize,
    get_share_mode: usize,
    set_share_mode: usize,
    get_property_value: usize,
    set_property_value: usize,
    set_default_endpoint: unsafe extern "system" fn(*mut c_void, PCWSTR, ERole) -> HRESULT,
    set_endpoint_visibility: usize,
}

#[link(name = "ole32")]
extern "system" {
    #[link_name = "CoCreateInstance"]
    fn co_create_instance_raw(
        rclsid: *const GUID,
        punkouter: *mut c_void,
        dwclscontext: u32,
        riid: *const GUID,
        ppv: *mut *mut c_void,
    ) -> HRESULT;
}

const CLSID_POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);
const IID_POLICY_CONFIG: GUID = GUID::from_u128(0xf8679f50_850a_41cf_9c72_430f290290c8);

pub fn set_default_audio_device(flow: AudioFlow, device_id: &str) -> bool {
    if device_id.is_empty() {
        return false;
    }
    unsafe { set_default_audio_device_inner(flow, device_id) }
}

unsafe fn set_default_audio_device_inner(_flow: AudioFlow, device_id: &str) -> bool {
    let mut raw = std::ptr::null_mut();
    let hr = co_create_instance_raw(
        &CLSID_POLICY_CONFIG_CLIENT,
        std::ptr::null_mut(),
        CLSCTX_ALL.0,
        &IID_POLICY_CONFIG,
        &mut raw,
    );
    if hr.is_err() || raw.is_null() {
        return false;
    }

    let vtbl = *(raw as *mut *mut PolicyConfigVTable);
    let device_id = wide(device_id);
    let mut ok = true;
    for role in [eConsole, eMultimedia, eCommunications] {
        ok &= ((*vtbl).set_default_endpoint)(raw, PCWSTR(device_id.as_ptr()), role).is_ok();
    }
    ((*vtbl).release)(raw);
    ok
}

struct WlanClient {
    handle: HANDLE,
}

impl Drop for WlanClient {
    fn drop(&mut self) {
        unsafe {
            let _ = WlanCloseHandle(self.handle, None);
        }
    }
}

struct WlanMemory<T>(*mut T);

impl<T> Drop for WlanMemory<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { WlanFreeMemory(self.0.cast()) };
        }
    }
}

pub fn wifi_networks() -> WifiNetworks {
    unsafe {
        match wifi_networks_inner() {
            Ok(networks) => networks,
            Err(status) => WifiNetworks {
                available: false,
                networks: Vec::new(),
                status: Some(status),
            },
        }
    }
}

pub fn connect_wifi(network: &WifiNetworkInfo, password: Option<&str>) -> WifiResult<()> {
    unsafe { connect_wifi_inner(network, password) }
}

unsafe fn wifi_networks_inner() -> WifiResult<WifiNetworks> {
    let client = open_wlan_client()?;
    let Some(interface) = first_wifi_interface(client.handle)? else {
        return Ok(WifiNetworks {
            available: false,
            networks: Vec::new(),
            status: Some("未检测到 Wi-Fi 适配器".to_string()),
        });
    };

    let _ = WlanScan(client.handle, &interface.InterfaceGuid, None, None, None);

    let mut list_ptr: *mut WLAN_AVAILABLE_NETWORK_LIST = std::ptr::null_mut();
    let code = WlanGetAvailableNetworkList(
        client.handle,
        &interface.InterfaceGuid,
        0,
        None,
        &mut list_ptr,
    );
    if code != 0 {
        return Ok(WifiNetworks {
            available: true,
            networks: Vec::new(),
            status: Some(wlan_error("获取 Wi-Fi 列表失败", code)),
        });
    }
    let list = WlanMemory(list_ptr);
    let count = (*list.0).dwNumberOfItems as usize;
    let raw_networks = slice::from_raw_parts((*list.0).Network.as_ptr(), count);
    let mut networks: Vec<WifiNetworkInfo> = Vec::new();
    for raw in raw_networks {
        let Some(info) = wifi_network_info(raw) else {
            continue;
        };
        if let Some(existing) = networks
            .iter_mut()
            .find(|existing| existing.ssid == info.ssid)
        {
            if wifi_network_rank(&info) > wifi_network_rank(existing) {
                *existing = info;
            }
        } else {
            networks.push(info);
        }
    }
    networks.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.signal.cmp(&a.signal))
            .then(a.ssid.cmp(&b.ssid))
    });

    Ok(WifiNetworks {
        available: true,
        networks,
        status: None,
    })
}

unsafe fn connect_wifi_inner(network: &WifiNetworkInfo, password: Option<&str>) -> WifiResult<()> {
    if network.ssid.is_empty() {
        return Err("网络名称为空".to_string());
    }
    if !network.connectable {
        return Err("该网络当前不可连接".to_string());
    }

    let client = open_wlan_client()?;
    let Some(interface) = first_wifi_interface(client.handle)? else {
        return Err("未检测到 Wi-Fi 适配器".to_string());
    };

    let mut ssid = dot11_ssid_from_name(&network.ssid)?;
    let profile_name = if network.profile_name.is_empty() {
        network.ssid.as_str()
    } else {
        network.profile_name.as_str()
    };
    let mut profile_wide = None;
    let mut mode = wlan_connection_mode_profile;
    let mut ssid_ptr = std::ptr::null_mut();

    if network.has_profile {
        profile_wide = Some(wide(profile_name));
    } else if network.secure {
        let password = password
            .filter(|value| !value.is_empty())
            .ok_or_else(|| "请输入网络密码".to_string())?;
        let profile_xml = wifi_profile_xml(network, password)?;
        let profile_xml_wide = wide(&profile_xml);
        let mut reason_code = 0u32;
        let code = WlanSetProfile(
            client.handle,
            &interface.InterfaceGuid,
            0,
            PCWSTR(profile_xml_wide.as_ptr()),
            PCWSTR::null(),
            true,
            None,
            &mut reason_code,
        );
        if code != 0 {
            return Err(wlan_error("保存 Wi-Fi 配置失败", code));
        }
        profile_wide = Some(wide(&network.ssid));
    } else {
        mode = wlan_connection_mode_discovery_unsecure;
        ssid_ptr = &mut ssid;
    }
    let profile = profile_wide
        .as_ref()
        .map(|value| PCWSTR(value.as_ptr()))
        .unwrap_or_else(PCWSTR::null);

    let params = WLAN_CONNECTION_PARAMETERS {
        wlanConnectionMode: mode,
        strProfile: profile,
        pDot11Ssid: ssid_ptr,
        pDesiredBssidList: std::ptr::null_mut(),
        dot11BssType: dot11_BSS_type_infrastructure,
        dwFlags: 0,
    };
    let code = WlanConnect(client.handle, &interface.InterfaceGuid, &params, None);
    if code != 0 {
        return Err(wlan_error("连接 Wi-Fi 失败", code));
    }
    Ok(())
}

unsafe fn open_wlan_client() -> WifiResult<WlanClient> {
    let mut negotiated = 0u32;
    let mut handle = HANDLE::default();
    let code = WlanOpenHandle(2, None, &mut negotiated, &mut handle);
    if code != 0 {
        return Err(wlan_error("打开 WLAN 服务失败", code));
    }
    Ok(WlanClient { handle })
}

unsafe fn first_wifi_interface(handle: HANDLE) -> WifiResult<Option<WLAN_INTERFACE_INFO>> {
    let mut list_ptr: *mut WLAN_INTERFACE_INFO_LIST = std::ptr::null_mut();
    let code = WlanEnumInterfaces(handle, None, &mut list_ptr);
    if code != 0 {
        return Err(wlan_error("枚举 Wi-Fi 适配器失败", code));
    }
    let list = WlanMemory(list_ptr);
    let count = (*list.0).dwNumberOfItems as usize;
    if count == 0 {
        return Ok(None);
    }
    let interfaces = slice::from_raw_parts((*list.0).InterfaceInfo.as_ptr(), count);
    Ok(interfaces.first().copied())
}

fn wifi_network_info(raw: &WLAN_AVAILABLE_NETWORK) -> Option<WifiNetworkInfo> {
    let ssid = ssid_to_string(&raw.dot11Ssid);
    if ssid.is_empty() {
        return None;
    }
    let profile_name = fixed_utf16_to_string(&raw.strProfileName);
    let secure = raw.bSecurityEnabled.as_bool();
    Some(WifiNetworkInfo {
        ssid,
        secure,
        has_profile: raw.dwFlags & WLAN_AVAILABLE_NETWORK_HAS_PROFILE != 0,
        signal: raw.wlanSignalQuality.min(100) as u8,
        connected: raw.dwFlags & WLAN_AVAILABLE_NETWORK_CONNECTED != 0,
        connectable: raw.bNetworkConnectable.as_bool(),
        profile_name,
        auth: auth_xml(raw.dot11DefaultAuthAlgorithm)
            .unwrap_or(if secure { "unsupported" } else { "open" })
            .to_string(),
        cipher: cipher_xml(raw.dot11DefaultCipherAlgorithm)
            .unwrap_or(if secure { "unsupported" } else { "none" })
            .to_string(),
    })
}

fn wifi_network_rank(network: &WifiNetworkInfo) -> (bool, bool, u8) {
    (network.connected, network.has_profile, network.signal)
}

fn ssid_to_string(ssid: &DOT11_SSID) -> String {
    let len = ssid.uSSIDLength.min(ssid.ucSSID.len() as u32) as usize;
    String::from_utf8_lossy(&ssid.ucSSID[..len]).to_string()
}

fn fixed_utf16_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&ch| ch == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn dot11_ssid_from_name(name: &str) -> WifiResult<DOT11_SSID> {
    let bytes = name.as_bytes();
    if bytes.len() > 32 {
        return Err("Wi-Fi 名称过长".to_string());
    }
    let mut ssid = DOT11_SSID {
        uSSIDLength: bytes.len() as u32,
        ucSSID: [0; 32],
    };
    ssid.ucSSID[..bytes.len()].copy_from_slice(bytes);
    Ok(ssid)
}

fn auth_xml(auth: DOT11_AUTH_ALGORITHM) -> Option<&'static str> {
    match auth {
        DOT11_AUTH_ALGO_80211_OPEN => Some("open"),
        DOT11_AUTH_ALGO_WPA_PSK => Some("WPAPSK"),
        DOT11_AUTH_ALGO_RSNA_PSK => Some("WPA2PSK"),
        DOT11_AUTH_ALGO_WPA3_SAE => Some("WPA3SAE"),
        _ => None,
    }
}

fn cipher_xml(cipher: DOT11_CIPHER_ALGORITHM) -> Option<&'static str> {
    match cipher {
        DOT11_CIPHER_ALGO_NONE => Some("none"),
        DOT11_CIPHER_ALGO_TKIP => Some("TKIP"),
        DOT11_CIPHER_ALGO_CCMP
        | DOT11_CIPHER_ALGO_CCMP_256
        | DOT11_CIPHER_ALGO_GCMP
        | DOT11_CIPHER_ALGO_GCMP_256 => Some("AES"),
        _ => None,
    }
}

fn wifi_profile_xml(network: &WifiNetworkInfo, password: &str) -> WifiResult<String> {
    if network.auth == "unsupported" || network.cipher == "unsupported" {
        return Err("当前网络的认证方式需要打开 Windows 网络设置连接".to_string());
    }
    if !matches!(network.auth.as_str(), "WPAPSK" | "WPA2PSK" | "WPA3SAE") {
        return Err("当前网络不支持在面板内输入密码连接".to_string());
    }
    if !matches!(network.cipher.as_str(), "AES" | "TKIP") {
        return Err("当前网络的加密方式需要打开 Windows 网络设置连接".to_string());
    }
    let ssid = xml_escape(&network.ssid);
    let password = xml_escape(password);
    Ok(format!(
        r#"<?xml version="1.0"?>
<WLANProfile xmlns="http://www.microsoft.com/networking/WLAN/profile/v1">
    <name>{ssid}</name>
    <SSIDConfig>
        <SSID>
            <name>{ssid}</name>
        </SSID>
    </SSIDConfig>
    <connectionType>ESS</connectionType>
    <connectionMode>manual</connectionMode>
    <MSM>
        <security>
            <authEncryption>
                <authentication>{auth}</authentication>
                <encryption>{cipher}</encryption>
                <useOneX>false</useOneX>
            </authEncryption>
            <sharedKey>
                <keyType>passPhrase</keyType>
                <protected>false</protected>
                <keyMaterial>{password}</keyMaterial>
            </sharedKey>
        </security>
    </MSM>
</WLANProfile>"#,
        ssid = ssid,
        auth = network.auth.as_str(),
        cipher = network.cipher.as_str(),
        password = password,
    ))
}

fn xml_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn wlan_error(action: &str, code: u32) -> String {
    format!("{}（错误 {}）", action, code)
}

pub fn bluetooth_devices() -> BluetoothDevices {
    unsafe {
        match bluetooth_devices_inner() {
            Ok(devices) => devices,
            Err(status) => BluetoothDevices {
                available: false,
                radio_name: None,
                devices: Vec::new(),
                status: Some(status),
            },
        }
    }
}

pub fn input_methods() -> InputMethods {
    unsafe { input_methods_inner() }
}

pub fn select_input_method(method: &InputMethodInfo) -> bool {
    if method.hkl == 0 {
        return false;
    }
    unsafe {
        let hkl = HKL(method.hkl as *mut c_void);
        let flags = windows::Win32::UI::Input::KeyboardAndMouse::ACTIVATE_KEYBOARD_LAYOUT_FLAGS(
            KLF_ACTIVATE.0 | KLF_SETFORPROCESS.0,
        );
        if ActivateKeyboardLayout(hkl, flags).is_err() {
            return false;
        }
        let _ = PostMessageW(
            HWND_BROADCAST,
            WM_INPUTLANGCHANGEREQUEST,
            WPARAM(0),
            LPARAM(method.hkl),
        );
        true
    }
}

pub fn open_bluetooth_settings() {
    open_uri("ms-settings:bluetooth");
}

pub fn open_input_settings() {
    open_uri("ms-settings:typing");
}

unsafe fn bluetooth_devices_inner() -> BluetoothResult<BluetoothDevices> {
    let params = BLUETOOTH_FIND_RADIO_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_FIND_RADIO_PARAMS>() as u32,
    };
    let mut radio = HANDLE::default();
    let Ok(find) = BluetoothFindFirstRadio(&params, &mut radio) else {
        return Ok(BluetoothDevices {
            available: false,
            radio_name: None,
            devices: Vec::new(),
            status: Some("No Bluetooth adapter detected".to_string()),
        });
    };

    let mut radio_name = None;
    let mut devices = Vec::new();
    loop {
        if !radio.is_invalid() {
            if radio_name.is_none() {
                radio_name = bluetooth_radio_name(radio);
            }
            enumerate_bluetooth_radio_devices(radio, &mut devices);
            let _ = CloseHandle(radio);
        }
        radio = HANDLE::default();
        if BluetoothFindNextRadio(find, &mut radio).is_err() {
            break;
        }
    }
    let _ = BluetoothFindRadioClose(find);
    devices.sort_by(|a, b| {
        b.connected
            .cmp(&a.connected)
            .then(b.remembered.cmp(&a.remembered))
            .then(a.name.cmp(&b.name))
    });
    devices.dedup_by(|a, b| !a.address.is_empty() && a.address.as_str() == b.address.as_str());

    Ok(BluetoothDevices {
        available: true,
        radio_name,
        devices,
        status: None,
    })
}

unsafe fn bluetooth_radio_name(radio: HANDLE) -> Option<String> {
    let mut info = BLUETOOTH_RADIO_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_RADIO_INFO>() as u32,
        ..Default::default()
    };
    (BluetoothGetRadioInfo(radio, &mut info) == 0)
        .then(|| fixed_utf16_to_string(&info.szName))
        .filter(|name| !name.is_empty())
}

unsafe fn enumerate_bluetooth_radio_devices(radio: HANDLE, devices: &mut Vec<BluetoothDeviceInfo>) {
    let params = BLUETOOTH_DEVICE_SEARCH_PARAMS {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_SEARCH_PARAMS>() as u32,
        fReturnAuthenticated: BOOL(1),
        fReturnRemembered: BOOL(1),
        fReturnUnknown: BOOL(0),
        fReturnConnected: BOOL(1),
        fIssueInquiry: BOOL(0),
        cTimeoutMultiplier: 1,
        hRadio: radio,
    };
    let mut info = BLUETOOTH_DEVICE_INFO {
        dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
        ..Default::default()
    };
    let Ok(find) = BluetoothFindFirstDevice(&params, &mut info) else {
        return;
    };
    loop {
        if let Some(device) = bluetooth_device_info(&info) {
            devices.push(device);
        }
        info = BLUETOOTH_DEVICE_INFO {
            dwSize: std::mem::size_of::<BLUETOOTH_DEVICE_INFO>() as u32,
            ..Default::default()
        };
        if BluetoothFindNextDevice(find, &mut info).is_err() {
            break;
        }
    }
    let _ = BluetoothFindDeviceClose(find);
}

fn bluetooth_device_info(info: &BLUETOOTH_DEVICE_INFO) -> Option<BluetoothDeviceInfo> {
    let name = fixed_utf16_to_string(&info.szName);
    if name.is_empty() {
        return None;
    }
    Some(BluetoothDeviceInfo {
        name,
        address: bluetooth_address_string(info.Address),
        connected: info.fConnected.as_bool(),
        remembered: info.fRemembered.as_bool(),
        authenticated: info.fAuthenticated.as_bool(),
    })
}

fn bluetooth_address_string(address: BLUETOOTH_ADDRESS) -> String {
    let raw = unsafe { address.Anonymous.ullLong } & 0x0000_FFFF_FFFF_FFFF;
    if raw == 0 {
        String::new()
    } else {
        format!("{:012X}", raw)
    }
}

unsafe fn input_methods_inner() -> InputMethods {
    let count = GetKeyboardLayoutList(None);
    if count <= 0 {
        let current = GetKeyboardLayout(0);
        let method = input_method_from_hkl(current, true);
        return InputMethods {
            methods: method.into_iter().collect(),
            status: Some("Only the current input method was detected".to_string()),
        };
    }
    let mut hkls = vec![HKL::default(); count as usize];
    let actual = GetKeyboardLayoutList(Some(hkls.as_mut_slice()));
    hkls.truncate(actual.max(0) as usize);
    let active = GetKeyboardLayout(0);
    let mut methods = Vec::with_capacity(hkls.len());
    for hkl in hkls {
        if let Some(method) = input_method_from_hkl(hkl, hkl.0 == active.0) {
            if !methods
                .iter()
                .any(|existing: &InputMethodInfo| existing.hkl == method.hkl)
            {
                methods.push(method);
            }
        }
    }
    InputMethods {
        methods,
        status: None,
    }
}

fn input_method_from_hkl(hkl: HKL, active: bool) -> Option<InputMethodInfo> {
    let value = hkl.0 as isize;
    if value == 0 {
        return None;
    }
    let id = keyboard_layout_id(value);
    let label = keyboard_layout_text(&id).unwrap_or_else(|| locale_display_name(value as u32));
    Some(InputMethodInfo {
        id,
        label,
        active,
        hkl: value,
    })
}

fn keyboard_layout_id(hkl: isize) -> String {
    format!("{:08X}", (hkl as usize) & 0xFFFF_FFFF)
}

fn keyboard_layout_text(id: &str) -> Option<String> {
    let subkey = wide(&format!(
        "SYSTEM\\CurrentControlSet\\Control\\Keyboard Layouts\\{}",
        id
    ));
    let value = wide("Layout Text");
    let mut bytes = 0u32;
    let first = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if first.0 != 0 || bytes < 2 {
        return None;
    }
    let mut buf = vec![0u16; (bytes as usize).div_ceil(2)];
    let second = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut c_void),
            Some(&mut bytes),
        )
    };
    (second.0 == 0)
        .then(|| fixed_utf16_to_string(&buf))
        .filter(|text| !text.is_empty())
}

fn locale_display_name(hkl: u32) -> String {
    let lang_id = hkl & 0xFFFF;
    let mut locale = [0u16; 85];
    let locale_len = unsafe { LCIDToLocaleName(lang_id, Some(&mut locale), 0) };
    if locale_len > 0 {
        let mut name = [0u16; 128];
        let len = unsafe {
            GetLocaleInfoEx(
                PCWSTR(locale.as_ptr()),
                LOCALE_SLOCALIZEDDISPLAYNAME,
                Some(&mut name),
            )
        };
        if len > 0 {
            let text = fixed_utf16_to_string(&name);
            if !text.is_empty() {
                return text;
            }
        }
    }
    format!("输入法 {:08X}", hkl)
}

/// Battery snapshot for the status line. `percent` is None on a desktop with no
/// battery (we then just show the clock).
#[derive(Clone, Copy, Default)]
pub struct Battery {
    pub present: bool,
    pub charging: bool,
    pub percent: u8,
}

const BATTERY_FLAG_NO_BATTERY: u8 = 128;
const AC_LINE_ONLINE: u8 = 1;
pub fn battery() -> Battery {
    unsafe {
        let mut status = SYSTEM_POWER_STATUS::default();
        if GetSystemPowerStatus(&mut status).is_err() {
            return Battery::default();
        }
        let present =
            status.BatteryFlag & BATTERY_FLAG_NO_BATTERY == 0 && status.BatteryFlag != 255;
        Battery {
            present,
            charging: status.ACLineStatus == AC_LINE_ONLINE,
            percent: if status.BatteryLifePercent <= 100 {
                status.BatteryLifePercent
            } else {
                0
            },
        }
    }
}

/// Local wall-clock, formatted as ("16:58", "6月24日") for the status line.
pub fn clock() -> (String, String) {
    unsafe {
        let t: SYSTEMTIME = GetLocalTime();
        (
            format!("{:02}:{:02}", t.wHour, t.wMinute),
            format!("{}月{}日", t.wMonth, t.wDay),
        )
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open a Windows settings page (e.g. `ms-settings:network-status`) or any shell URI.
pub fn open_uri(uri: &str) {
    unsafe {
        let target = wide(uri);
        ShellExecuteW(
            None,
            w!("open"),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn open_sound_settings() {
    open_uri("ms-settings:sound");
}

pub fn open_battery_saver_settings() {
    open_uri("ms-settings:batterysaver");
}

pub fn open_power_settings() {
    open_uri("ms-settings:powersleep");
}

pub fn open_date_time_settings() {
    open_uri("ms-settings:dateandtime");
}

fn key_event(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Press Win + `vk`, then release — used to invoke a system shortcut.
unsafe fn tap_with_win(vk: VIRTUAL_KEY) {
    let inputs = [
        key_event(VK_LWIN, false),
        key_event(vk, false),
        key_event(vk, true),
        key_event(VK_LWIN, true),
    ];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Cycle the input method / keyboard layout (Win+Space), the system shortcut.
pub fn switch_input_method() {
    unsafe { tap_with_win(VK_SPACE) }
}
