use serde::{Deserialize, Serialize};
use shairplay::PairingStore;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PersistedState {
    pub mac: Option<[u8; 6]>,
    #[serde(default)]
    pub identity_seed: Option<[u8; 32]>,
    #[serde(default)]
    pub paired_keys: HashMap<String, [u8; 32]>,
}

impl PersistedState {
    pub fn load(path: &Path) -> Self {
        fs::read_to_string(path)
            .ok()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, path: &Path) {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = fs::write(path, json);
        }
    }
}

pub struct FilePairingStore {
    path: PathBuf,
    state: Mutex<PersistedState>,
}

impl FilePairingStore {
    pub fn new(path: PathBuf, state: PersistedState) -> Self {
        Self {
            path,
            state: Mutex::new(state),
        }
    }
}

impl PairingStore for FilePairingStore {
    fn get(&self, device_id: &str) -> Option<[u8; 32]> {
        self.state.lock().ok()?.paired_keys.get(device_id).copied()
    }

    fn put(&self, device_id: &str, public_key: [u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            state.paired_keys.insert(device_id.to_owned(), public_key);
            state.save(&self.path);
        }
    }

    fn remove(&self, device_id: &str) {
        if let Ok(mut state) = self.state.lock() {
            state.paired_keys.remove(device_id);
            state.save(&self.path);
        }
    }

    fn has_any_pairing(&self) -> bool {
        self.state
            .lock()
            .map(|state| !state.paired_keys.is_empty())
            .unwrap_or(false)
    }

    fn load_identity(&self) -> Option<[u8; 32]> {
        self.state.lock().ok()?.identity_seed
    }

    fn save_identity(&self, seed: [u8; 32]) {
        if let Ok(mut state) = self.state.lock() {
            state.identity_seed = Some(seed);
            state.save(&self.path);
        }
    }
}
