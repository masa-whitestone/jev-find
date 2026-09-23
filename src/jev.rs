//! TypeSafe System One API (POST {base}/v1/systemone) の薄いクライアント。
//! 仕様: https://docs.typesafe.ai/api
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Serialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Question {
    Noul {
        instructions: Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        criteria: Option<Value>,
    },
    Choice {
        instructions: Value,
        criteria: BTreeMap<String, Value>,
    },
    Score {
        instructions: Value,
        criteria: Vec<Value>,
    },
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Answer {
    Noul {
        noul: f64,
    },
    Choice {
        choice: String,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
    Score {
        score: f64,
        #[serde(default)]
        legend: BTreeMap<String, String>,
        #[serde(default)]
        probabilities: BTreeMap<String, f64>,
        confidence: f64,
    },
}

#[derive(Serialize)]
struct Request<'a> {
    model: &'a str,
    state: &'a Value,
    questions: &'a BTreeMap<String, Question>,
}

#[derive(Deserialize, Default, Clone, Copy)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
}

#[derive(Deserialize)]
struct Response {
    answers: BTreeMap<String, Answer>,
    #[serde(default)]
    usage: Usage,
}

pub struct Client {
    http: reqwest::Client,
    url: String,
    key: String,
    pub model: String,
}

impl Client {
    pub fn new(base: &str, key: String, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .pool_max_idle_per_host(256)
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self {
            http,
            url: format!("{}/v1/systemone", base.trim_end_matches('/')),
            key,
            model,
        })
    }

    /// 429 / 529 / 5xx / 接続エラーは指数バックオフで再試行する(ドキュメントの推奨どおり)
    pub async fn evaluate(
        &self,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<(BTreeMap<String, Answer>, Usage)> {
        let body = Request { model: &self.model, state, questions };
        let mut delay = Duration::from_millis(250);
        const MAX: u32 = 6;
        for attempt in 1..=MAX {
            match self.http.post(&self.url).bearer_auth(&self.key).json(&body).send().await {
                Ok(r) if r.status().is_success() => {
                    let v: Response = r.json().await?;
                    return Ok((v.answers, v.usage));
                }
                Ok(r) => {
                    let st = r.status().as_u16();
                    let retryable = st == 429 || st == 529 || st >= 500;
                    if !retryable || attempt == MAX {
                        let t = r.text().await.unwrap_or_default();
                        return Err(anyhow!("HTTP {st}: {}", summarize(&t)));
                    }
                }
                Err(e) if attempt < MAX && (e.is_timeout() || e.is_connect() || e.is_request()) => {}
                Err(e) => return Err(e.into()),
            }
            let jitter = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos() % 150);
            tokio::time::sleep(delay + Duration::from_millis(jitter as u64)).await;
            delay = (delay * 2).min(Duration::from_secs(8));
        }
        unreachable!()
    }
}

/// エラー本文を1行に縮める。HTML(Cloudflare のブロック画面など)は <title> だけにする
fn summarize(body: &str) -> String {
    let t = body.trim();
    if t.starts_with('<') {
        let title = t
            .split_once("<title>")
            .and_then(|(_, r)| r.split_once("</title>"))
            .map_or("HTML response", |(x, _)| x.trim());
        return format!("{title} (blocked by the API's firewall)");
    }
    let one: String = t.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 300 {
        one.chars().take(300).collect::<String>() + "…"
    } else {
        one
    }
}
