//! Per-dojo serialization for `bare.git` and other dojo-local filesystem mutations.
//!
//! [`FsDojoUploadStore::io_lock`] is per handler instance and does not coordinate across
//! concurrent HTTP requests. Git smart HTTP and mirror normalize must share one lock.

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::{Mutex, OwnedMutexGuard, RwLock};

pub struct DojoIoLocks {
    locks: RwLock<HashMap<String, Arc<Mutex<()>>>>,
}

impl DojoIoLocks {
    pub fn new() -> Self {
        DojoIoLocks {
            locks: RwLock::new(HashMap::new()),
        }
    }

    pub async fn lock(&self, dojo_id: &str) -> OwnedMutexGuard<()> {
        assert!(!dojo_id.is_empty(), "dojo_id must not be empty");
        let arc = {
            let mut map = self.locks.write().await;
            map.entry(dojo_id.to_string())
                .or_insert_with(|| Arc::new(Mutex::new(())))
                .clone()
        };
        arc.lock_owned().await
    }
}
