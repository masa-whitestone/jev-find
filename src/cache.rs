//! (model, state, questions) のハッシュをキーにした回答キャッシュ。
//! state に mtime とサイズを含めるので、ファイルが変われば自然に無効になる。
//!
//! 1回答1ファイルだと、数千ファイルの探索で数千回の open/write/rename が走り、
//! キャッシュが効いた再実行でも数千回の open/read が走る。そこで追記専用のログ1本
//! (`key\tanswers_json\n` の行)にまとめ、起動時に裏で一度だけ読み込んでメモリ上の表から引く。
use crate::jev::{Answer, Question};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;
use tokio::task::JoinHandle;

type Map = HashMap<String, String>;

pub struct Cache {
    inner: Option<Arc<Inner>>,
}

struct Inner {
    path: PathBuf,
    loader: Mutex<Option<JoinHandle<Loaded>>>,
    map: OnceCell<Mutex<Map>>,
    /// 追記用に開いたログ(最初の書き込みで開く)
    log: Mutex<Option<std::fs::File>>,
    /// 前回の書き込みが途中で切れていた(末尾が改行でない)
    needs_newline: Mutex<bool>,
}

struct Loaded {
    map: Map,
    needs_newline: bool,
}

/// 重複行がこの倍率を超えたら読み込み時に詰め直す
const COMPACT_RATIO: usize = 2;
const COMPACT_MIN_BYTES: usize = 1 << 20;

impl Cache {
    /// キャッシュの読み込みはここで裏に回す。探索と並行して進み、最初の get で待ち合わせる
    pub fn new(enabled: bool) -> Self {
        if !enabled {
            return Self { inner: None };
        }
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
        let Some(root) = base.map(|b| b.join("jfd")) else { return Self { inner: None } };
        let dir = root.join("v2");
        if std::fs::create_dir_all(&dir).is_err() {
            return Self { inner: None };
        }
        let path = dir.join("answers.log");
        let p = path.clone();
        let loader = tokio::task::spawn_blocking(move || {
            // 旧形式 (1回答1ファイル) は使わないので片付ける
            let old = root.join("v1");
            if old.is_dir() {
                let _ = std::fs::remove_dir_all(old);
            }
            load(&p)
        });
        Self {
            inner: Some(Arc::new(Inner {
                path,
                loader: Mutex::new(Some(loader)),
                map: OnceCell::new(),
                log: Mutex::new(None),
                needs_newline: Mutex::new(false),
            })),
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

    async fn map(inner: &Inner) -> &Mutex<Map> {
        inner
            .map
            .get_or_init(|| async {
                let h = inner.loader.lock().unwrap().take();
                let l = match h {
                    Some(h) => h.await.unwrap_or(Loaded { map: Map::new(), needs_newline: false }),
                    None => Loaded { map: Map::new(), needs_newline: false },
                };
                *inner.needs_newline.lock().unwrap() = l.needs_newline;
                Mutex::new(l.map)
            })
            .await
    }

    pub async fn get(&self, key: &str) -> Option<BTreeMap<String, Answer>> {
        let inner = self.inner.as_ref()?;
        let raw = Self::map(inner).await.lock().unwrap().get(key)?.clone();
        serde_json::from_str(&raw).ok()
    }

    /// 1回の API 応答ぶんをまとめて1回の追記で書く
    pub async fn put_many<'a>(&self, entries: impl IntoIterator<Item = (&'a str, &'a BTreeMap<String, Answer>)>) {
        let Some(inner) = self.inner.as_ref() else { return };
        let map = Self::map(inner).await;
        let mut buf = Vec::new();
        {
            let mut m = map.lock().unwrap();
            for (k, a) in entries {
                let Ok(v) = serde_json::to_string(a) else { continue };
                buf.extend_from_slice(k.as_bytes());
                buf.push(b'\t');
                buf.extend_from_slice(v.as_bytes());
                buf.push(b'\n');
                m.insert(k.to_string(), v);
            }
        }
        if buf.is_empty() {
            return;
        }
        let mut log = inner.log.lock().unwrap();
        if log.is_none() {
            *log = std::fs::OpenOptions::new().create(true).append(true).open(&inner.path).ok();
        }
        let Some(f) = log.as_mut() else { return };
        let mut nl = inner.needs_newline.lock().unwrap();
        if *nl {
            buf.insert(0, b'\n');
            *nl = false;
        }
        // O_APPEND の1回の write なので、同時に動く別の jfd と行が混ざらない
        let _ = f.write_all(&buf);
    }
}

fn load(path: &std::path::Path) -> Loaded {
    let Ok(bytes) = std::fs::read(path) else {
        return Loaded { map: Map::new(), needs_newline: false };
    };
    let needs_newline = bytes.last().is_some_and(|&b| b != b'\n');
    let text = String::from_utf8_lossy(&bytes);
    let mut map = Map::new();
    let mut lines = 0usize;
    for line in text.lines() {
        lines += 1;
        let Some((k, v)) = line.split_once('\t') else { continue };
        if k.len() != 64 || !k.bytes().all(|b| b.is_ascii_hexdigit()) || !v.starts_with('{') {
            continue;
        }
        map.insert(k.to_string(), v.to_string());
    }
    if bytes.len() >= COMPACT_MIN_BYTES && lines > map.len() * COMPACT_RATIO {
        compact(path, &map);
        return Loaded { map, needs_newline: false };
    }
    Loaded { map, needs_newline }
}

/// 重複・壊れた行を除いて書き直す(一時ファイル → rename)
fn compact(path: &std::path::Path, map: &Map) {
    let tmp = path.with_extension(format!("tmp{}", std::process::id()));
    let mut out = Vec::with_capacity(map.len() * 160);
    for (k, v) in map {
        out.extend_from_slice(k.as_bytes());
        out.push(b'\t');
        out.extend_from_slice(v.as_bytes());
        out.push(b'\n');
    }
    if std::fs::write(&tmp, out).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn roundtrip_and_recover_from_torn_line() {
        let dir = std::env::temp_dir().join(format!("jfd-cache-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("XDG_CACHE_HOME", &dir);
        let k1 = "a".repeat(64);
        let k2 = "b".repeat(64);
        let mut a = BTreeMap::new();
        a.insert("match".to_string(), Answer::Noul { noul: 0.5 });
        {
            let c = Cache::new(true);
            assert!(c.get(&k1).await.is_none());
            c.put_many([(k1.as_str(), &a)]).await;
            assert!(c.get(&k1).await.is_some());
        }
        // 書き込みが途中で切れた行を足す
        let log = dir.join("jfd/v2/answers.log");
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        f.write_all(b"cccc\t{\"x\":").unwrap();
        drop(f);
        {
            let c = Cache::new(true);
            assert!(c.get(&k1).await.is_some());
            c.put_many([(k2.as_str(), &a)]).await;
        }
        let c = Cache::new(true);
        assert!(c.get(&k1).await.is_some());
        assert!(c.get(&k2).await.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
