use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use pbkdf2::pbkdf2_hmac;
use sha2::Sha512;

const KDF_ROUNDS: u32 = 256_000;

fn cache_path() -> Option<PathBuf> {
    if std::env::var("WCE_DERIVED_CACHE_DISABLE").ok().as_deref() == Some("1") {
        return None;
    }
    if let Some(value) = std::env::var_os("WCE_DERIVED_CACHE") {
        return Some(PathBuf::from(value));
    }
    #[cfg(windows)]
    {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("wechat-analysis-rs").join("derived_keys.json"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .map(|base| base.join("wechat-analysis-rs").join("derived_keys.json"))
    }
}

fn store() -> &'static Mutex<HashMap<String, String>> {
    static STORE: OnceLock<Mutex<HashMap<String, String>>> = OnceLock::new();
    STORE.get_or_init(|| {
        let mut map = HashMap::new();
        if let Some(path) = cache_path() {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(parsed) = serde_json::from_str::<HashMap<String, String>>(&text) {
                    map = parsed;
                }
            }
        }
        Mutex::new(map)
    })
}

fn lookup_key(path: &Path, salt_hex: &str) -> String {
    format!("{}|{}", path.to_string_lossy().to_lowercase(), salt_hex)
}

fn read_salt(path: &Path) -> Option<[u8; 16]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut salt = [0u8; 16];
    file.read_exact(&mut salt).ok()?;
    Some(salt)
}

pub(crate) fn cached(path: &Path) -> Option<String> {
    let salt = read_salt(path)?;
    let key = lookup_key(path, &hex::encode(salt));
    let guard = store().lock().ok()?;
    guard.get(&key).cloned()
}

pub(crate) fn ensure_derived(path: &Path, key_hex: &str) -> Option<String> {
    let salt = read_salt(path)?;
    let salt_hex = hex::encode(salt);
    let key = lookup_key(path, &salt_hex);
    if let Some(value) = store().lock().ok()?.get(&key).cloned() {
        return Some(value);
    }
    let key_bytes = hex::decode(key_hex).ok()?;
    if key_bytes.len() != 32 {
        return None;
    }
    let mut derived = [0u8; 32];
    pbkdf2_hmac::<Sha512>(&key_bytes, &salt, KDF_ROUNDS, &mut derived);
    let value = hex::encode(derived);
    if let Ok(mut guard) = store().lock() {
        guard.insert(key, value.clone());
        persist(&guard);
    }
    Some(value)
}

pub(crate) fn invalidate(path: &Path, key_hex: &str) {
    let _ = key_hex;
    let Some(salt) = read_salt(path) else {
        return;
    };
    let key = lookup_key(path, &hex::encode(salt));
    if let Ok(mut guard) = store().lock() {
        if guard.remove(&key).is_some() {
            persist(&guard);
        }
    }
}

fn persist(map: &HashMap<String, String>) {
    let Some(path) = cache_path() else {
        return;
    };
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(text) = serde_json::to_string(map) else {
        return;
    };
    let temp = path.with_extension("json.tmp");
    if std::fs::write(&temp, text).is_ok() {
        let _ = std::fs::rename(&temp, &path);
    }
}
