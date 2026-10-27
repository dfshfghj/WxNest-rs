use std::collections::HashSet;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use memchr::memmem;
use wechat_core::image_key::{scan_v2_templates, trusted_xor_for_verified_aes_key, V2Template};
use wechat_core::sqlcipher::{read_page1, resolve_page1_key_material, KeyMode};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Memory::{
    VirtualQueryEx, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_IMAGE, MEM_PRIVATE,
    PAGE_EXECUTE_READ, PAGE_EXECUTE_READWRITE, PAGE_EXECUTE_WRITECOPY, PAGE_GUARD, PAGE_NOACCESS,
    PAGE_READONLY, PAGE_READWRITE, PAGE_WRITECOPY,
};
use windows_sys::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, IsWindowVisible,
};

use crate::{ImageKeyMatch, KeyScanError, RecoveredKey, Result};

const CHUNK_SIZE: usize = 1024 * 1024;
const READABLE: u32 = PAGE_READONLY
    | PAGE_READWRITE
    | PAGE_WRITECOPY
    | PAGE_EXECUTE_READ
    | PAGE_EXECUTE_READWRITE
    | PAGE_EXECUTE_WRITECOPY;
const MAX_SCAN_TIME: Duration = Duration::from_secs(120);
const IMAGE_CHUNK_SIZE: usize = 4 * 1024 * 1024;
const IMAGE_CHUNK_OVERLAP: usize = 68;
const MAX_IMAGE_REGION_SIZE: usize = 50 * 1024 * 1024;
const WRITABLE: u32 =
    PAGE_READWRITE | PAGE_WRITECOPY | PAGE_EXECUTE_READWRITE | PAGE_EXECUTE_WRITECOPY;

type Region = (usize, usize);
type RegionSet = (Vec<Region>, Vec<Region>);
type KeyBatch = (Vec<[u8; 32]>, Vec<[u8; 32]>);

const STUB_NEEDLE: [u8; 26] = [
    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x20, 0, 0, 0, 0, 0, 0, 0, 0x2f, 0, 0, 0, 0, 0, 0, 0,
];

#[derive(Clone, Copy)]
struct SendHandle(HANDLE);

unsafe impl Send for SendHandle {}
unsafe impl Sync for SendHandle {}

pub fn recover_key_for_db_storage(
    db_storage: &Path,
    wechat_dir: Option<&Path>,
) -> Result<RecoveredKey> {
    let probe = probe_database(db_storage).ok_or_else(|| {
        KeyScanError::NotFound(format!("{}/session/session.db", db_storage.display()))
    })?;
    let page1 = read_page1(&probe).map_err(|error| KeyScanError::Windows(error.to_string()))?;

    let processes = weixin_processes();
    if processes.is_empty() {
        return Err(KeyScanError::NotFound(
            "未找到运行中的 Weixin.exe/WeChat.exe".into(),
        ));
    }

    let dll_started = Instant::now();
    let dll_path = find_weixin_dll(wechat_dir, &processes);
    let dll_keys = dll_path.as_deref().map(scan_dll_keys).unwrap_or_default();
    debug_log(|| {
        format!(
            "dll={:?} candidates={} took={:?}",
            dll_path,
            dll_keys.len(),
            dll_started.elapsed()
        )
    });

    let deadline = Instant::now() + MAX_SCAN_TIME;
    let ordered = ordered_pids(&processes);
    let main_pid = ordered.first().copied();
    let mut candidate_count = 0usize;

    for pid in ordered {
        if Instant::now() >= deadline {
            break;
        }
        let pid_started = Instant::now();
        debug_log(|| format!("scanning pid {pid}"));
        let Ok((images, privates)) = scan_process_memory(pid, deadline) else {
            continue;
        };
        debug_log(|| {
            format!(
                "pid {pid} done: images={} privates={} took={:?}",
                images.len(),
                privates.len(),
                pid_started.elapsed()
            )
        });
        let mut keys = images;
        keys.extend(privates);
        keys.sort();
        keys.dedup();
        if keys.is_empty() {
            continue;
        }
        candidate_count += keys.len();
        debug_expect(&keys, &dll_keys);
        if let Some(found) = try_validate_memory(&keys, &dll_keys, &page1) {
            return Ok(found);
        }
        if Some(pid) == main_pid {
            break;
        }
    }

    for internal in &dll_keys {
        if let Some((key_hex, raw_key)) = validate(internal, None, &page1) {
            return Ok(RecoveredKey {
                key_hex,
                raw_key,
                source: "dll".into(),
                internal_key_hex: Some(hex::encode(internal)),
            });
        }
    }

    Err(KeyScanError::NotFound(format!(
        "扫描到 {candidate_count} 个内存候选、{} 个 DLL 候选，但没有通过数据库校验",
        dll_keys.len()
    )))
}

#[derive(Clone, Copy)]
enum ValidateTask {
    Exact(usize),
    Xor(usize, usize),
}

fn try_validate_memory(
    keys: &[[u8; 32]],
    dll_keys: &[[u8; 32]],
    page1: &[u8],
) -> Option<RecoveredKey> {
    let mut tasks = Vec::new();
    if dll_keys.is_empty() {
        tasks.extend((0..keys.len()).map(ValidateTask::Exact));
    } else {
        for key_index in 0..keys.len() {
            for dll_index in 0..dll_keys.len() {
                tasks.push(ValidateTask::Xor(key_index, dll_index));
            }
        }
        tasks.extend((0..keys.len()).map(ValidateTask::Exact));
    }
    if tasks.is_empty() {
        return None;
    }

    let next = AtomicUsize::new(0);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let result = Mutex::new(None);
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
        .clamp(1, 16);
    let tasks = &tasks;

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let stop = &stop;
            let result = &result;
            scope.spawn(move || loop {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                let index = next.fetch_add(1, Ordering::Relaxed);
                if index >= tasks.len() {
                    break;
                }
                let found =
                    match tasks[index] {
                        ValidateTask::Exact(key_index) => validate(&keys[key_index], None, page1)
                            .map(|(key_hex, raw_key)| RecoveredKey {
                                key_hex,
                                raw_key,
                                source: "memory".into(),
                                internal_key_hex: None,
                            }),
                        ValidateTask::Xor(key_index, dll_index) => {
                            validate(&keys[key_index], Some(&dll_keys[dll_index]), page1).map(
                                |(key_hex, raw_key)| RecoveredKey {
                                    key_hex,
                                    raw_key,
                                    source: "memory^dll".into(),
                                    internal_key_hex: Some(hex::encode(dll_keys[dll_index])),
                                },
                            )
                        }
                    };
                if let Some(found) = found {
                    let mut slot = result.lock().unwrap();
                    if slot.is_none() {
                        *slot = Some(found);
                    }
                    stop.store(true, Ordering::Relaxed);
                    break;
                }
            });
        }
    });

    result.into_inner().unwrap()
}

fn debug_expect(memory: &[[u8; 32]], dll: &[[u8; 32]]) {
    if std::env::var_os("WCE_KEY_SCAN_DEBUG").is_none() {
        return;
    }
    let Ok(expected) = std::env::var("WCE_KEY_SCAN_EXPECT_HEX") else {
        return;
    };
    let Ok(bytes) = hex::decode(expected.trim()) else {
        return;
    };
    let Ok(target) = <[u8; 32]>::try_from(bytes) else {
        return;
    };
    let exact = memory.contains(&target);
    let mut xor_hits = 0;
    for internal in dll {
        let mut masked = target;
        for (byte, mask) in masked.iter_mut().zip(internal.iter()) {
            *byte ^= mask;
        }
        if memory.contains(&masked) {
            xor_hits += 1;
        }
    }
    eprintln!(
        "[key-scan] expect exact={exact} xor_hits={xor_hits} memory={} dll={}",
        memory.len(),
        dll.len()
    );
}

fn debug_log(message: impl FnOnce() -> String) {
    if std::env::var_os("WCE_KEY_SCAN_DEBUG").is_some() {
        eprintln!("[key-scan] {}", message());
    }
}

fn validate(key: &[u8; 32], internal: Option<&[u8; 32]>, page1: &[u8]) -> Option<(String, bool)> {
    let mut candidate = *key;
    if let Some(internal) = internal {
        for (byte, mask) in candidate.iter_mut().zip(internal.iter()) {
            *byte ^= mask;
        }
    }
    resolve_page1_key_material(&candidate, page1).map(|resolved| {
        (
            hex::encode(candidate),
            matches!(resolved.mode, KeyMode::RawEncKey),
        )
    })
}

pub fn recover_image_key_for_account(
    account_dir: &Path,
    pid: Option<u32>,
) -> Result<ImageKeyMatch> {
    let scan = scan_v2_templates(account_dir, 32, 500);
    if scan.templates.is_empty() {
        return Err(KeyScanError::NotFound(format!(
            "在 {} 下没有找到可用的 V2 图片模板（扫描了 {} 个文件）",
            account_dir.display(),
            scan.files_scanned
        )));
    }
    let pids = match pid {
        Some(pid) => vec![pid],
        None => ordered_pids(&weixin_processes()),
    };
    if pids.is_empty() {
        return Err(KeyScanError::NotFound(
            "未找到运行中的 Weixin.exe/WeChat.exe".into(),
        ));
    }
    let deadline = Instant::now() + MAX_SCAN_TIME;
    for pid in pids {
        if Instant::now() >= deadline {
            break;
        }
        match scan_process_image_key(pid, &scan.templates, deadline) {
            Ok(Some(found)) => return Ok(found),
            _ => continue,
        }
    }
    Err(KeyScanError::NotFound(format!(
        "扫描了 {} 个 V2 模板，但没有在微信进程内存中找到匹配的图片密钥",
        scan.templates.len()
    )))
}

fn scan_process_image_key(
    pid: u32,
    templates: &[V2Template],
    deadline: Instant,
) -> Result<Option<ImageKeyMatch>> {
    let handle =
        unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(KeyScanError::Windows(format!("OpenProcess({pid}) 失败")));
    }
    let regions = collect_writable_regions(handle);
    let mut seen: HashSet<String> = HashSet::new();
    let mut buffer = vec![0u8; IMAGE_CHUNK_SIZE + IMAGE_CHUNK_OVERLAP];
    let mut result = None;
    'regions: for (base, size) in regions {
        if Instant::now() >= deadline {
            break;
        }
        let mut offset = 0usize;
        let mut trailing: Vec<u8> = Vec::new();
        while offset < size {
            if Instant::now() >= deadline {
                break 'regions;
            }
            let want = IMAGE_CHUNK_SIZE.min(size - offset);
            let read_size = (want + IMAGE_CHUNK_OVERLAP).min(size - offset);
            let mut read = 0usize;
            let ok = unsafe {
                ReadProcessMemory(
                    handle,
                    (base + offset) as *const _,
                    buffer.as_mut_ptr() as *mut _,
                    read_size,
                    &mut read,
                )
            };
            if ok == 0 || read == 0 {
                trailing.clear();
                offset += want;
                continue;
            }
            let mut data = Vec::with_capacity(trailing.len() + read);
            data.extend_from_slice(&trailing);
            data.extend_from_slice(&buffer[..read]);
            for (aes_key, encoding) in extract_image_candidates(&data) {
                if !seen.insert(aes_key.clone()) {
                    continue;
                }
                if let Some(xor_key) = trusted_xor_for_verified_aes_key(&aes_key, templates) {
                    result = Some(ImageKeyMatch {
                        aes_key,
                        xor_key,
                        encoding: encoding.to_string(),
                        pid,
                    });
                    break 'regions;
                }
            }
            trailing = if read == read_size {
                data[data.len() - IMAGE_CHUNK_OVERLAP..].to_vec()
            } else {
                Vec::new()
            };
            offset += want;
        }
    }
    unsafe { CloseHandle(handle) };
    Ok(result)
}

fn collect_writable_regions(handle: HANDLE) -> Vec<Region> {
    let mut regions = Vec::new();
    let mut address: usize = 0;
    loop {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let written = unsafe {
            VirtualQueryEx(
                handle,
                address as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let size = info.RegionSize;
        if info.State == MEM_COMMIT
            && (info.Protect & PAGE_GUARD) == 0
            && (info.Protect & PAGE_NOACCESS) == 0
            && (info.Protect & WRITABLE) != 0
            && size <= MAX_IMAGE_REGION_SIZE
        {
            regions.push((base, size));
        }
        let next = base.saturating_add(size);
        if next <= address {
            break;
        }
        address = next;
    }
    regions
}

fn extract_image_candidates(data: &[u8]) -> Vec<(String, &'static str)> {
    let mut output = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();

    let mut index = 0usize;
    while index < data.len() {
        if data[index].is_ascii_alphanumeric() {
            let start = index;
            while index < data.len() && data[index].is_ascii_alphanumeric() {
                index += 1;
            }
            if index - start == 32 {
                if let Ok(text) = std::str::from_utf8(&data[start..start + 16]) {
                    let aes_key = text.to_string();
                    if seen.insert(aes_key.clone()) {
                        output.push((aes_key, "ascii"));
                    }
                }
            }
        } else {
            index += 1;
        }
    }

    let mut index = 0usize;
    while index + 1 < data.len() {
        if data[index].is_ascii_alphanumeric() && data[index + 1] == 0 {
            let start = index;
            let mut count = 0usize;
            while index + 1 < data.len()
                && data[index].is_ascii_alphanumeric()
                && data[index + 1] == 0
            {
                count += 1;
                index += 2;
            }
            if count == 32 {
                let characters: Vec<u8> = (0..32).map(|step| data[start + step * 2]).collect();
                if let Ok(text) = std::str::from_utf8(&characters[..16]) {
                    let aes_key = text.to_string();
                    if seen.insert(aes_key.clone()) {
                        output.push((aes_key, "utf-16le"));
                    }
                }
            }
        } else {
            index += 1;
        }
    }

    output
}

fn probe_database(db_storage: &Path) -> Option<PathBuf> {
    let nested = db_storage.join("session").join("session.db");
    if nested.is_file() {
        return Some(nested);
    }
    let flat = db_storage.join("session.db");
    flat.is_file().then_some(flat)
}

fn is_weixin_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Weixin.exe") || name.eq_ignore_ascii_case("WeChat.exe")
}

fn weixin_processes() -> Vec<(u32, PathBuf)> {
    let mut output = Vec::new();
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot.is_null() || snapshot == INVALID_HANDLE_VALUE {
        return output;
    }
    let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
    let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while ok {
        let end = entry
            .szExeFile
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(entry.szExeFile.len());
        let name = OsString::from_wide(&entry.szExeFile[..end])
            .to_string_lossy()
            .to_string();
        if is_weixin_name(&name) {
            let path = process_path(entry.th32ProcessID).unwrap_or_default();
            output.push((entry.th32ProcessID, path));
        }
        ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe { CloseHandle(snapshot) };
    output
}

fn process_path(pid: u32) -> Option<PathBuf> {
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return None;
    }
    let mut buffer = vec![0u16; 32768];
    let mut size = buffer.len() as u32;
    let ok = unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut size) };
    unsafe { CloseHandle(handle) };
    if ok == 0 {
        return None;
    }
    Some(PathBuf::from(OsString::from_wide(&buffer[..size as usize])))
}

fn ordered_pids(processes: &[(u32, PathBuf)]) -> Vec<u32> {
    unsafe extern "system" fn callback(
        hwnd: windows_sys::Win32::Foundation::HWND,
        lparam: isize,
    ) -> i32 {
        if IsWindowVisible(hwnd) == 0 {
            return 1;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, &mut pid);
        if pid != 0 {
            let pids = &mut *(lparam as *mut Vec<u32>);
            pids.push(pid);
        }
        1
    }
    let mut pids: Vec<u32> = Vec::new();
    unsafe {
        EnumWindows(Some(callback), &mut pids as *mut _ as isize);
    }
    let mut ordered = Vec::new();
    for pid in pids {
        if processes.iter().any(|(candidate, _)| *candidate == pid) && !ordered.contains(&pid) {
            ordered.push(pid);
        }
    }
    for (pid, _) in processes {
        if !ordered.contains(pid) {
            ordered.push(*pid);
        }
    }
    ordered
}

fn scan_process_memory(pid: u32, deadline: Instant) -> Result<KeyBatch> {
    let handle =
        unsafe { OpenProcess(PROCESS_VM_READ | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        return Err(KeyScanError::Windows(format!("OpenProcess({pid}) 失败")));
    }
    let (images, privates) = collect_regions(handle);
    debug_log(|| {
        format!(
            "pid {pid} regions: images={} ({:.1}MB) privates={} ({:.1}MB)",
            images.len(),
            images.iter().map(|(_, size)| *size).sum::<usize>() as f64 / 1048576.0,
            privates.len(),
            privates.iter().map(|(_, size)| *size).sum::<usize>() as f64 / 1048576.0,
        )
    });
    let shared = SendHandle(handle);
    let image_keys = scan_regions(shared, &images, deadline);
    let private_keys = scan_regions(shared, &privates, deadline);
    unsafe { CloseHandle(handle) };
    Ok((
        image_keys.into_iter().collect(),
        private_keys.into_iter().collect(),
    ))
}

fn collect_regions(handle: HANDLE) -> RegionSet {
    let mut images = Vec::new();
    let mut privates = Vec::new();
    let mut address: usize = 0;
    loop {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let written = unsafe {
            VirtualQueryEx(
                handle,
                address as *const _,
                &mut info,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        if written == 0 {
            break;
        }
        let base = info.BaseAddress as usize;
        let size = info.RegionSize;
        if info.State == MEM_COMMIT
            && (info.Protect & PAGE_GUARD) == 0
            && (info.Protect & PAGE_NOACCESS) == 0
            && (info.Protect & READABLE) != 0
        {
            if info.Type == MEM_IMAGE {
                images.push((base, size));
            } else if info.Type == MEM_PRIVATE {
                privates.push((base, size));
            }
        }
        let next = base.saturating_add(size);
        if next <= address {
            break;
        }
        address = next;
    }
    (images, privates)
}

fn scan_regions(handle: SendHandle, regions: &[Region], deadline: Instant) -> HashSet<[u8; 32]> {
    if regions.is_empty() {
        return HashSet::new();
    }
    let next = AtomicUsize::new(0);
    let found = Mutex::new(HashSet::new());
    let workers = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(4)
        .clamp(1, 8);

    std::thread::scope(|scope| {
        for _ in 0..workers {
            let next = &next;
            let found = &found;
            scope.spawn(move || {
                let mut local = HashSet::new();
                let mut buffer = vec![0u8; CHUNK_SIZE + 64];
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= regions.len() || Instant::now() >= deadline {
                        break;
                    }
                    let (base, size) = regions[index];
                    scan_region_shared(handle, base, size, &mut buffer, &mut local, deadline);
                }
                found.lock().unwrap().extend(local);
            });
        }
    });

    found.into_inner().unwrap()
}

fn scan_region_shared(
    handle: SendHandle,
    base: usize,
    size: usize,
    buffer: &mut [u8],
    keys: &mut HashSet<[u8; 32]>,
    deadline: Instant,
) {
    scan_region(handle.0, base, size, buffer, keys, deadline);
}

fn scan_region(
    handle: HANDLE,
    base: usize,
    size: usize,
    buffer: &mut [u8],
    keys: &mut HashSet<[u8; 32]>,
    deadline: Instant,
) {
    let mut offset = 0usize;
    while offset < size {
        if Instant::now() >= deadline {
            return;
        }
        let want = CHUNK_SIZE.min(size - offset);
        let read_size = (want + 64).min(size - offset);
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                handle,
                (base + offset) as *const _,
                buffer.as_mut_ptr() as *mut _,
                read_size.min(buffer.len()),
                &mut read,
            )
        };
        if ok != 0 && read >= 32 {
            find_stub_pointers(handle, &buffer[..read], keys);
        }
        offset += want;
    }
}

fn find_stub_pointers(handle: HANDLE, buffer: &[u8], keys: &mut HashSet<[u8; 32]>) {
    for position in memmem::find_iter(buffer, &STUB_NEEDLE) {
        if position < 6 {
            continue;
        }
        let start = position - 6;
        let pointer = u64::from_le_bytes(buffer[start..start + 8].try_into().unwrap()) as usize;
        if pointer < 0x10000 {
            continue;
        }
        let mut key = [0u8; 32];
        let mut read = 0usize;
        let ok = unsafe {
            ReadProcessMemory(
                handle,
                pointer as *const _,
                key.as_mut_ptr() as *mut _,
                32,
                &mut read,
            )
        };
        if ok != 0 && read == 32 && is_potential_key(&key) {
            keys.insert(key);
        }
    }
}

fn is_potential_key(key: &[u8; 32]) -> bool {
    let distinct: HashSet<u8> = key.iter().copied().collect();
    if distinct.len() < 15 {
        return false;
    }
    let printable = key.iter().filter(|byte| (32..=126).contains(*byte)).count();
    printable <= 24
}

fn find_weixin_dll(wechat_dir: Option<&Path>, processes: &[(u32, PathBuf)]) -> Option<PathBuf> {
    if let Some(dir) = wechat_dir {
        if let Some(found) = search_dll(dir, 0) {
            return Some(found);
        }
    }
    for (_, exe) in processes {
        if let Some(parent) = exe.parent() {
            if let Some(found) = search_dll(parent, 0) {
                return Some(found);
            }
        }
    }
    None
}

fn search_dll(dir: &Path, depth: usize) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let entries: Vec<_> = entries.flatten().collect();
    for entry in &entries {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_ascii_lowercase();
        if name == "weixin.dll" || name == "wechat.dll" {
            return Some(path);
        }
    }
    if depth >= 3 {
        return None;
    }
    for entry in entries {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = search_dll(&path, depth + 1) {
                return Some(found);
            }
        }
    }
    None
}

fn scan_dll_keys(path: &Path) -> Vec<[u8; 32]> {
    let Ok(data) = std::fs::read(path) else {
        return Vec::new();
    };
    let mut keys: HashSet<[u8; 32]> = HashSet::new();
    for section in code_sections(&data) {
        scan_dll_section(section, &mut keys);
    }
    keys.into_iter().collect()
}

fn code_sections(data: &[u8]) -> Vec<&[u8]> {
    let mut sections = Vec::new();
    if data.len() < 0x40 || &data[0..2] != b"MZ" {
        return sections;
    }
    let pe_offset = u32::from_le_bytes(data[0x3c..0x40].try_into().unwrap()) as usize;
    if pe_offset + 24 > data.len() || &data[pe_offset..pe_offset + 4] != b"PE\0\0" {
        return sections;
    }
    let coff = pe_offset + 4;
    let count = u16::from_le_bytes(data[coff + 2..coff + 4].try_into().unwrap()) as usize;
    let optional_size = u16::from_le_bytes(data[coff + 16..coff + 18].try_into().unwrap()) as usize;
    let table = coff + 20 + optional_size;
    for index in 0..count {
        let offset = table + index * 40;
        if offset + 40 > data.len() {
            break;
        }
        let characteristics =
            u32::from_le_bytes(data[offset + 36..offset + 40].try_into().unwrap());
        if characteristics & 0x2000_0000 == 0 {
            continue;
        }
        let size = u32::from_le_bytes(data[offset + 16..offset + 20].try_into().unwrap()) as usize;
        let raw = u32::from_le_bytes(data[offset + 20..offset + 24].try_into().unwrap()) as usize;
        if size > 0 && raw + size <= data.len() {
            sections.push(&data[raw..raw + size]);
        }
    }
    sections
}

fn scan_dll_section(data: &[u8], keys: &mut HashSet<[u8; 32]>) {
    let mut key = [0u8; 32];
    for first in 0..data.len().saturating_sub(2) {
        if data[first] != 0x48 || data[first + 1] != 0xBA || first + 10 > data.len() {
            continue;
        }
        key[0..8].copy_from_slice(&data[first + 2..first + 10]);
        for gap1 in 3..=8usize {
            let second = first + 10 + gap1;
            if second + 10 > data.len() || data[second] != 0x48 || data[second + 1] != 0xBA {
                continue;
            }
            key[8..16].copy_from_slice(&data[second + 2..second + 10]);
            for gap2 in 3..=8usize {
                let third = second + 10 + gap2;
                if third + 10 > data.len() || data[third] != 0x48 || data[third + 1] != 0xBA {
                    continue;
                }
                key[16..24].copy_from_slice(&data[third + 2..third + 10]);
                for gap3 in 3..=8usize {
                    let fourth = third + 10 + gap3;
                    if fourth + 10 > data.len() || data[fourth] != 0x48 || data[fourth + 1] != 0xBA
                    {
                        continue;
                    }
                    key[24..32].copy_from_slice(&data[fourth + 2..fourth + 10]);
                    for gap4 in 3..=8usize {
                        let tail = fourth + 10 + gap4;
                        if tail + 3 <= data.len()
                            && data[tail] == 0x48
                            && data[tail + 1] == 0x85
                            && data[tail + 2] == 0xC0
                        {
                            keys.insert(key);
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod image_key_tests {
    use super::extract_image_candidates;

    #[test]
    fn extracts_ascii_candidate() {
        let data = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let candidates = extract_image_candidates(data);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "aaaaaaaaaaaaaaaa");
        assert_eq!(candidates[0].1, "ascii");
    }

    #[test]
    fn rejects_wrong_run_length() {
        assert!(extract_image_candidates(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_empty());
        assert!(extract_image_candidates(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_empty());
    }

    #[test]
    fn extracts_utf16_candidate() {
        let mut data = Vec::new();
        for _ in 0..32 {
            data.push(b'b');
            data.push(0);
        }
        let candidates = extract_image_candidates(&data);
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].0, "bbbbbbbbbbbbbbbb");
        assert_eq!(candidates[0].1, "utf-16le");
    }
}
