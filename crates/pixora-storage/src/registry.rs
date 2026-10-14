//! 驱动注册表。
//!
//! 核心系统通过实例键取驱动：默认实例用驱动名（`local`），
//! 同类型挂了多个后端时用 `驱动名#后端id`（`github#2`）。
//! 键由服务层从 `blobs.storage_driver` + `blobs.backend_id` 拼出来，
//! 所以「这张图当初存在哪个后端」在数据层是可追溯的。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::driver::StorageDriver;

#[derive(Default)]
pub struct StorageRegistry {
    drivers: RwLock<HashMap<String, Arc<dyn StorageDriver>>>,
    default_key: RwLock<Option<String>>,
}

impl StorageRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 注册一个驱动实例。`key` 建议用 `驱动名` 或 `驱动名#后端id`。
    pub fn register(&self, key: impl Into<String>, driver: Arc<dyn StorageDriver>) {
        let key = key.into();
        let mut drivers = self.drivers.write().expect("驱动表不该中毒");
        drivers.insert(key.clone(), driver);
        let mut default = self.default_key.write().expect("默认键不该中毒");
        if default.is_none() {
            *default = Some(key);
        }
    }

    /// 切换默认后端。切换后新的上传走新后端，已有文件仍按各自记录的驱动读取。
    pub fn set_default(&self, key: &str) {
        *self.default_key.write().expect("默认键不该中毒") = Some(key.to_string());
    }

    pub fn default_key(&self) -> Option<String> {
        self.default_key.read().expect("默认键不该中毒").clone()
    }

    pub fn get(&self, key: &str) -> Option<Arc<dyn StorageDriver>> {
        self.drivers
            .read()
            .expect("驱动表不该中毒")
            .get(key)
            .cloned()
    }

    /// 按驱动名取。同类型有多个实例时取 id 最小的那个（键后缀数字最小）。
    pub fn get_by_driver(&self, driver_name: &str) -> Option<Arc<dyn StorageDriver>> {
        if let Some(exact) = self.get(driver_name) {
            return Some(exact);
        }
        let drivers = self.drivers.read().expect("驱动表不该中毒");
        let mut candidates: Vec<(&String, &Arc<dyn StorageDriver>)> = drivers
            .iter()
            .filter(|(key, _)| key.starts_with(&format!("{driver_name}#")))
            .collect();
        candidates.sort_by_key(|(key, _)| key.as_str());
        candidates.first().map(|(_, driver)| Arc::clone(driver))
    }

    /// 默认驱动。注入阶段没配默认后端时按本地磁盘兜底——
    /// 图床没有后端可用比内存多用几兆严重得多。
    pub fn default_driver(&self) -> Option<Arc<dyn StorageDriver>> {
        let key = self.default_key()?;
        self.get(&key)
    }

    /// 解析「某张图当初存在哪个后端」。找不到精确实例时退到同类型驱动。
    pub fn resolve(&self, driver_name: &str, backend_id: Option<i64>) -> Option<Arc<dyn StorageDriver>> {
        match backend_id {
            Some(id) => self
                .get(&instance_key(driver_name, Some(id)))
                .or_else(|| self.get_by_driver(driver_name)),
            None => self.get(driver_name).or_else(|| self.get_by_driver(driver_name)),
        }
    }

    pub fn keys(&self) -> Vec<String> {
        let mut keys: Vec<String> = self
            .drivers
            .read()
            .expect("驱动表不该中毒")
            .keys()
            .cloned()
            .collect();
        keys.sort();
        keys
    }

    pub fn clear(&self) {
        self.drivers.write().expect("驱动表不该中毒").clear();
        *self.default_key.write().expect("默认键不该中毒") = None;
    }
}

/// 拼实例键。`backend_id` 为空即默认实例。
pub fn instance_key(driver_name: &str, backend_id: Option<i64>) -> String {
    match backend_id {
        Some(id) => format!("{driver_name}#{id}"),
        None => driver_name.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_keys() {
        assert_eq!(instance_key("local", None), "local");
        assert_eq!(instance_key("github", Some(2)), "github#2");
    }
}
