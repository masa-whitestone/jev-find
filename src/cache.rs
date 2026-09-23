//! (model, state, questions) のハッシュをキーにした回答キャッシュ。
//! state に mtime とサイズを含めるので、ファイルが変われば自然に無効になる。
use crate::jev::{Answer, Question};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct Cache {
    dir: Option<PathBuf>,
}

impl Cache {
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { dir: None };
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
        let dir = base.map(|b| b.join("jfd").join("v1"));
        match &dir {
            Some(d) if std::fs::create_dir_all(d).is_ok() => Self { dir },
            _ => Self { dir: None },
        }
    }

    pub fn key(model: &str, state: &Value, qs: &BTreeMap<String, Question>) -> String {
        let mut h = blake3::Hasher::new();
        h.update(model.as_bytes());
        h.update(&[0]);
        h.update(&serde_json::to_vec(state).unwrap_or_default());
        h.update(&[0]);
        h.update(&serde_json::to_vec(qs).unwrap_or_default());
        h.finalize().to_hex().to_string()
    }

    fn path(&self, key: &str) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(&key[..2]).join(key))
    }

    pub async fn get(&self, key: &str) -> Option<BTreeMap<String, Answer>> {
        let b = tokio::fs::read(self.path(key)?).await.ok()?;
        serde_json::from_slice(&b).ok()
    }

    pub async fn put(&self, key: &str, a: &BTreeMap<String, Answer>) {
        let Some(p) = self.path(key) else { return };
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let Ok(bytes) = serde_json::to_vec(a) else { return };
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        if tokio::fs::write(&tmp, bytes).await.is_ok() {
            let _ = tokio::fs::rename(&tmp, &p).await;
        }
    }
}
