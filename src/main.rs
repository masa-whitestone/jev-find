//! jfd (jev-find) — Jev を探索の中核に置いたファイルファインダ
//!
//! パイプライン(問い合わせはすべてディレクトリをまたいでバッチ化し、1リクエストで多数を判定する):
//!   並列ウォーカー ──(ディレクトリ単位で Jev に枝刈りを問う)──▶ 候補ファイル
//!   候補ファイル ──▶ 段階1: メタデータのみで判定(安い・内容を読まない)
//!                     ├ 確率 ≤ meta_reject → 棄却
//!                     ├ 確率 ≥ meta_accept → 採用
//!                     └ その中間         → 段階2: 先頭数KBを読んで判定(+分類・採点を同時に)
mod cache;
mod config;
mod i18n;
mod jev;
mod update;

use anyhow::{bail, Context as _, Result};
use clap::{CommandFactory, FromArgMatches, Parser};
use globset::{Glob, GlobSet, GlobSetBuilder};
use jev::{Answer, Question};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::{mpsc, oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;

type BoxFut = Pin<Box<dyn Future<Output = ()> + Send>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squeeze_trims_and_collapses() {
        assert_eq!(squeeze("a  \n\n\n\nb\t\n", false), "a\n\nb\n");
        assert_eq!(squeeze("\n\nx", false), "x");
        // 切れた最後の行は捨てる
        assert_eq!(squeeze("line one\nline two\npart", true), "line one\nline two");
    }

    #[test]
    fn split_paths_shares_dirs() {
        let mut items = serde_json::Map::new();
        items.insert("f0".into(), json!({"path": "src/a/x.rs", "size_bytes": 1}));
        items.insert("f1".into(), json!({"path": "src/a/y.rs", "size_bytes": 2}));
        items.insert("f2".into(), json!({"path": "top.rs", "size_bytes": 3}));
        items.insert("f3".into(), json!({"path": "/etc", "size_bytes": 4}));
        let dirs = split_paths(&mut items);
        assert_eq!(dirs.len(), 3);
        assert_eq!(items["f0"]["dir"], items["f1"]["dir"]);
        let d = |k: &str| dirs[items[k]["dir"].as_str().unwrap()].as_str().unwrap().to_string();
        assert_eq!(d("f0"), "src/a");
        assert_eq!(d("f2"), ".");
        assert_eq!(d("f3"), "/");
        assert_eq!(items["f1"]["name"], "y.rs");
        assert_eq!(items["f1"]["size_bytes"], 2);
        assert!(items["f1"].get("path").is_none());
    }
}

#[derive(Parser, Debug)]
#[command(name = "jfd", version, about = "Semantic file finder powered by Jev")]
struct Args {
    /// What to look for, in natural language
    query: String,
    /// Where to start searching
    #[arg(default_value = ".")]
    paths: Vec<PathBuf>,

    /// Acceptance threshold for the final (content-based) judgment
    #[arg(short = 't', long, default_value_t = 0.7)]
    threshold: f64,
    /// Reject without reading content if the metadata-only probability is at or below this
    #[arg(long, default_value_t = 0.15)]
    meta_reject: f64,
    /// Accept without reading content if the metadata-only probability is at or above this
    #[arg(long, default_value_t = 0.97)]
    meta_accept: f64,
    /// Skip the metadata stage and always judge by content
    #[arg(long)]
    deep: bool,

    /// Let Jev prune directories that look irrelevant
    #[arg(long)]
    prune: bool,
    /// Do not descend into directories below this probability
    #[arg(long, default_value_t = 0.1)]
    prune_threshold: f64,
    /// Depth at which pruning starts (root = 0)
    #[arg(long, default_value_t = 1)]
    prune_min_depth: usize,

    /// Files judged per request in the metadata stage
    #[arg(long, default_value_t = 64)]
    batch: usize,
    /// Files judged per request in the content stage
    #[arg(long, default_value_t = 16)]
    deep_batch: usize,
    /// Also descend into directories skipped by default (~/Library, node_modules, app bundles, CACHEDIR.TAG build output)
    #[arg(long)]
    no_ignore: bool,

    /// Number of leading bytes sent to Jev in the content stage
    #[arg(short = 'H', long, default_value_t = 4096)]
    head_bytes: usize,
    /// Only consider these extensions (mechanical filter, no API cost)
    #[arg(short = 'e', long = "ext")]
    exts: Vec<String>,
    /// Glob to exclude (mechanical filter)
    #[arg(short = 'E', long = "exclude")]
    excludes: Vec<String>,
    /// Include hidden files and directories
    #[arg(long)]
    hidden: bool,
    /// Maximum directory depth
    #[arg(short = 'd', long)]
    max_depth: Option<usize>,
    /// Skip files larger than this many bytes
    #[arg(long)]
    max_size: Option<u64>,

    /// Classification label (repeatable): -c test="test code" -c config="config file"
    #[arg(short = 'c', long = "classify", value_name = "LABEL=DESC")]
    classify: Vec<String>,
    /// Scoring aspect; results are sorted by score (0 = lowest level, 1 = highest) and show the nearest level
    #[arg(long)]
    rank: Option<String>,
    /// Score levels (2-10, comma separated, lowest first)
    #[arg(long, value_delimiter = ',', default_values = ["low", "medium", "high"])]
    levels: Vec<String>,

    /// Concurrent requests to the Jev API
    #[arg(short = 'j', long, default_value_t = 64)]
    jobs: usize,
    /// Maximum number of API requests (cost safety valve); stops the search when reached
    #[arg(long)]
    budget: Option<u64>,
    /// Output JSON Lines
    #[arg(long)]
    json: bool,
    /// Also print probabilities
    #[arg(short = 'p', long)]
    show_prob: bool,
    /// Separate results with NUL (for xargs -0)
    #[arg(short = '0', long)]
    print0: bool,
    /// Print only the N most likely results (by probability, or by score with --rank)
    #[arg(short = 'n', long, value_name = "N")]
    top: Option<usize>,
    /// Sort results by path (disables streaming output)
    #[arg(long)]
    sort: bool,
    /// Do not use the answer cache
    #[arg(long)]
    no_cache: bool,
    /// Print statistics (requests, tokens, estimated cost) to stderr
    #[arg(long)]
    stats: bool,
    /// Print pruning decisions etc. to stderr
    #[arg(short = 'v', long)]
    verbose: bool,
    /// Model to use [default: config file, then jev-latest]
    #[arg(long, env = "JEV_MODEL")]
    model: Option<String>,
    /// API base URL [default: config file, then https://api.typesafe.ai]
    #[arg(long, env = "TYPESAFE_BASE_URL")]
    base_url: Option<String>,
}

const AFTER_HELP_EN: &str = "\
Examples:
  jfd \"code that refreshes OAuth tokens\" src/
  jfd \"config files that look broken\" -e yaml -e toml -p
  jfd \"test code\" -c unit=\"unit test\" -c e2e=\"end-to-end test\"
  jfd \"risky code\" --rank \"security risk\" --levels low,medium,high
  jfd \"old notes\" ~/Documents --prune --budget 500 --stats
  jfd \"meeting notes about the budget\" ~/Documents -n 10 -p

Setup:
  jfd setup              interactive setup: API key, language, model (runs automatically the first time)
  jfd config             show current settings
  jfd config set-key     change the API key
  jfd config --help      more config commands
  jfd update             update to the latest version";

const AFTER_HELP_JA: &str = "\
使用例:
  jfd \"OAuthのトークン更新を実装しているコード\" src/
  jfd \"壊れていそうな設定ファイル\" -e yaml -e toml -p
  jfd \"テストコード\" -c unit=単体テスト -c e2e=E2Eテスト
  jfd \"セキュリティ上危なそうな処理\" --rank 危険度 --levels 低,中,高
  jfd \"昔のメモ\" ~/Documents --prune --budget 500 --stats
  jfd \"予算について話した会議メモ\" ~/Documents -n 10 -p

設定:
  jfd setup              APIキー・言語・モデルを対話式に設定(初回は自動で起動)
  jfd config             現在の設定を表示
  jfd config set-key     APIキーを変更
  jfd config --help      その他の設定コマンド
  jfd update             最新版に更新する";

const HELP_JA: &[(&str, &str)] = &[
    ("query", "探したいファイルの条件(自然言語)"),
    ("paths", "探索の起点"),
    ("threshold", "最終判定(内容を読んだ段階)の採用しきい値"),
    ("meta_reject", "段階1(メタデータのみ)でこの確率以下なら、内容を読まずに棄却"),
    ("meta_accept", "段階1でこの確率以上なら、内容を読まずに採用"),
    ("deep", "段階1を飛ばし、常に内容を読んで判定する"),
    ("prune", "Jevによるディレクトリの意味的枝刈りを有効にする"),
    ("prune_threshold", "この確率未満のディレクトリには降りない"),
    ("prune_min_depth", "枝刈りを始める深さ(起点=0)"),
    ("batch", "段階1(メタデータ)で1リクエストにまとめるファイル数"),
    ("deep_batch", "段階2(内容)で1リクエストにまとめるファイル数"),
    ("no_ignore", "既定で飛ばすディレクトリ(~/Library・node_modules・アプリバンドル・CACHEDIR.TAG のあるビルド出力)にも降りる"),
    ("head_bytes", "段階2でJevに渡す先頭バイト数"),
    ("exts", "拡張子で事前に絞る(機械的フィルタ・API消費なし)"),
    ("excludes", "除外するglob(機械的フィルタ)"),
    ("hidden", "隠しファイル・ディレクトリも対象にする"),
    ("max_depth", "探索する最大の深さ"),
    ("max_size", "このバイト数を超えるファイルは対象外"),
    ("classify", "分類ラベル(繰り返し指定): -c test=テストコード -c config=設定ファイル"),
    ("rank", "採点の観点。スコア(0=最低の段階〜1=最高の段階)の降順で、最も近い段階名も表示する"),
    ("levels", "採点の段階(2〜10個、カンマ区切り、低い順)"),
    ("jobs", "Jev APIへの同時リクエスト数"),
    ("budget", "APIリクエスト数の上限(コストの安全弁)。達したら探索を打ち切る"),
    ("json", "JSON Lines で出力する"),
    ("show_prob", "確率も出力する"),
    ("print0", "NUL区切りで出力する(xargs -0 用)"),
    ("top", "確率の高い上位N件だけを表示する(--rank 指定時はスコア順)"),
    ("sort", "結果をパス順にソートする(ストリーミング出力をやめる)"),
    ("no_cache", "回答キャッシュを使わない"),
    ("stats", "統計(リクエスト数・トークン・推定コスト)を stderr に出す"),
    ("verbose", "枝刈りなどの判断を stderr に出す"),
    ("model", "使うモデル [既定: 設定ファイル → jev-latest]"),
    ("base_url", "APIのベースURL [既定: 設定ファイル → https://api.typesafe.ai]"),
];

/// 現在の言語に合わせてヘルプ文を差し替えた clap::Command を作る
fn command() -> clap::Command {
    let cmd = Args::command();
    if !i18n::ja() {
        return cmd.after_help(AFTER_HELP_EN);
    }
    HELP_JA
        .iter()
        .fold(cmd, |c, (id, h)| c.mut_arg(*id, |a| a.help(*h)))
        .about("Jevを中核に置いた意味検索型ファイルファインダ")
        .after_help(AFTER_HELP_JA)
}

#[derive(Default)]
struct Stats {
    requests: AtomicU64,
    cache_hits: AtomicU64,
    in_tok: AtomicU64,
    out_tok: AtomicU64,
    dirs: AtomicU64,
    pruned: AtomicU64,
    ignored: AtomicU64,
    denied: AtomicU64,
    files: AtomicU64,
    meta_decided: AtomicU64,
    deep: AtomicU64,
    errors: AtomicU64,
    blocked: AtomicU64,
}

struct Ctx {
    args: Args,
    jev: jev::Client,
    cache: cache::Cache,
    api: Arc<Semaphore>,
    fsem: Semaphore,
    excludes: GlobSet,
    exts: Vec<String>,
    classify: BTreeMap<String, Value>,
    home_library: Option<PathBuf>,
    pools: Pools,
    /// 予算切れで探索を打ち切ったら立つ
    stop: AtomicBool,
    st: Stats,
    now: u64,
}

#[derive(Debug)]
struct BudgetExhausted;
impl std::fmt::Display for BudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "API budget exhausted")
    }
}
impl std::error::Error for BudgetExhausted {}

impl Ctx {
    /// 1リクエストを送る(予算と同時実行数の管理つき)。キャッシュは judge 側で項目単位に持つ。
    /// `permit` はバッチを組む前に取った同時実行枠(なければここで取る)
    async fn ask(
        &self,
        state: &Value,
        qs: &BTreeMap<String, Question>,
        permit: Option<OwnedSemaphorePermit>,
    ) -> Result<BTreeMap<String, Answer>> {
        if self.stopped() {
            return Err(BudgetExhausted.into());
        }
        let n = self.st.requests.fetch_add(1, Relaxed);
        if self.args.budget.is_some_and(|b| n >= b) {
            self.st.requests.fetch_sub(1, Relaxed);
            self.stop.store(true, Relaxed);
            return Err(BudgetExhausted.into());
        }
        let _permit = match permit {
            Some(p) => p,
            None => self.api.clone().acquire_owned().await?,
        };
        let (a, u) = self.jev.evaluate(state, qs).await?;
        self.st.in_tok.fetch_add(u.input_tokens, Relaxed);
        self.st.out_tok.fetch_add(u.output_tokens, Relaxed);
        Ok(a)
    }

    /// 1項目(ファイルやディレクトリ)を判定する。キャッシュになければバッチ行きの列に並べて答えを待つ。
    async fn judge(&self, kind: Kind, item: Value) -> Result<BTreeMap<String, Answer>, Fail> {
        let qs: BTreeMap<String, Question> =
            questions(self, kind, "x").into_iter().map(|(k, q)| (k.to_string(), q)).collect();
        let mut st = shared_state(self, kind);
        st.insert("item".into(), item.clone());
        let key = cache::Cache::key(&self.jev.model, &Value::Object(st), &qs);
        if let Some(a) = self.cache.get(&key).await {
            self.st.cache_hits.fetch_add(1, Relaxed);
            return Ok(a);
        }
        if self.stopped() {
            return Err(Fail::Failed);
        }
        let (tx, rx) = oneshot::channel();
        self.pools.get(kind).send(Job { item, key, reply: tx }).await.map_err(|_| Fail::Failed)?;
        rx.await.unwrap_or(Err(Fail::Failed))
    }

    fn report(&self, what: &str, e: anyhow::Error) {
        if !e.is::<BudgetExhausted>() {
            self.st.errors.fetch_add(1, Relaxed);
            eprintln!("jfd: {what}: {e:#}");
        }
    }

    fn stopped(&self) -> bool {
        self.stop.load(Relaxed)
    }

    /// 既定で降りないディレクトリ(`--no-ignore` で無効)
    fn ignored_dir(&self, path: &Path, name: &str) -> bool {
        if self.args.no_ignore {
            return false;
        }
        const NAMES: &[&str] = &["node_modules", "__pycache__", "site-packages", "DerivedData", "Pods"];
        const BUNDLES: &[&str] = &["app", "framework", "bundle", "photoslibrary", "musiclibrary", "tvlibrary", "xcarchive"];
        NAMES.contains(&name)
            || path.extension().and_then(|x| x.to_str()).is_some_and(|x| BUNDLES.contains(&x))
            || (name == "Library"
                && self.home_library.is_some()
                && std::fs::canonicalize(path).ok() == self.home_library)
    }
}

// ---------- 質問の組み立て ----------

#[derive(Clone, Copy)]
enum Kind {
    Prune,
    Meta,
    Content,
}

// トークン削減のため、判定の基準や前提は state の `task` に1リクエスト1回だけ書き、
// 項目ごとの質問は「どの項目について問うか」だけの短い文にする。
// (以前は同じ指示文と criteria を項目の数だけ繰り返しており、段階1の入力の6割強がそれだった)

/// ファイルのパスは、バッチ内で共有するディレクトリ表 `dirs` と名前に分けて送る
const PATH_RULE: &str = "A file's path is `dirs[<its dir>]` + \"/\" + `name`.";

/// kind ごとの共通の指示(state の `task` に入れる)
fn task(kind: Kind) -> String {
    match kind {
        Kind::Prune => "Each question asks about one entry of `directories`. Judging by its path and \
             `sample_entries`, could that directory (anywhere beneath it) contain a file that satisfies `query`? \
             true = plausibly contains matching files; false = clearly unrelated to the query, e.g. third-party \
             dependencies, build output, caches, or assets irrelevant to it."
            .into(),
        Kind::Meta => format!(
            "Each question asks about one entry of `files`. Judge only from its path and metadata: its content \
             is NOT available, so stay uncertain if the name alone is not decisive. {PATH_RULE} \
             For match questions: true = the file satisfies `query`; false = it does not."
        ),
        Kind::Content => format!(
            "Each question asks about one entry of `files`. Judge from its path, metadata and `content_head` \
             (the first bytes of the file; `content_truncated` says whether more follows). {PATH_RULE} \
             For match questions: true = the file satisfies `query`; false = it does not."
        ),
    }
}

/// 項目 `id` について問う質問の一覧(キーは答えを取り出すときの名前)
fn questions(ctx: &Ctx, kind: Kind, id: &str) -> Vec<(&'static str, Question)> {
    match kind {
        Kind::Prune => vec![(
            "dir",
            Question::Noul { instructions: json!(format!("Could `directories.{id}` contain a match?")), criteria: None },
        )],
        Kind::Meta => vec![(
            "match",
            Question::Noul { instructions: json!(format!("Does `files.{id}` satisfy `query`?")), criteria: None },
        )],
        Kind::Content => {
            let mut v = vec![(
                "match",
                Question::Noul { instructions: json!(format!("Does `files.{id}` satisfy `query`?")), criteria: None },
            )];
            if !ctx.classify.is_empty() {
                v.push((
                    "label",
                    Question::Choice {
                        instructions: json!(format!("Which category best describes `files.{id}`?")),
                        criteria: ctx.classify.clone(),
                    },
                ));
            }
            if ctx.args.rank.is_some() {
                v.push((
                    "rank",
                    Question::Score {
                        instructions: json!(format!("Rate `files.{id}` on `aspect`.")),
                        criteria: ctx.args.levels.iter().map(|s| json!(s)).collect(),
                    },
                ));
            }
            v
        }
    }
}

/// state のうち、項目によらない部分(`query` と `task`、--rank の観点)
fn shared_state(ctx: &Ctx, kind: Kind) -> serde_json::Map<String, Value> {
    let mut m = serde_json::Map::new();
    m.insert("query".into(), json!(ctx.args.query));
    m.insert("task".into(), json!(task(kind)));
    if let (Kind::Content, Some(r)) = (kind, &ctx.args.rank) {
        m.insert("aspect".into(), json!(r));
    }
    m
}

/// ファイル項目の "path" を、バッチ内で共有するディレクトリ表への参照 "dir" と "name" に分ける
fn split_paths(items: &mut serde_json::Map<String, Value>) -> serde_json::Map<String, Value> {
    let mut ids: BTreeMap<String, String> = BTreeMap::new();
    let mut dirs = serde_json::Map::new();
    for item in items.values_mut() {
        let Some(m) = item.as_object_mut() else { continue };
        let Some(Value::String(path)) = m.remove("path") else { continue };
        let (dir, name) = path.rsplit_once('/').unwrap_or((".", &path));
        let dir = if dir.is_empty() { "/" } else { dir };
        let id = ids.entry(dir.to_string()).or_insert_with(|| {
            let id = format!("p{}", dirs.len());
            dirs.insert(id.clone(), json!(dir));
            id
        });
        let mut out = serde_json::Map::new();
        out.insert("dir".into(), json!(id));
        out.insert("name".into(), json!(name));
        out.extend(std::mem::take(m));
        *m = out;
    }
    dirs
}

fn noul(a: Option<&Answer>) -> f64 {
    match a {
        Some(Answer::Noul { noul }) => *noul,
        _ => 0.0,
    }
}

// ---------- バッチ化 ----------
// ディレクトリをまたいで項目を束ね、1リクエストで多数の質問を問う。
// 項目ごとのキャッシュなので、束ね方が変わっても再実行時にはキャッシュが効く。

/// 判定できなかった理由
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fail {
    /// 内容が API 手前のファイアウォール (Cloudflare) に遮断された
    Blocked,
    /// それ以外(エラーは報告済み、または予算切れ)
    Failed,
}

struct Job {
    item: Value,
    key: String,
    reply: oneshot::Sender<Result<BTreeMap<String, Answer>, Fail>>,
}

struct Pools {
    prune: mpsc::Sender<Job>,
    meta: mpsc::Sender<Job>,
    content: mpsc::Sender<Job>,
}

impl Pools {
    fn get(&self, kind: Kind) -> &mpsc::Sender<Job> {
        match kind {
            Kind::Prune => &self.prune,
            Kind::Meta => &self.meta,
            Kind::Content => &self.content,
        }
    }
}

/// バッチを組むときに、次の1件をこれ以上待たない間隔
const BATCH_IDLE: Duration = Duration::from_millis(3);
/// 最初の1件から送信までの最長の待ち
const BATCH_LINGER: Duration = Duration::from_millis(30);

/// 適応的なバッチ化:
/// - 先に API の同時実行枠を取ってからバッチを組む。枠が埋まっている間は項目が列にたまり、
///   空いた瞬間にまとめて大きなバッチで送る(混んでいるほどバッチが大きくなる)
/// - 枠が空いていれば、項目の流れが BATCH_IDLE 途切れた時点ですぐ送る(固定の待ち時間を払わない)
async fn run_pool(ctx: Arc<Ctx>, kind: Kind, mut rx: mpsc::Receiver<Job>, size: usize) {
    let size = size.max(1);
    while let Some(j) = rx.recv().await {
        let Ok(permit) = ctx.api.clone().acquire_owned().await else { return };
        let mut batch = vec![j];
        let deadline = tokio::time::Instant::now() + BATCH_LINGER;
        while batch.len() < size {
            match rx.try_recv() {
                Ok(j) => {
                    batch.push(j);
                    continue;
                }
                Err(mpsc::error::TryRecvError::Disconnected) => break,
                Err(mpsc::error::TryRecvError::Empty) => {}
            }
            let idle = (tokio::time::Instant::now() + BATCH_IDLE).min(deadline);
            match tokio::time::timeout_at(idle, rx.recv()).await {
                Ok(Some(j)) => batch.push(j),
                _ => break,
            }
        }
        if ctx.stopped() {
            continue; // reply を落とすと待ち手には Fail::Failed が返る
        }
        tokio::spawn(run_batch(ctx.clone(), kind, batch, Some(permit)));
    }
}

fn splittable(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("max_tokens_exceeded") || s.starts_with("HTTP 403") || s.starts_with("HTTP 413")
}

fn run_batch(ctx: Arc<Ctx>, kind: Kind, batch: Vec<Job>, permit: Option<OwnedSemaphorePermit>) -> BoxFut {
    Box::pin(run_batch_inner(ctx, kind, batch, permit))
}

async fn run_batch_inner(ctx: Arc<Ctx>, kind: Kind, mut batch: Vec<Job>, permit: Option<OwnedSemaphorePermit>) {
    let (coll, prefix) = match kind {
        Kind::Prune => ("directories", "d"),
        Kind::Meta | Kind::Content => ("files", "f"),
    };
    let mut items = serde_json::Map::new();
    let mut qs = BTreeMap::new();
    for (i, j) in batch.iter().enumerate() {
        let id = format!("{prefix}{i}");
        for (k, q) in questions(&ctx, kind, &id) {
            qs.insert(format!("{id}_{k}"), q);
        }
        items.insert(id, j.item.clone());
    }
    let mut state = shared_state(&ctx, kind);
    if !matches!(kind, Kind::Prune) {
        state.insert("dirs".into(), Value::Object(split_paths(&mut items)));
    }
    state.insert(coll.into(), Value::Object(items));
    let state = Value::Object(state);
    let mut answers = match ctx.ask(&state, &qs, permit).await {
        Ok(a) => a,
        // トークン上限超過やファイアウォールのブロック(特定ファイルの内容が原因)は、
        // 半分に割って送り直す。原因の1件だけが最後に落ちる
        Err(e) if batch.len() > 1 && splittable(&e) => {
            let second = batch.split_off(batch.len() / 2);
            tokio::spawn(run_batch(ctx.clone(), kind, second, None));
            return run_batch(ctx, kind, batch, None).await;
        }
        // 1件まで絞っても遮断される → そのファイルの内容が原因。呼び出し側で名前だけの判定に切り替える
        Err(e) if format!("{e:#}").starts_with("HTTP 403") => {
            for j in batch {
                let _ = j.reply.send(Err(Fail::Blocked));
            }
            return;
        }
        Err(e) => {
            let what = match (batch.len(), batch[0].item.get("path").and_then(|p| p.as_str())) {
                (1, Some(p)) => p.to_string(),
                (n, _) => tr!("batch of {n} items", "{n}件のバッチ"),
            };
            return ctx.report(&what, e);
        }
    };
    // 答えのキーは "{id}_{質問名}"。id ごとに振り分ける
    let mut per: Vec<BTreeMap<String, Answer>> = (0..batch.len()).map(|_| BTreeMap::new()).collect();
    for (k, a) in std::mem::take(&mut answers) {
        let Some((id, q)) = k.split_once('_') else { continue };
        let Some(i) = id.strip_prefix(prefix).and_then(|n| n.parse::<usize>().ok()) else { continue };
        if let Some(m) = per.get_mut(i) {
            m.insert(q.to_string(), a);
        }
    }
    ctx.cache.put_many(batch.iter().zip(&per).map(|(j, m)| (j.key.as_str(), m))).await;
    for (j, mine) in batch.into_iter().zip(per) {
        let _ = j.reply.send(Ok(mine));
    }
}

// ---------- ウォーカー ----------

struct Ent {
    name: String,
    is_dir: bool,
    is_file: bool,
    size: u64,
    mtime: u64,
}

struct FileEntry {
    path: PathBuf,
    size: u64,
    mtime: u64,
}

fn list_dir(dir: &Path) -> std::io::Result<Vec<Ent>> {
    let mut v = Vec::new();
    for e in std::fs::read_dir(dir)?.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        let (size, mtime) = if ft.is_file() {
            e.metadata().map_or((0, 0), |m| {
                let t = m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
                (m.len(), t.map_or(0, |d| d.as_secs()))
            })
        } else {
            (0, 0)
        };
        v.push(Ent {
            name: e.file_name().to_string_lossy().into_owned(),
            is_dir: ft.is_dir(),
            is_file: ft.is_file(),
            size,
            mtime,
        });
    }
    v.sort_by(|a, b| a.name.cmp(&b.name)); // サンプルを決定的にしてキャッシュを効かせる
    Ok(v)
}

fn show(p: &Path) -> String {
    let s = p.to_string_lossy();
    s.strip_prefix("./").unwrap_or(&s).to_string()
}

fn age_days(now: u64, mtime: u64) -> f64 {
    (now.saturating_sub(mtime) as f64 / 86400.0 * 10.0).round() / 10.0
}

/// 枝刈りの判定結果を待つための札。None = 判定待ち、Some(true) = 降りてよい
type Gate = watch::Receiver<Option<bool>>;

fn open_gate() -> Gate {
    watch::channel(Some(true)).1
}

/// 判定が出るまで待つ。判定役がいなくなったら降りる側に倒す (fail-open)
async fn pass(g: &mut Gate) -> bool {
    match g.wait_for(|v| v.is_some()).await {
        Ok(v) => v.unwrap_or(true),
        Err(_) => true,
    }
}

/// `parent` は親ディレクトリの枝刈り判定。親の判定を待たずに自分の一覧と枝刈りの問い合わせを
/// 先に始め(1段だけ先読み)、親が「降りない」と決めたらそこで捨てる。
/// 深い木でも「1段ごとに API の往復を1回待つ」直列の連鎖にならない
fn walk_dir(ctx: Arc<Ctx>, dir: PathBuf, depth: usize, hits: mpsc::Sender<Hit>, mut parent: Gate) -> BoxFut {
    Box::pin(async move {
        if ctx.stopped() || *parent.borrow() == Some(false) {
            return;
        }
        let listing = {
            let _p = ctx.fsem.acquire().await.expect("semaphore");
            let d = dir.clone();
            match tokio::task::spawn_blocking(move || list_dir(&d)).await {
                Ok(Ok(v)) => v,
                // macOS の保護フォルダ (~/Library/Safari など) は黙って飛ばす
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                    ctx.st.denied.fetch_add(1, Relaxed);
                    if ctx.args.verbose {
                        eprintln!("jfd: {}: {e}", show(&dir));
                    }
                    return;
                }
                Ok(Err(e)) => return eprintln!("jfd: {}: {e}", show(&dir)),
                Err(_) => return,
            }
        };
        // CACHEDIR.TAG はビルド出力やキャッシュの印 (cargo の target/ など)
        if depth > 0 && !ctx.args.no_ignore && listing.iter().any(|e| e.name == "CACHEDIR.TAG") {
            ctx.st.ignored.fetch_add(1, Relaxed);
            return;
        }

        // 意味的枝刈り: 読んだ一覧をそのまま使う(追加のI/Oなし)。答えは待たずに先へ進む
        let mut gate = if ctx.args.prune && depth >= ctx.args.prune_min_depth {
            let sample: Vec<String> = listing
                .iter()
                .take(60)
                .map(|e| if e.is_dir { format!("{}/", e.name) } else { e.name.clone() })
                .collect();
            let item = json!({ "path": show(&dir), "entry_count": listing.len(), "sample_entries": sample });
            let (tx, rx) = watch::channel(None);
            let c = ctx.clone();
            let d = dir.clone();
            tokio::spawn(async move {
                let keep = match c.judge(Kind::Prune, item).await {
                    Ok(a) => {
                        let p = noul(a.get("dir"));
                        if p < c.args.prune_threshold && c.args.verbose {
                            eprintln!("jfd: prune {:.2} {}", p, show(&d));
                        }
                        p >= c.args.prune_threshold
                    }
                    Err(_) => !c.stopped(), // fail-open: 判定できなければ降りる
                };
                tx.send_replace(Some(keep));
            });
            rx
        } else {
            open_gate()
        };

        // 親が降りると決めるまでは子に手を付けない(先読みは1段だけ)
        if !pass(&mut parent).await {
            return;
        }

        let mut subdirs = Vec::new();
        let mut files_here = Vec::new();
        for e in listing {
            if !ctx.args.hidden && e.name.starts_with('.') {
                continue;
            }
            let path = dir.join(&e.name);
            if ctx.excludes.is_match(&path) || ctx.excludes.is_match(&e.name) {
                continue;
            }
            if e.is_dir {
                if ctx.ignored_dir(&path, &e.name) {
                    ctx.st.ignored.fetch_add(1, Relaxed);
                } else if ctx.args.max_depth.map_or(true, |m| depth + 1 < m) {
                    subdirs.push(path);
                }
            } else if e.is_file {
                if !ctx.exts.is_empty() {
                    let ok = path
                        .extension()
                        .and_then(|x| x.to_str())
                        .is_some_and(|x| ctx.exts.iter().any(|w| w.eq_ignore_ascii_case(x)));
                    if !ok {
                        continue;
                    }
                }
                if ctx.args.max_size.is_some_and(|m| e.size > m) {
                    continue;
                }
                files_here.push(FileEntry { path, size: e.size, mtime: e.mtime });
            }
        }
        // 子ディレクトリは自分の判定を待たずに走らせる(子は自分の判定が出るまでファイルに手を付けない)
        for path in subdirs {
            tokio::spawn(walk_dir(ctx.clone(), path, depth + 1, hits.clone(), gate.clone()));
        }
        if !pass(&mut gate).await {
            if !ctx.stopped() {
                ctx.st.pruned.fetch_add(1, Relaxed);
            }
            return;
        }
        ctx.st.dirs.fetch_add(1, Relaxed);
        let mut files = JoinSet::new();
        for f in files_here {
            files.spawn(eval_file(ctx.clone(), f, hits.clone()));
        }
        while files.join_next().await.is_some() {}
    })
}

// ---------- ファイル判定 ----------

#[derive(Serialize)]
struct Hit {
    path: String,
    prob: f64,
    stage: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label_confidence: Option<f64>,
    /// --rank のスコア。0(最低の段階)〜1(最高の段階)に正規化
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    /// スコアに最も近い段階の名前
    #[serde(skip_serializing_if = "Option::is_none")]
    level: Option<String>,
}

/// tokio::fs だと open と read のたびにスレッドプールを往復するので、1回の spawn_blocking で済ませる
async fn read_head(p: &Path, n: usize) -> Result<(String, bool, bool)> {
    let p = p.to_path_buf();
    Ok(tokio::task::spawn_blocking(move || read_head_sync(&p, n)).await??)
}

fn read_head_sync(p: &Path, n: usize) -> std::io::Result<(String, bool, bool)> {
    use std::io::Read;
    let f = std::fs::File::open(p)?;
    let mut buf = Vec::with_capacity(n.min(1 << 20) + 1);
    f.take(n as u64 + 1).read_to_end(&mut buf)?;
    let truncated = buf.len() > n;
    buf.truncate(n);
    let binary = buf.iter().take(8192).any(|&b| b == 0);
    let text = if binary { String::new() } else { squeeze(&String::from_utf8_lossy(&buf), truncated) };
    Ok((text, truncated, binary))
}

/// 意味を変えずにトークンを減らす: 行末の空白を落とし、連続する空行は1行にまとめ、
/// 途中で切れた最後の行は捨てる(切れた単語や壊れた文字は判定の役に立たない)
fn squeeze(text: &str, truncated: bool) -> String {
    let body = match text.rfind('\n') {
        Some(i) if truncated && i >= text.len() / 2 => &text[..i],
        _ => text,
    };
    let mut out = String::with_capacity(body.len());
    let mut blank = false;
    for line in body.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            if blank || out.is_empty() {
                continue;
            }
            blank = true;
        } else {
            blank = false;
        }
        out.push_str(line);
        out.push('\n');
    }
    if !body.ends_with('\n') && out.ends_with('\n') {
        out.pop();
    }
    out
}

async fn eval_file(ctx: Arc<Ctx>, f: FileEntry, hits: mpsc::Sender<Hit>) {
    ctx.st.files.fetch_add(1, Relaxed);
    let path = show(&f.path);
    let mut item = json!({
        "path": path,
        "size_bytes": f.size,
        "age_days": age_days(ctx.now, f.mtime),
    });
    let needs_extra = !ctx.classify.is_empty() || ctx.args.rank.is_some();

    // 段階1: メタデータのみ
    let mut meta_p = None;
    if !ctx.args.deep {
        let Ok(a) = ctx.judge(Kind::Meta, item.clone()).await else { return };
        let p = noul(a.get("match"));
        meta_p = Some(p);
        if p <= ctx.args.meta_reject {
            ctx.st.meta_decided.fetch_add(1, Relaxed);
            return;
        }
        if p >= ctx.args.meta_accept && !needs_extra {
            ctx.st.meta_decided.fetch_add(1, Relaxed);
            let _ = hits.send(Hit::bare(path, p, "meta")).await;
            return;
        }
    }

    // 段階2: 内容の先頭を読み、判定・分類・採点をまとめて問う
    ctx.st.deep.fetch_add(1, Relaxed);
    let read = {
        let _p = ctx.fsem.acquire().await.expect("semaphore");
        read_head(&f.path, ctx.args.head_bytes).await
    };
    let (head, truncated, binary) = match read {
        Ok(v) => v,
        Err(e) => return ctx.report(&path, e),
    };
    let meta_item = item.clone();
    let m = item.as_object_mut().expect("object");
    if binary {
        m.insert("content".into(), json!("(binary file; content omitted)"));
    } else {
        m.insert("content_head".into(), json!(head));
        m.insert("content_truncated".into(), json!(truncated));
    }
    let a = match ctx.judge(Kind::Content, item).await {
        Ok(a) => a,
        Err(Fail::Blocked) => return eval_blocked(&ctx, path, meta_item, meta_p, &hits).await,
        Err(Fail::Failed) => return,
    };
    let p = noul(a.get("match"));
    if p < ctx.args.threshold {
        return;
    }
    let (label, label_confidence) = match a.get("label") {
        Some(Answer::Choice { choice, confidence, .. }) => (Some(choice.clone()), Some(*confidence)),
        _ => (None, None),
    };
    let (score, level) = match a.get("rank") {
        Some(Answer::Score { score, .. }) => {
            // API のスコアは段階の番号 (0..L-1) の期待値。0〜1 に揃え、最も近い段階名を添える
            let top = (ctx.args.levels.len() - 1) as f64;
            let idx = score.round().clamp(0.0, top) as usize;
            (Some((score / top).clamp(0.0, 1.0)), Some(ctx.args.levels[idx].clone()))
        }
        _ => (None, None),
    };
    let _ = hits.send(Hit { path, prob: p, stage: "content", label, label_confidence, score, level }).await;
}

/// 内容がファイアウォールに遮断されたファイルは、名前とメタデータだけの判定で代用する
async fn eval_blocked(ctx: &Ctx, path: String, item: Value, meta_p: Option<f64>, hits: &mpsc::Sender<Hit>) {
    ctx.st.blocked.fetch_add(1, Relaxed);
    let p = match meta_p {
        Some(p) => p,
        None => match ctx.judge(Kind::Meta, item).await {
            Ok(a) => noul(a.get("match")),
            Err(_) => return,
        },
    };
    eprintln!("{}", tr!(
        "jfd: {path}: content was blocked by the API's firewall (Cloudflare); judged by name and metadata only (p={p:.2})",
        "jfd: {path}: 内容がAPIのファイアウォール(Cloudflare)に遮断されたため、名前とメタデータだけで判定しました (確率 {p:.2})"
    ));
    if p >= ctx.args.threshold {
        let _ = hits.send(Hit::bare(path, p, "meta_blocked")).await;
    }
}

impl Hit {
    fn bare(path: String, prob: f64, stage: &'static str) -> Self {
        Self { path, prob, stage, label: None, label_confidence: None, score: None, level: None }
    }
}

// ---------- 出力 ----------

struct Output {
    buffered: bool,
    buf: Vec<Hit>,
}

impl Output {
    fn line(a: &Args, h: &Hit) -> String {
        let mut s = if a.json {
            serde_json::to_string(h).unwrap_or_default()
        } else {
            // 列をそろえるため、値がない欄は "-"(内容を遮断されたファイルなど)
            let mut parts = Vec::new();
            if a.rank.is_some() {
                parts.push(h.score.map_or("-".into(), |sc| format!("{sc:.2}")));
                parts.push(h.level.clone().unwrap_or_else(|| "-".into()));
            }
            if !a.classify.is_empty() {
                parts.push(h.label.clone().unwrap_or_else(|| "-".into()));
            }
            if a.show_prob {
                parts.push(format!("{:.2}", h.prob));
            }
            parts.push(h.path.clone());
            parts.join("\t")
        };
        s.push(if a.print0 { '\0' } else { '\n' });
        s
    }

    fn write(a: &Args, h: &Hit) {
        let mut out = std::io::stdout().lock();
        if out.write_all(Self::line(a, h).as_bytes()).and_then(|_| out.flush()).is_err() {
            std::process::exit(0); // `| head` などでパイプが閉じられた
        }
    }

    fn emit(&mut self, a: &Args, h: Hit) {
        if self.buffered {
            self.buf.push(h);
        } else {
            Self::write(a, &h);
        }
    }

    fn finish(mut self, a: &Args) {
        if a.rank.is_some() {
            self.buf.sort_by(|x, y| y.score.unwrap_or(-1.0).total_cmp(&x.score.unwrap_or(-1.0)));
        } else if a.top.is_some() {
            self.buf.sort_by(|x, y| y.prob.total_cmp(&x.prob).then_with(|| x.path.cmp(&y.path)));
        } else {
            self.buf.sort_by(|x, y| x.path.cmp(&y.path));
        }
        if let Some(n) = a.top {
            self.buf.truncate(n);
        }
        for h in &self.buf {
            Self::write(a, h);
        }
    }
}

// ---------- main ----------

#[tokio::main]
async fn main() -> Result<()> {
    let argv: Vec<String> = std::env::args().collect();
    if config::dispatch(&argv).await? {
        return Ok(());
    }
    let mut cfg = config::Config::load()?;
    i18n::set(cfg.lang.unwrap_or_else(i18n::Lang::detect));
    let args = Args::from_arg_matches(&command().get_matches_from(&argv)).unwrap_or_else(|e| e.exit());
    let key = config::resolve_key(&mut cfg).await?;
    let model = args.model.clone().or(cfg.model).unwrap_or_else(|| config::DEFAULT_MODEL.into());
    let base_url = args.base_url.clone().or(cfg.base_url).unwrap_or_else(|| config::DEFAULT_BASE_URL.into());
    if !(2..=10).contains(&args.levels.len()) {
        bail!(tr!("--levels takes 2 to 10 levels", "--levels は2〜10段階で指定してください"));
    }
    let mut gb = GlobSetBuilder::new();
    for g in &args.excludes {
        gb.add(Glob::new(g)?);
    }
    let classify = args
        .classify
        .iter()
        .map(|kv| {
            kv.split_once('=')
                .map(|(k, v)| (k.to_string(), json!(v)))
                .with_context(|| tr!("--classify must be LABEL=DESC", "--classify は LABEL=DESC の形式で指定してください"))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let jobs = args.jobs.max(1);
    let (prune_tx, prune_rx) = mpsc::channel(8192);
    let (meta_tx, meta_rx) = mpsc::channel(8192);
    let (content_tx, content_rx) = mpsc::channel(8192);
    let started = Instant::now();
    let jev = jev::Client::new(&base_url, key, model)?;
    let ctx = Arc::new(Ctx {
        jev,
        cache: cache::Cache::new(!args.no_cache),
        api: Arc::new(Semaphore::new(jobs)),
        fsem: Semaphore::new(128),
        excludes: gb.build()?,
        exts: args.exts.iter().map(|e| e.trim_start_matches('.').to_string()).collect(),
        classify,
        home_library: std::env::var_os("HOME").and_then(|h| std::fs::canonicalize(PathBuf::from(h).join("Library")).ok()),
        pools: Pools { prune: prune_tx, meta: meta_tx, content: content_tx },
        stop: AtomicBool::new(false),
        st: Stats::default(),
        now: SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()),
        args,
    });

    tokio::spawn(run_pool(ctx.clone(), Kind::Prune, prune_rx, ctx.args.batch));
    tokio::spawn(run_pool(ctx.clone(), Kind::Meta, meta_rx, ctx.args.batch));
    tokio::spawn(run_pool(ctx.clone(), Kind::Content, content_rx, ctx.args.deep_batch));

    let (hit_tx, mut hit_rx) = mpsc::channel::<Hit>(1024);
    for root in ctx.args.paths.clone() {
        let md = std::fs::metadata(&root).with_context(|| format!("{}", root.display()))?;
        if md.is_file() {
            let mtime = md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_secs());
            tokio::spawn(eval_file(ctx.clone(), FileEntry { path: root, size: md.len(), mtime }, hit_tx.clone()));
        } else {
            tokio::spawn(walk_dir(ctx.clone(), root, 0, hit_tx.clone(), open_gate()));
        }
    }
    drop(hit_tx); // 全ウォーカーとファイル判定が終わると hit_rx が閉じる

    let mut out = Output { buffered: ctx.args.sort || ctx.args.rank.is_some() || ctx.args.top.is_some(), buf: Vec::new() };
    while let Some(hit) = hit_rx.recv().await {
        out.emit(&ctx.args, hit);
    }
    out.finish(&ctx.args);

    let s = &ctx.st;
    if ctx.stopped() {
        let b = ctx.args.budget.unwrap_or(0);
        eprintln!("{}", tr!(
            "jfd: stopped after reaching --budget {b}; results are incomplete. Raise --budget, or narrow the search with a path, -e or --prune.",
            "jfd: --budget {b} に達したため探索を打ち切りました。結果は不完全です。--budget を増やすか、場所・-e・--prune で範囲を絞ってください。"
        ));
    }
    let denied = s.denied.load(Relaxed);
    if denied > 0 && !ctx.args.verbose && !ctx.args.stats {
        eprintln!("{}", tr!(
            "jfd: skipped {denied} directories without permission (-v to list)",
            "jfd: 権限のないディレクトリ {denied} 個を飛ばしました (-v で一覧)"
        ));
    }
    if ctx.args.stats {
        let tin = s.in_tok.load(Relaxed);
        eprintln!(
            "dirs={} pruned={} ignored={} denied={} files={} meta_decided={} deep={} blocked={} requests={} cache_hits={} errors={}",
            s.dirs.load(Relaxed), s.pruned.load(Relaxed), s.ignored.load(Relaxed), denied,
            s.files.load(Relaxed), s.meta_decided.load(Relaxed), s.deep.load(Relaxed), s.blocked.load(Relaxed),
            s.requests.load(Relaxed), s.cache_hits.load(Relaxed), s.errors.load(Relaxed),
        );
        eprintln!("elapsed={:.3}s", started.elapsed().as_secs_f64());
        eprintln!(
            "tokens in={} out={}  est. cost ≈ ${:.5} (at $0.042 / 1M input tokens)",
            tin, s.out_tok.load(Relaxed), tin as f64 * 0.042 / 1e6
        );
    }
    Ok(())
}
