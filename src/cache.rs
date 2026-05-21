use dashmap::DashMap;
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{event, Level};

// 缓存条目结构 - 存储任意 JSON 数据
#[derive(Clone)]
pub struct CachedResponse {
    pub body: Value,
    pub cached_at: Instant,
    pub expires_in: u64,
}

impl CachedResponse {
    pub fn is_expired(&self) -> bool {
        self.cached_at.elapsed() > Duration::from_secs(self.expires_in)
    }

    pub fn new(body: Value, expires_in: u64) -> Self {
        Self {
            body,
            cached_at: Instant::now(),
            expires_in,
        }
    }
}

// 通用缓存管理器
pub struct JsonCache {
    store: DashMap<String, CachedResponse>,
}

impl JsonCache {
    pub fn new() -> Self {
        Self {
            store: DashMap::new(),
        }
    }

    // 设置缓存
    pub fn set(&self, key: String, body: Value, expires_in: u64) {
        let key_preview = key[..8.min(key.len())].to_string();
        self.store.remove(&key);
        self.store.insert(key, CachedResponse::new(body, expires_in));
        event!(Level::DEBUG, "Cache set for key: {}...", key_preview);
    }

    // 获取缓存（如果过期则自动删除）
    pub fn get(&self, key: &str) -> Option<Value> {
        if let Some(entry) = self.store.get(key) {
            if entry.is_expired() {
                drop(entry);
                self.store.remove(key);
                event!(Level::DEBUG, "Cache expired for key: {}...", &key[..8.min(key.len())]);
                return None;
            }
            return Some(entry.body.clone());
        }
        None
    }

    // 获取并清除缓存（原子操作）
    pub fn get_and_clear(&self, key: &str) -> Option<Value> {
        if let Some(entry) = self.store.get(key) {
            if entry.is_expired() {
                drop(entry);
                self.store.remove(key);
                event!(Level::DEBUG, "Cache expired for key: {}...", &key[..8.min(key.len())]);
                return None;
            }
            let body = entry.body.clone();
            drop(entry);
            self.store.remove(key);
            event!(Level::DEBUG, "Cache get_and_clear for key: {}...", &key[..8.min(key.len())]);
            return Some(body);
        }
        None
    }

    // 清除指定 key 的缓存
    pub fn clear(&self, key: &str) -> bool {
        self.store.remove(key).is_some()
    }

    // 获取当前缓存大小
    pub fn len(&self) -> usize {
        self.store.len()
    }
}

impl Default for JsonCache {
    fn default() -> Self {
        Self::new()
    }
}

// 缓存全局实例类型
pub type SharedCache = Arc<JsonCache>;

// 创建全局缓存实例
pub fn create_shared_cache() -> SharedCache {
    Arc::new(JsonCache::new())
}
