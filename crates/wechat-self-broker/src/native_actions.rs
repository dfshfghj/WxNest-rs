use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TEXT_CONFIG_SIZE: usize = 1688;
const LOG_PATH_OFFSET: usize = 16;
const LOG_PATH_CHARS: usize = 260;
const TEXT_OFFSET: usize = 536;
const TEXT_BYTES: usize = 1024;
const TARGET_OFFSET: usize = 1560;
const TARGET_BYTES: usize = 128;
const MAX_TEXT_BYTES: usize = 1023;
const MAX_TARGET_BYTES: usize = 127;
const MOMENTS_CONFIG_SIZE: usize = 32144;
const MOMENTS_SELF_WXID_OFFSET: usize = 1560;
const MOMENTS_VISIBILITY_OFFSET: usize = 1688;
const MOMENTS_IMAGE_COUNT_OFFSET: usize = 1692;
const MOMENTS_TIMEOUT_OFFSET: usize = 1696;
const MOMENTS_PATHS_OFFSET: usize = 1704;
const MOMENTS_PATH_CHARS: usize = 1024;
const MOMENTS_TEXT_OFFSET: usize = 20136;
const MOMENTS_TEXT_BYTES: usize = 12001;
const MAX_MOMENTS_IMAGES: usize = 9;
const INTERACTION_CONFIG_SIZE: usize = 2744;
const INTERACTION_ACCOUNT_OFFSET: usize = 1560;
const INTERACTION_SNS_ID_OFFSET: usize = 1688;
const INTERACTION_TIMEOUT_OFFSET: usize = 1696;
const INTERACTION_FLAGS_OFFSET: usize = 1700;
const INTERACTION_ACTION_OFFSET: usize = 1704;
const INTERACTION_RESERVED_OFFSET: usize = 1708;
const INTERACTION_CONTENT_OFFSET: usize = 1712;
const INTERACTION_CONTENT_BYTES: usize = 1024;
const INTERACTION_OPERATION_OFFSET: usize = 2736;
const POLL_RESULT_SIZE: usize = 64;
const ATTACHMENT_ACCOUNT_OFFSET: usize = 1688;
const IMAGE_CONFIG_SIZE: usize = 1688;
const FILE_CONFIG_SIZE: usize = 1824;
const VOICE_CONFIG_SIZE: usize = 1824;
const EMOTICON_CONFIG_SIZE: usize = 1824;
const AT_TEXT_CONFIG_SIZE: usize = 2712;
const SEND_FLAG_OFFSET: usize = 1816;
const VOICE_DURATION_OFFSET: usize = 1816;
const VOICE_SEND_OFFSET: usize = 1820;
const AT_MEMBERS_OFFSET: usize = 1688;
const AT_MEMBERS_BYTES: usize = 1024;

#[allow(dead_code)]
struct BuildProfile {
    label: &'static str,
    bytes: u64,
    sha256: &'static str,
    timestamp: u32,
    image_size: u32,
    profile_id: u32,
}

#[allow(dead_code)]
const BUILDS: [BuildProfile; 2] = [
    BuildProfile {
        label: "4.1.15.13",
        bytes: 201_552_944,
        sha256: "10f8e995453e2da46d4f2b5080cd6da1f13cc5147746adc119ceae38cb039de5",
        timestamp: 0x6ab0a378,
        image_size: 0x0c0ae000,
        profile_id: 1,
    },
    BuildProfile {
        label: "4.1.15.56",
        bytes: 204_305_968,
        sha256: "e0428a7b691853de496532523c2c09aa095f90941c37c9f4cfb3f137f3c66556",
        timestamp: 0x6ad083c8,
        image_size: 0x0c34f000,
        profile_id: 2,
    },
];

pub struct NativeProvider {
    dll: PathBuf,
    work_dir: PathBuf,
    timeout: Duration,
    target_db: PathBuf,
    gate: Mutex<()>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct NativeSendResult {
    pub build: String,
    pub pid: u32,
    pub target: String,
    pub native_status: u32,
    pub completion_success: bool,
    pub thread_id: u64,
    pub elapsed_ms: u128,
}

fn valid_mentions(members: &str) -> bool {
    if members.is_empty() || members.len() >= AT_MEMBERS_BYTES {
        return false;
    }
    let mut count = 1usize;
    let mut part = 0usize;
    for value in members.bytes() {
        if value == b',' {
            if part == 0 || part >= 128 || count >= 8 {
                return false;
            }
            part = 0;
            count += 1;
        } else if value.is_ascii_alphanumeric() || matches!(value, b'_' | b'.' | b'@' | b'-') {
            part += 1;
        } else {
            return false;
        }
    }
    part > 0 && part < 128
}

fn valid_target(target: &str) -> bool {
    !target.is_empty()
        && target.len() <= MAX_TARGET_BYTES
        && target.bytes().all(|value| {
            value.is_ascii_alphanumeric() || matches!(value, b'_' | b'.' | b'@' | b'-')
        })
}

impl NativeProvider {
    pub fn new(dll: PathBuf, work_dir: PathBuf, timeout: Duration, target_db: PathBuf) -> Self {
        Self {
            dll,
            work_dir,
            timeout,
            target_db,
            gate: Mutex::new(()),
        }
    }

    pub fn publish_moments(
        &self,
        self_wxid: &str,
        text: &str,
        images: &[String],
        visibility: u32,
    ) -> Result<serde_json::Value, String> {
        if !valid_target(self_wxid) || self_wxid.contains('@') {
            return Err("moments self wxid is invalid".to_string());
        }
        if text.len() >= MOMENTS_TEXT_BYTES || text.is_empty() && images.is_empty() {
            return Err("moments text is out of range".to_string());
        }
        if images.len() > MAX_MOMENTS_IMAGES {
            return Err("too many moments images".to_string());
        }
        if visibility != 1 && visibility != 2 {
            return Err("moments visibility must be 1 or 2".to_string());
        }
        for image in images {
            let path = Path::new(image);
            if !path.is_absolute() || !path.is_file() {
                return Err(format!("moments image is invalid: {image}"));
            }
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| format!("create native work dir failed: {err}"))?;
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::publish_moments(self, self_wxid, text, images, visibility)
    }

    pub fn moments_action(
        &self,
        self_wxid: &str,
        sns_id: u64,
        action: u32,
        content: &str,
    ) -> Result<serde_json::Value, String> {
        if !valid_target(self_wxid) || self_wxid.contains('@') {
            return Err("moments self wxid is invalid".to_string());
        }
        if sns_id == 0 {
            return Err("moments feed id is empty".to_string());
        }
        if !(1..=3).contains(&action) {
            return Err("moments action must be 1, 2 or 3".to_string());
        }
        if content.len() >= INTERACTION_CONTENT_BYTES
            || (action == 2 && content.is_empty())
            || (action == 3 && (!Path::new(content).is_absolute() || !Path::new(content).is_file()))
        {
            return Err("moments action content is invalid".to_string());
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::moments_action(self, self_wxid, sns_id, action, content)
    }

    pub fn send_image(
        &self,
        self_wxid: &str,
        target: &str,
        path: &str,
    ) -> Result<serde_json::Value, String> {
        self.send_path("WxSendImage", self_wxid, target, path, "message.send_image")
    }

    pub fn send_file(
        &self,
        self_wxid: &str,
        target: &str,
        path: &str,
    ) -> Result<serde_json::Value, String> {
        self.send_path("WxSendFile", self_wxid, target, path, "message.send_file")
    }

    pub fn send_emoticon(
        &self,
        self_wxid: &str,
        target: &str,
        path: &str,
    ) -> Result<serde_json::Value, String> {
        self.send_path(
            "WxSendEmoticon",
            self_wxid,
            target,
            path,
            "message.send_emoticon",
        )
    }

    pub fn send_voice(
        &self,
        self_wxid: &str,
        target: &str,
        path: &str,
        duration_ms: u32,
    ) -> Result<serde_json::Value, String> {
        if duration_ms == 0 || duration_ms > 600_000 {
            return Err("voice duration is out of range".to_string());
        }
        if !valid_target(target) || !valid_target(self_wxid) || self_wxid.contains('@') {
            return Err("voice target or account is invalid".to_string());
        }
        let path_value = Path::new(path);
        if !path_value.is_absolute() || !path_value.is_file() {
            return Err(format!("voice path is invalid: {path}"));
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| format!("create native work dir failed: {err}"))?;
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::send_voice(self, self_wxid, target, path, duration_ms)
    }

    pub(crate) fn send_path(
        &self,
        export: &'static str,
        self_wxid: &str,
        target: &str,
        path: &str,
        action: &str,
    ) -> Result<serde_json::Value, String> {
        if !valid_target(target) {
            return Err("attachment target is invalid".to_string());
        }
        if !valid_target(self_wxid) || self_wxid.contains('@') {
            return Err("attachment account is invalid".to_string());
        }
        let path_value = Path::new(path);
        if !path_value.is_absolute() || !path_value.is_file() {
            return Err(format!("attachment path is invalid: {path}"));
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| format!("create native work dir failed: {err}"))?;
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::send_path(self, export, self_wxid, target, path, action)
    }

    pub fn send_at_text(
        &self,
        self_wxid: &str,
        target: &str,
        text: &str,
        members: &str,
    ) -> Result<serde_json::Value, String> {
        if !valid_target(target) || !target.ends_with("@chatroom") {
            return Err("at text target must be a group".to_string());
        }
        if !valid_target(self_wxid) || self_wxid.contains('@') {
            return Err("at text account is invalid".to_string());
        }
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err("at text is out of range".to_string());
        }
        if members.is_empty() || members.len() >= AT_MEMBERS_BYTES || !valid_mentions(members) {
            return Err("at text members are invalid".to_string());
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| format!("create native work dir failed: {err}"))?;
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::send_at_text(self, self_wxid, target, text, members)
    }

    pub fn send_text(&self, target: &str, text: &str) -> Result<NativeSendResult, String> {
        if !valid_target(target) {
            return Err("native target is invalid".to_string());
        }
        if text.is_empty() || text.len() > MAX_TEXT_BYTES {
            return Err("native text length is out of range".to_string());
        }
        if !self.dll.is_file() {
            return Err(format!("native bridge not found: {}", self.dll.display()));
        }
        std::fs::create_dir_all(&self.work_dir)
            .map_err(|err| format!("create native work dir failed: {err}"))?;
        let _gate = self
            .gate
            .lock()
            .map_err(|_| "native provider lock poisoned".to_string())?;
        platform::send_text(self, target, text)
    }
}

#[cfg(windows)]
mod platform {
    use super::*;
    use std::ffi::{c_void, OsString};
    use std::fs::File;
    use std::io::Read;
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use std::ptr;
    use sha2::{Digest, Sha256};
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
        MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
    use windows_sys::Win32::System::Memory::{
        VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
    };
    use windows_sys::Win32::System::RestartManager::{
        RmEndSession, RmGetList, RmRegisterResources, RmStartSession, RM_PROCESS_INFO,
    };
    use windows_sys::Win32::System::Threading::{
        CreateRemoteThread, GetExitCodeThread, OpenProcess, WaitForSingleObject,
        PROCESS_CREATE_THREAD, PROCESS_QUERY_INFORMATION, PROCESS_VM_OPERATION, PROCESS_VM_READ,
        PROCESS_VM_WRITE,
    };

    const ACCESS: u32 = PROCESS_CREATE_THREAD
        | PROCESS_QUERY_INFORMATION
        | PROCESS_VM_OPERATION
        | PROCESS_VM_WRITE
        | PROCESS_VM_READ;
    const INJECT_TIMEOUT: Duration = Duration::from_secs(10);
    const POLL_INTERVAL: Duration = Duration::from_millis(100);

    struct CoreImage {
        pid: u32,
        path: PathBuf,
        timestamp: u32,
        image_size: u32,
    }

    struct RemoteProcess {
        handle: HANDLE,
        pid: u32,
    }

    pub fn send_text(
        provider: &NativeProvider,
        target: &str,
        text: &str,
    ) -> Result<NativeSendResult, String> {
        let started = Instant::now();
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let work_dir = std::fs::canonicalize(&provider.work_dir)
            .map_err(|err| format!("canonicalize native work dir failed: {err}"))?;
        let process = open_remote(image.pid)?;
        let result = send_text_inner(
            &process,
            profile,
            &provider.dll,
            &work_dir,
            provider.timeout,
            target,
            text,
        );
        close(process.handle);
        result.map(|mut value| {
            value.elapsed_ms = started.elapsed().as_millis();
            value
        })
    }

    pub fn publish_moments(
        provider: &NativeProvider,
        self_wxid: &str,
        text: &str,
        images: &[String],
        visibility: u32,
    ) -> Result<serde_json::Value, String> {
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let work_dir = std::fs::canonicalize(&provider.work_dir)
            .map_err(|err| format!("canonicalize native work dir failed: {err}"))?;
        let process = open_remote(image.pid)?;
        let result = publish_inner(
            &process,
            profile,
            &provider.dll,
            &work_dir,
            self_wxid,
            text,
            images,
            visibility,
        );
        close(process.handle);
        result
    }

    pub fn moments_action(
        provider: &NativeProvider,
        self_wxid: &str,
        sns_id: u64,
        action: u32,
        content: &str,
    ) -> Result<serde_json::Value, String> {
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let process = open_remote(image.pid)?;
        let result = action_inner(
            &process,
            profile,
            &provider.dll,
            self_wxid,
            sns_id,
            action,
            content,
        );
        close(process.handle);
        result
    }

    pub fn send_path(
        provider: &NativeProvider,
        export: &str,
        self_wxid: &str,
        target: &str,
        path: &str,
        action: &str,
    ) -> Result<serde_json::Value, String> {
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let work_dir = std::fs::canonicalize(&provider.work_dir)
            .map_err(|err| format!("canonicalize native work dir failed: {err}"))?;
        let process = open_remote(image.pid)?;
        let result = (|| {
            let base = ensure_bridge(&process, &provider.dll)?;
            let rva = pe_export_rva(&provider.dll, export)?;
            let size = match export {
                "WxSendImage" => IMAGE_CONFIG_SIZE,
                "WxSendEmoticon" => EMOTICON_CONFIG_SIZE,
                _ => FILE_CONFIG_SIZE,
            };
            let nonce: u64 = rand::random();
            let log_path = strip_verbatim(
                &work_dir.join(format!("wce-{}-{}-{nonce:016x}.json", process.pid, export)),
            );
            let _ = std::fs::remove_file(&log_path);
            let mut config = vec![0u8; size];
            write_u32(&mut config, 0, size as u32);
            write_u32(&mut config, 4, process.pid);
            write_u64(&mut config, 8, 0);
            write_wide(
                &mut config[LOG_PATH_OFFSET..LOG_PATH_OFFSET + LOG_PATH_CHARS * 2],
                &log_path,
            )?;
            write_bytes(
                &mut config[TEXT_OFFSET..TEXT_OFFSET + TEXT_BYTES],
                path.as_bytes(),
            );
            write_bytes(
                &mut config[TARGET_OFFSET..TARGET_OFFSET + TARGET_BYTES],
                target.as_bytes(),
            );
            if size > 1688 {
                write_bytes(
                    &mut config[ATTACHMENT_ACCOUNT_OFFSET..ATTACHMENT_ACCOUNT_OFFSET + 128],
                    self_wxid.as_bytes(),
                );
                write_u32(&mut config, SEND_FLAG_OFFSET, 1);
            }
            let remote = alloc_remote(process.handle, &config)?;
            let call = remote_call(
                process.handle,
                base + rva as u64,
                remote as u64,
                INJECT_TIMEOUT,
            )?;
            free_remote(process.handle, remote);
            if call != 0 {
                return Err(format!("{export} returned {call}"));
            }
            let value = wait_moments_log(&log_path, Duration::from_secs(120))?;
            Ok(serde_json::json!({
                "build": profile.label,
                "pid": process.pid,
                "action": action,
                "path": path,
                "log": value,
            }))
        })();
        close(process.handle);
        result
    }

    pub fn send_voice(
        provider: &NativeProvider,
        self_wxid: &str,
        target: &str,
        path: &str,
        duration_ms: u32,
    ) -> Result<serde_json::Value, String> {
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let work_dir = std::fs::canonicalize(&provider.work_dir)
            .map_err(|err| format!("canonicalize native work dir failed: {err}"))?;
        let process = open_remote(image.pid)?;
        let result = (|| {
            let base = ensure_bridge(&process, &provider.dll)?;
            let rva = pe_export_rva(&provider.dll, "WxSendVoice")?;
            let nonce: u64 = rand::random();
            let log_path = strip_verbatim(
                &work_dir.join(format!("wce-voice-{}-{nonce:016x}.json", process.pid)),
            );
            let _ = std::fs::remove_file(&log_path);
            let mut config = vec![0u8; VOICE_CONFIG_SIZE];
            write_u32(&mut config, 0, VOICE_CONFIG_SIZE as u32);
            write_u32(&mut config, 4, process.pid);
            write_u64(&mut config, 8, 0);
            write_wide(
                &mut config[LOG_PATH_OFFSET..LOG_PATH_OFFSET + LOG_PATH_CHARS * 2],
                &log_path,
            )?;
            write_bytes(
                &mut config[TEXT_OFFSET..TEXT_OFFSET + TEXT_BYTES],
                path.as_bytes(),
            );
            write_bytes(
                &mut config[TARGET_OFFSET..TARGET_OFFSET + TARGET_BYTES],
                target.as_bytes(),
            );
            write_bytes(
                &mut config[ATTACHMENT_ACCOUNT_OFFSET..ATTACHMENT_ACCOUNT_OFFSET + 128],
                self_wxid.as_bytes(),
            );
            write_u32(&mut config, VOICE_DURATION_OFFSET, duration_ms);
            write_u32(&mut config, VOICE_SEND_OFFSET, 1);
            let remote = alloc_remote(process.handle, &config)?;
            let call = remote_call(
                process.handle,
                base + rva as u64,
                remote as u64,
                INJECT_TIMEOUT,
            )?;
            free_remote(process.handle, remote);
            if call != 0 {
                return Err(format!("WxSendVoice returned {call}"));
            }
            let value = wait_moments_log(&log_path, Duration::from_secs(120))?;
            Ok(serde_json::json!({
                "build": profile.label,
                "pid": process.pid,
                "action": "message.send_voice",
                "path": path,
                "duration_ms": duration_ms,
                "log": value,
            }))
        })();
        close(process.handle);
        result
    }

    pub fn send_at_text(
        provider: &NativeProvider,
        _self_wxid: &str,
        target: &str,
        text: &str,
        members: &str,
    ) -> Result<serde_json::Value, String> {
        let image = find_core_image(&provider.target_db)?;
        let profile = verify_build(&image)?;
        let work_dir = std::fs::canonicalize(&provider.work_dir)
            .map_err(|err| format!("canonicalize native work dir failed: {err}"))?;
        let process = open_remote(image.pid)?;
        let result = (|| {
            let base = ensure_bridge(&process, &provider.dll)?;
            let rva = pe_export_rva(&provider.dll, "WxSendAtText")?;
            let nonce: u64 = rand::random();
            let log_path =
                strip_verbatim(&work_dir.join(format!("wce-at-{}-{nonce:016x}.json", process.pid)));
            let _ = std::fs::remove_file(&log_path);
            let mut config = vec![0u8; AT_TEXT_CONFIG_SIZE];
            write_u32(&mut config, 0, AT_TEXT_CONFIG_SIZE as u32);
            write_u32(&mut config, 4, process.pid);
            write_u64(&mut config, 8, 0);
            write_wide(
                &mut config[LOG_PATH_OFFSET..LOG_PATH_OFFSET + LOG_PATH_CHARS * 2],
                &log_path,
            )?;
            write_bytes(
                &mut config[TEXT_OFFSET..TEXT_OFFSET + TEXT_BYTES],
                text.as_bytes(),
            );
            write_bytes(
                &mut config[TARGET_OFFSET..TARGET_OFFSET + TARGET_BYTES],
                target.as_bytes(),
            );
            write_bytes(
                &mut config[AT_MEMBERS_OFFSET..AT_MEMBERS_OFFSET + AT_MEMBERS_BYTES],
                members.as_bytes(),
            );
            let remote = alloc_remote(process.handle, &config)?;
            let call = remote_call(
                process.handle,
                base + rva as u64,
                remote as u64,
                INJECT_TIMEOUT,
            )?;
            free_remote(process.handle, remote);
            if call != 0 {
                return Err(format!("WxSendAtText returned {call}"));
            }
            let value = wait_completion(&log_path, Duration::from_secs(120))?;
            Ok(serde_json::json!({
                "build": profile.label,
                "pid": process.pid,
                "action": "message.send_at_text",
                "target": target,
                "members": members,
                "native_status": value.native_status,
                "completion_success": value.completion_success,
            }))
        })();
        close(process.handle);
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_inner(
        process: &RemoteProcess,
        profile: &'static BuildProfile,
        dll: &Path,
        work_dir: &Path,
        self_wxid: &str,
        text: &str,
        images: &[String],
        visibility: u32,
    ) -> Result<serde_json::Value, String> {
        let base = ensure_bridge(process, dll)?;
        let rva = pe_export_rva(dll, "WxPublishMoments")?;
        let nonce: u64 = rand::random();
        let log_path = strip_verbatim(
            &work_dir.join(format!("wce-moments-{}-{nonce:016x}.json", process.pid)),
        );
        let _ = std::fs::remove_file(&log_path);
        let mut config = vec![0u8; MOMENTS_CONFIG_SIZE];
        write_u32(&mut config, 0, MOMENTS_CONFIG_SIZE as u32);
        write_u32(&mut config, 4, process.pid);
        write_u64(&mut config, 8, 0);
        write_wide(
            &mut config[LOG_PATH_OFFSET..LOG_PATH_OFFSET + LOG_PATH_CHARS * 2],
            &log_path,
        )?;
        write_bytes(
            &mut config[MOMENTS_SELF_WXID_OFFSET..MOMENTS_SELF_WXID_OFFSET + 128],
            self_wxid.as_bytes(),
        );
        write_u32(&mut config, MOMENTS_VISIBILITY_OFFSET, visibility);
        write_u32(&mut config, MOMENTS_IMAGE_COUNT_OFFSET, images.len() as u32);
        write_u32(&mut config, MOMENTS_TIMEOUT_OFFSET, 30_000);
        for (index, image) in images.iter().enumerate() {
            let offset = MOMENTS_PATHS_OFFSET + index * MOMENTS_PATH_CHARS * 2;
            write_wide(
                &mut config[offset..offset + MOMENTS_PATH_CHARS * 2],
                &strip_verbatim(Path::new(image)),
            )?;
        }
        write_bytes(
            &mut config[MOMENTS_TEXT_OFFSET..MOMENTS_TEXT_OFFSET + MOMENTS_TEXT_BYTES],
            text.as_bytes(),
        );
        let remote = alloc_remote(process.handle, &config)?;
        let call = remote_call(
            process.handle,
            base + rva as u64,
            remote as u64,
            INJECT_TIMEOUT,
        );
        if call.is_ok() {
            free_remote(process.handle, remote);
        }
        let call = call?;
        if call != 0 {
            return Err(format!("WxPublishMoments returned {call}"));
        }
        let value = wait_moments_log(&log_path, Duration::from_secs(120))?;
        Ok(serde_json::json!({
            "build": profile.label,
            "pid": process.pid,
            "visibility": visibility,
            "images": images.len(),
            "log": value,
        }))
    }

    fn action_inner(
        process: &RemoteProcess,
        profile: &'static BuildProfile,
        dll: &Path,
        self_wxid: &str,
        sns_id: u64,
        action: u32,
        content: &str,
    ) -> Result<serde_json::Value, String> {
        let base = ensure_bridge(process, dll)?;
        let start_rva = pe_export_rva(dll, "WxMomentsInteractionStart")?;
        let poll_rva = pe_export_rva(dll, "WxMomentsInteractionPoll")?;
        let close_rva = pe_export_rva(dll, "WxMomentsInteractionClose")?;
        let operation: u64 = rand::random();
        let mut config = vec![0u8; INTERACTION_CONFIG_SIZE];
        write_u32(&mut config, 0, INTERACTION_CONFIG_SIZE as u32);
        write_u32(&mut config, 4, process.pid);
        write_bytes(
            &mut config[INTERACTION_ACCOUNT_OFFSET..INTERACTION_ACCOUNT_OFFSET + 128],
            self_wxid.as_bytes(),
        );
        write_u64(&mut config, INTERACTION_SNS_ID_OFFSET, sns_id);
        write_u32(&mut config, INTERACTION_TIMEOUT_OFFSET, 30_000);
        write_u32(&mut config, INTERACTION_FLAGS_OFFSET, 0);
        write_u32(&mut config, INTERACTION_ACTION_OFFSET, action);
        write_u32(&mut config, INTERACTION_RESERVED_OFFSET, 0);
        write_bytes(
            &mut config[INTERACTION_CONTENT_OFFSET
                ..INTERACTION_CONTENT_OFFSET + INTERACTION_CONTENT_BYTES],
            content.as_bytes(),
        );
        write_u64(&mut config, INTERACTION_OPERATION_OFFSET, operation);
        let remote_config = alloc_remote(process.handle, &config)?;
        let start = remote_call(
            process.handle,
            base + start_rva as u64,
            remote_config as u64,
            INJECT_TIMEOUT,
        )?;
        free_remote(process.handle, remote_config);
        if start != 0 {
            return Err(format!("WxMomentsInteractionStart returned {start}"));
        }
        let poll_remote = alloc_remote(process.handle, &[0u8; POLL_RESULT_SIZE])?;
        let deadline = Instant::now() + Duration::from_secs(40);
        let mut state;
        let mut snapshot = [0u8; POLL_RESULT_SIZE];
        loop {
            std::thread::sleep(Duration::from_millis(250));
            write_u32(&mut snapshot, 0, POLL_RESULT_SIZE as u32);
            write_u64(&mut snapshot, 56, operation);
            write_remote(process.handle, poll_remote, &snapshot)?;
            let poll = remote_call(
                process.handle,
                base + poll_rva as u64,
                poll_remote as u64,
                INJECT_TIMEOUT,
            )?;
            if poll != 0 {
                let _ = remote_call(process.handle, base + close_rva as u64, 0, INJECT_TIMEOUT);
                free_remote(process.handle, poll_remote);
                return Err(format!("WxMomentsInteractionPoll returned {poll}"));
            }
            read_remote(process.handle, poll_remote, &mut snapshot)?;
            state = read_u32(&snapshot, 4).unwrap_or(0);
            if matches!(state, 6 | 7 | 8 | 9 | 11 | 12 | 13) {
                break;
            }
            if Instant::now() >= deadline {
                let _ = remote_call(process.handle, base + close_rva as u64, 0, INJECT_TIMEOUT);
                free_remote(process.handle, poll_remote);
                return Err("moments interaction timed out".to_string());
            }
        }
        let _ = remote_call(process.handle, base + close_rva as u64, 0, INJECT_TIMEOUT);
        free_remote(process.handle, poll_remote);
        let success = matches!(state, 11 | 13);
        Ok(serde_json::json!({
            "build": profile.label,
            "pid": process.pid,
            "feed_id": sns_id,
            "action": action,
            "state": state,
            "success": success,
            "attempted": read_u32(&snapshot, 32).unwrap_or(0),
            "native_known": read_u32(&snapshot, 36).unwrap_or(0),
            "native_value": read_u32(&snapshot, 40).unwrap_or(0),
            "was_liked": read_u32(&snapshot, 44).unwrap_or(0),
            "native_status": read_u32(&snapshot, 48).unwrap_or(0),
            "exception": read_u32(&snapshot, 52).unwrap_or(0),
        }))
    }

    fn wait_moments_log(path: &Path, timeout: Duration) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if value.get("completion_success").is_some() {
                        return Ok(value);
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err("moments publish timed out".to_string());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn write_remote(process: HANDLE, address: *mut u8, bytes: &[u8]) -> Result<(), String> {
        let mut written = 0usize;
        let ok = unsafe {
            WriteProcessMemory(
                process,
                address as *const c_void,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
                &mut written,
            )
        };
        if ok == 0 || written != bytes.len() {
            return Err("WriteProcessMemory failed".to_string());
        }
        Ok(())
    }

    fn read_remote(process: HANDLE, address: *mut u8, bytes: &mut [u8]) -> Result<(), String> {
        let mut read = 0usize;
        let ok = unsafe {
            windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory(
                process,
                address as *const c_void,
                bytes.as_mut_ptr() as *mut c_void,
                bytes.len(),
                &mut read,
            )
        };
        if ok == 0 || read != bytes.len() {
            return Err("ReadProcessMemory failed".to_string());
        }
        Ok(())
    }

    fn send_text_inner(
        process: &RemoteProcess,
        profile: &'static BuildProfile,
        dll: &Path,
        work_dir: &Path,
        timeout: Duration,
        target: &str,
        text: &str,
    ) -> Result<NativeSendResult, String> {
        let base = ensure_bridge(process, dll)?;
        let send_rva = pe_export_rva(dll, "WxSendText")?;
        let inspect_rva = pe_export_rva(dll, "WxInspectCoreVersion")?;
        let inspect = remote_call(process.handle, base + inspect_rva as u64, 0, INJECT_TIMEOUT)?;
        if inspect != profile.profile_id {
            return Err(format!(
                "native bridge selected profile {inspect}, expected {}",
                profile.profile_id
            ));
        }
        let nonce: u64 = rand::random();
        let log_path = work_dir.join(format!("wce-native-{}-{nonce:016x}.json", process.pid));
        let log_path = strip_verbatim(&log_path);
        let _ = std::fs::remove_file(&log_path);
        let mut config = vec![0u8; TEXT_CONFIG_SIZE];
        write_u32(&mut config, 0, TEXT_CONFIG_SIZE as u32);
        write_u32(&mut config, 4, process.pid);
        write_u64(&mut config, 8, 0);
        write_wide(
            &mut config[LOG_PATH_OFFSET..LOG_PATH_OFFSET + LOG_PATH_CHARS * 2],
            &log_path,
        )?;
        write_bytes(
            &mut config[TEXT_OFFSET..TEXT_OFFSET + TEXT_BYTES],
            text.as_bytes(),
        );
        write_bytes(
            &mut config[TARGET_OFFSET..TARGET_OFFSET + TARGET_BYTES],
            target.as_bytes(),
        );
        let mut call = 0;
        for attempt in 0..3 {
            let remote = alloc_remote(process.handle, &config)?;
            call = remote_call(
                process.handle,
                base + send_rva as u64,
                remote as u64,
                INJECT_TIMEOUT,
            )?;
            free_remote(process.handle, remote);
            if call != 170 || attempt == 2 {
                break;
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        if call != 0 {
            return Err(format!("WxSendText returned {call}"));
        }
        let completion = wait_completion(&log_path, timeout)?;
        Ok(NativeSendResult {
            build: profile.label.to_string(),
            pid: process.pid,
            target: target.to_string(),
            native_status: completion.native_status,
            completion_success: completion.completion_success,
            thread_id: completion.thread_id,
            elapsed_ms: 0,
        })
    }

    struct Completion {
        native_status: u32,
        completion_success: bool,
        thread_id: u64,
    }

    fn wait_completion(path: &Path, timeout: Duration) -> Result<Completion, String> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(value) = serde_json::from_str::<serde_json::Value>(&text) {
                    if value.get("stage").and_then(|v| v.as_u64()) == Some(21) {
                        return Ok(Completion {
                            native_status: value
                                .get("native_status")
                                .and_then(|v| v.as_u64())
                                .unwrap_or(1) as u32,
                            completion_success: value
                                .get("completion_success")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                            thread_id: value.get("thread_id").and_then(|v| v.as_u64()).unwrap_or(0),
                        });
                    }
                }
            }
            if Instant::now() >= deadline {
                return Err("native send completion timed out".to_string());
            }
            std::thread::sleep(POLL_INTERVAL);
        }
    }

    fn pids_using_file(path: &Path) -> Vec<u32> {
        if path.as_os_str().is_empty() {
            return Vec::new();
        }
        let mut session = 0u32;
        let mut key = [0u16; 33];
        if unsafe { RmStartSession(&mut session, 0, key.as_mut_ptr()) } != 0 {
            return Vec::new();
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let files = [wide.as_ptr()];
        let registered = unsafe {
            RmRegisterResources(
                session,
                1,
                files.as_ptr(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
            )
        };
        let mut pids = Vec::new();
        if registered == 0 {
            let mut needed = 0u32;
            let mut count = 0u32;
            let mut reasons = 0u32;
            let mut infos: Vec<RM_PROCESS_INFO> =
                vec![unsafe { std::mem::zeroed() }; 16];
            let mut status = unsafe {
                RmGetList(
                    session,
                    &mut needed,
                    &mut count,
                    infos.as_mut_ptr(),
                    &mut reasons,
                )
            };
            if status == 234 && needed > 0 {
                infos = vec![unsafe { std::mem::zeroed() }; needed as usize];
                count = needed;
                status = unsafe {
                    RmGetList(
                        session,
                        &mut needed,
                        &mut count,
                        infos.as_mut_ptr(),
                        &mut reasons,
                    )
                };
            }
            if status == 0 {
                for info in infos.iter().take(count as usize) {
                    pids.push(info.Process.dwProcessId);
                }
            }
        }
        unsafe { RmEndSession(session) };
        pids
    }

    fn find_core_image(target_db: &Path) -> Result<CoreImage, String> {
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return Err("process snapshot failed".to_string());
        }
        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found: Vec<CoreImage> = Vec::new();
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            let name = wide_string(&entry.szExeFile);
            if name.eq_ignore_ascii_case("Weixin.exe") || name.eq_ignore_ascii_case("WeChat.exe") {
                if let Some(image) = core_image(entry.th32ProcessID) {
                    if !found.iter().any(|item| item.pid == image.pid) {
                        found.push(image);
                    }
                }
            }
            ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        close(snapshot);
        if found.len() == 1 {
            return Ok(found.remove(0));
        }
        if found.is_empty() {
            return Err("no Weixin.exe process with Weixin.dll found".to_string());
        }
        let owners = pids_using_file(target_db);
        let matched: Vec<CoreImage> = found
            .into_iter()
            .filter(|item| owners.contains(&item.pid))
            .collect();
        let pids: Vec<String> = matched.iter().map(|item| item.pid.to_string()).collect();
        match matched.len() {
            1 => Ok(matched.into_iter().next().unwrap()),
            0 => Err(format!(
                "multiple Weixin processes found and none owns {}; pids using file: {:?}",
                target_db.display(),
                owners
            )),
            _ => Err(format!(
                "multiple Weixin processes own the target database: {}",
                pids.join(",")
            )),
        }
    }

    fn core_image(pid: u32) -> Option<CoreImage> {
        let snapshot =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut result = None;
        let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            if wide_string(&entry.szModule).eq_ignore_ascii_case("Weixin.dll") {
                let path = PathBuf::from(OsString::from_wide(trim_nul(&entry.szExePath)));
                if let Some((timestamp, image_size)) = pe_identity(&path) {
                    result = Some(CoreImage {
                        pid,
                        path,
                        timestamp,
                        image_size,
                    });
                }
                break;
            }
            ok = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
        }
        close(snapshot);
        result
    }

    fn verify_build(image: &CoreImage) -> Result<&'static BuildProfile, String> {
        let (bytes, digest) = file_digest(&image.path)?;
        for profile in &BUILDS {
            if bytes == profile.bytes
                && digest == profile.sha256
                && image.timestamp == profile.timestamp
                && image.image_size == profile.image_size
            {
                return Ok(profile);
            }
        }
        Err(format!(
            "unsupported Weixin.dll build: {} bytes sha256 {}",
            bytes, digest
        ))
    }

    fn open_remote(pid: u32) -> Result<RemoteProcess, String> {
        let handle = unsafe { OpenProcess(ACCESS, 0, pid) };
        if handle.is_null() {
            return Err(format!("OpenProcess({pid}) failed"));
        }
        Ok(RemoteProcess { handle, pid })
    }

    fn ensure_bridge(process: &RemoteProcess, dll: &Path) -> Result<u64, String> {
        if let Some(base) = remote_module_base(process.pid, "text_bridge.dll") {
            return Ok(base);
        }
        let path = dll
            .canonicalize()
            .map_err(|err| format!("canonicalize bridge path failed: {err}"))?;
        let wide = wide_path(&strip_verbatim(&path))?;
        let remote = alloc_remote(process.handle, &wide)?;
        let load = remote_load_library(process.pid)?;
        remote_call(process.handle, load, remote as u64, INJECT_TIMEOUT)?;
        free_remote(process.handle, remote);
        remote_module_base(process.pid, "text_bridge.dll")
            .ok_or_else(|| "text_bridge.dll did not appear in the target module list".to_string())
    }

    fn remote_module_base(pid: u32, name: &str) -> Option<u64> {
        let snapshot =
            unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid) };
        if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
            return None;
        }
        let mut entry: MODULEENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
        let mut base = None;
        let mut ok = unsafe { Module32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            if wide_string(&entry.szModule).eq_ignore_ascii_case(name) {
                base = Some(entry.modBaseAddr as u64);
                break;
            }
            ok = unsafe { Module32NextW(snapshot, &mut entry) } != 0;
        }
        close(snapshot);
        base
    }

    fn remote_load_library(pid: u32) -> Result<u64, String> {
        let local = unsafe { GetModuleHandleW(wide_ptr("kernel32.dll").as_ptr()) };
        if local.is_null() {
            return Err("GetModuleHandleW(kernel32.dll) failed".to_string());
        }
        let load = unsafe { GetProcAddress(local, c"LoadLibraryW".as_ptr() as *const u8) }
            .ok_or_else(|| "GetProcAddress(LoadLibraryW) failed".to_string())?
            as usize as u64;
        let rva = load - local as u64;
        let remote = remote_module_base(pid, "kernel32.dll")
            .ok_or_else(|| "kernel32.dll is not loaded in the target".to_string())?;
        Ok(remote + rva)
    }

    fn remote_call(
        process: HANDLE,
        address: u64,
        parameter: u64,
        timeout: Duration,
    ) -> Result<u32, String> {
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            unsafe { std::mem::transmute(address as usize) };
        let thread = unsafe {
            CreateRemoteThread(
                process,
                ptr::null(),
                0,
                Some(start),
                parameter as *const c_void,
                0,
                ptr::null_mut(),
            )
        };
        if thread.is_null() {
            return Err("CreateRemoteThread failed".to_string());
        }
        let wait = unsafe { WaitForSingleObject(thread, timeout.as_millis() as u32) };
        if wait != WAIT_OBJECT_0 {
            close(thread);
            return Err("remote call timed out".to_string());
        }
        let mut code = 0u32;
        let ok = unsafe { GetExitCodeThread(thread, &mut code) };
        close(thread);
        if ok == 0 {
            return Err("GetExitCodeThread failed".to_string());
        }
        Ok(code)
    }

    fn alloc_remote(process: HANDLE, bytes: &[u8]) -> Result<*mut u8, String> {
        let memory = unsafe {
            VirtualAllocEx(
                process,
                ptr::null(),
                bytes.len(),
                MEM_COMMIT | MEM_RESERVE,
                PAGE_READWRITE,
            )
        };
        if memory.is_null() {
            return Err("VirtualAllocEx failed".to_string());
        }
        let mut written = 0usize;
        let ok = unsafe {
            WriteProcessMemory(
                process,
                memory,
                bytes.as_ptr() as *const c_void,
                bytes.len(),
                &mut written,
            )
        };
        if ok == 0 || written != bytes.len() {
            free_remote(process, memory as *mut u8);
            return Err("WriteProcessMemory failed".to_string());
        }
        Ok(memory as *mut u8)
    }

    fn free_remote(process: HANDLE, memory: *mut u8) {
        unsafe {
            VirtualFreeEx(process, memory as *mut c_void, 0, MEM_RELEASE);
        }
    }

    fn file_digest(path: &Path) -> Result<(u64, String), String> {
        let mut file = File::open(path).map_err(|err| format!("open Weixin.dll failed: {err}"))?;
        let mut hasher = Sha256::new();
        let mut buffer = vec![0u8; 1 << 20];
        let mut total = 0u64;
        loop {
            let read = file
                .read(&mut buffer)
                .map_err(|err| format!("read Weixin.dll failed: {err}"))?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
            total += read as u64;
        }
        Ok((total, hex::encode(hasher.finalize())))
    }

    fn pe_identity(path: &Path) -> Option<(u32, u32)> {
        let mut file = File::open(path).ok()?;
        let mut buffer = [0u8; 4096];
        let read = file.read(&mut buffer).ok()?;
        let data = &buffer[..read];
        let lfanew = read_u32(data, 0x3c)? as usize;
        if data.get(lfanew..lfanew + 4)? != b"PE\0\0" {
            return None;
        }
        let coff = lfanew + 4;
        let timestamp = read_u32(data, coff + 4)?;
        let opt = coff + 20;
        let magic = read_u16(data, opt)?;
        let image_size_offset = match magic {
            0x20b => opt + 56,
            0x10b => opt + 56,
            _ => return None,
        };
        Some((timestamp, read_u32(data, image_size_offset)?))
    }

    fn pe_export_rva(path: &Path, name: &str) -> Result<u32, String> {
        let data = std::fs::read(path).map_err(|err| format!("read bridge failed: {err}"))?;
        let lfanew = read_u32(&data, 0x3c).ok_or("PE header missing")? as usize;
        if data.get(lfanew..lfanew + 4) != Some(b"PE\0\0") {
            return Err("bridge is not a PE file".to_string());
        }
        let coff = lfanew + 4;
        let sections = read_u16(&data, coff + 2).ok_or("PE section count missing")? as usize;
        let opt_size = read_u16(&data, coff + 16).ok_or("PE optional size missing")? as usize;
        let opt = coff + 20;
        let magic = read_u16(&data, opt).ok_or("PE optional header missing")?;
        let directories = match magic {
            0x20b => opt + 112,
            0x10b => opt + 96,
            _ => return Err("unsupported PE format".to_string()),
        };
        let export_rva = read_u32(&data, directories).ok_or("export directory missing")? as usize;
        if export_rva == 0 {
            return Err("bridge has no export directory".to_string());
        }
        let table = opt + opt_size;
        let to_offset = |rva: usize| -> Option<usize> {
            for index in 0..sections {
                let section = table + index * 40;
                let virtual_size = read_u32(&data, section + 8)? as usize;
                let virtual_address = read_u32(&data, section + 12)? as usize;
                let raw_size = read_u32(&data, section + 16)? as usize;
                let raw_pointer = read_u32(&data, section + 20)? as usize;
                let span = virtual_size.max(raw_size);
                if rva >= virtual_address && rva < virtual_address + span {
                    return Some(raw_pointer + (rva - virtual_address));
                }
            }
            None
        };
        let export = to_offset(export_rva).ok_or("export directory is outside sections")?;
        let count = read_u32(&data, export + 24).ok_or("export name count missing")? as usize;
        let functions =
            to_offset(read_u32(&data, export + 28).ok_or("export functions missing")? as usize)
                .ok_or("export functions outside sections")?;
        let names = to_offset(read_u32(&data, export + 32).ok_or("export names missing")? as usize)
            .ok_or("export names outside sections")?;
        let ordinals =
            to_offset(read_u32(&data, export + 36).ok_or("export ordinals missing")? as usize)
                .ok_or("export ordinals outside sections")?;
        for index in 0..count {
            let name_rva =
                read_u32(&data, names + index * 4).ok_or("export name entry missing")? as usize;
            let name_offset = to_offset(name_rva).ok_or("export name outside sections")?;
            let end = data[name_offset..]
                .iter()
                .position(|value| *value == 0)
                .ok_or("export name is not terminated")?;
            if &data[name_offset..name_offset + end] == name.as_bytes() {
                let ordinal =
                    read_u16(&data, ordinals + index * 2).ok_or("export ordinal missing")? as usize;
                return read_u32(&data, functions + ordinal * 4)
                    .ok_or_else(|| format!("export {name} has no address"));
            }
        }
        Err(format!("export {name} not found"))
    }

    fn write_wide(target: &mut [u8], path: &Path) -> Result<(), String> {
        let wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.len() + 1 > LOG_PATH_CHARS {
            return Err("native log path is too long".to_string());
        }
        for (index, value) in wide.iter().enumerate() {
            let offset = index * 2;
            target[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
        }
        Ok(())
    }

    fn write_bytes(target: &mut [u8], bytes: &[u8]) {
        let count = bytes.len().min(target.len().saturating_sub(1));
        target[..count].copy_from_slice(&bytes[..count]);
    }

    fn write_u32(target: &mut [u8], offset: usize, value: u32) {
        target[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn write_u64(target: &mut [u8], offset: usize, value: u64) {
        target[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn read_u16(data: &[u8], offset: usize) -> Option<u16> {
        Some(u16::from_le_bytes(
            data.get(offset..offset + 2)?.try_into().ok()?,
        ))
    }

    fn read_u32(data: &[u8], offset: usize) -> Option<u32> {
        Some(u32::from_le_bytes(
            data.get(offset..offset + 4)?.try_into().ok()?,
        ))
    }

    fn wide_string(value: &[u16]) -> String {
        OsString::from_wide(trim_nul(value))
            .to_string_lossy()
            .to_string()
    }

    fn trim_nul(value: &[u16]) -> &[u16] {
        let end = value
            .iter()
            .position(|item| *item == 0)
            .unwrap_or(value.len());
        &value[..end]
    }

    fn wide_path(path: &Path) -> Result<Vec<u8>, String> {
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let mut bytes = Vec::with_capacity(wide.len() * 2);
        for value in wide {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Ok(bytes)
    }

    fn wide_ptr(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(std::iter::once(0)).collect()
    }

    fn strip_verbatim(path: &Path) -> PathBuf {
        let text = path.to_string_lossy();
        match text.strip_prefix(r"\?\") {
            Some(rest) => PathBuf::from(rest),
            None => path.to_path_buf(),
        }
    }

    fn close(handle: HANDLE) {
        if !handle.is_null() {
            unsafe {
                CloseHandle(handle);
            }
        }
    }
    #[cfg(test)]
    mod tests {
        use super::*;

        fn system_dll(name: &str) -> PathBuf {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
            PathBuf::from(root).join("System32").join(name)
        }

        #[test]
        fn parses_system_dll_exports() {
            let path = system_dll("version.dll");
            assert!(pe_export_rva(&path, "GetFileVersionInfoW").is_ok());
            assert!(pe_export_rva(&path, "MissingExportForTest").is_err());
        }

        #[test]
        fn reads_pe_identity() {
            let path = system_dll("version.dll");
            let (timestamp, image_size) = pe_identity(&path).unwrap();
            assert!(timestamp > 0);
            assert!(image_size > 0);
        }
    }
}

#[cfg(not(windows))]
mod platform {
    use super::*;

    pub fn send_text(
        _provider: &NativeProvider,
        _target: &str,
        _text: &str,
    ) -> Result<NativeSendResult, String> {
        Err("native actions are only available on Windows".to_string())
    }

    pub fn publish_moments(
        _provider: &NativeProvider,
        _self_wxid: &str,
        _text: &str,
        _images: &[String],
        _visibility: u32,
    ) -> Result<serde_json::Value, String> {
        Err("native actions are only available on Windows".to_string())
    }

    pub fn moments_action(
        _provider: &NativeProvider,
        _self_wxid: &str,
        _sns_id: u64,
        _action: u32,
        _content: &str,
    ) -> Result<serde_json::Value, String> {
        Err("native actions are only available on Windows".to_string())
    }

    pub fn send_path(
        _provider: &NativeProvider,
        _export: &str,
        _self_wxid: &str,
        _target: &str,
        _path: &str,
        _action: &str,
    ) -> Result<serde_json::Value, String> {
        Err("native actions are only available on Windows".to_string())
    }

    pub fn send_voice(
        _provider: &NativeProvider,
        _self_wxid: &str,
        _target: &str,
        _path: &str,
        _duration_ms: u32,
    ) -> Result<serde_json::Value, String> {
        Err("native actions are only available on Windows".to_string())
    }

    pub fn send_at_text(
        _provider: &NativeProvider,
        _self_wxid: &str,
        _target: &str,
        _text: &str,
        _members: &str,
    ) -> Result<serde_json::Value, String> {
        Err("native actions are only available on Windows".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_config_layout_matches_bridge() {
        assert_eq!(TEXT_CONFIG_SIZE, 1560 + TARGET_BYTES);
        assert_eq!(LOG_PATH_OFFSET + LOG_PATH_CHARS * 2, TEXT_OFFSET);
        assert_eq!(TEXT_OFFSET + TEXT_BYTES, TARGET_OFFSET);
        assert_eq!(TARGET_OFFSET + TARGET_BYTES, TEXT_CONFIG_SIZE);
    }

    #[test]
    fn moments_config_layout_matches_bridge() {
        assert_eq!(MOMENTS_CONFIG_SIZE, 32144);
        assert_eq!(MOMENTS_SELF_WXID_OFFSET, 1560);
        assert_eq!(MOMENTS_VISIBILITY_OFFSET, 1688);
        assert_eq!(MOMENTS_IMAGE_COUNT_OFFSET, 1692);
        assert_eq!(MOMENTS_TIMEOUT_OFFSET, 1696);
        assert_eq!(MOMENTS_PATHS_OFFSET, 1704);
        assert_eq!(
            MOMENTS_PATHS_OFFSET + 9 * MOMENTS_PATH_CHARS * 2,
            MOMENTS_TEXT_OFFSET
        );
        assert_eq!(
            MOMENTS_TEXT_OFFSET + MOMENTS_TEXT_BYTES,
            MOMENTS_CONFIG_SIZE - 7
        );
    }

    #[test]
    fn interaction_config_layout_matches_bridge() {
        assert_eq!(INTERACTION_CONFIG_SIZE, 2744);
        assert_eq!(INTERACTION_ACCOUNT_OFFSET, 1560);
        assert_eq!(INTERACTION_SNS_ID_OFFSET, 1688);
        assert_eq!(INTERACTION_TIMEOUT_OFFSET, 1696);
        assert_eq!(INTERACTION_ACTION_OFFSET, 1704);
        assert_eq!(INTERACTION_CONTENT_OFFSET, 1712);
        assert_eq!(INTERACTION_CONTENT_OFFSET + INTERACTION_CONTENT_BYTES, 2736);
        assert_eq!(INTERACTION_OPERATION_OFFSET, 2736);
        assert_eq!(POLL_RESULT_SIZE, 64);
    }

    #[test]
    fn validates_targets() {
        assert!(valid_target("filehelper"));
        assert!(valid_target("wxid_test"));
        assert!(valid_target("123@chatroom"));
        assert!(!valid_target(""));
        assert!(!valid_target("bad target"));
    }
}
