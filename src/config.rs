//! 設定ファイル (~/.config/jfd/config.json) と `jfd setup` / `jfd config` / `jfd update` サブコマンド。
//! 優先順位: コマンドライン > 環境変数 > 設定ファイル > 既定値
use crate::i18n::{self, Lang};
use crate::jev::{self, Question};
use crate::tr;
use crate::update;
use anyhow::{bail, Context as _, Result};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;

pub const DEFAULT_MODEL: &str = "jev-latest";
pub const DEFAULT_BASE_URL: &str = "https://api.typesafe.ai";
pub const KEY_ENV: &str = "TYPESAFE_API_KEY";

#[derive(Serialize, Deserialize, Default, Debug)]
pub struct Config {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<Lang>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `jfd update` で再ビルドするソースの場所(未設定なら、ビルド時の git チェックアウトか GitHub のリリース)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_dir: Option<PathBuf>,
}

pub fn path() -> Result<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .context("HOME is not set")?;
    Ok(base.join("jfd").join("config.json"))
}

impl Config {
    pub fn load() -> Result<Self> {
        let p = path()?;
        match std::fs::read(&p) {
            Ok(b) => serde_json::from_slice(&b)
                .with_context(|| format!("{}: {}", p.display(), tr!("invalid config file", "設定ファイルが壊れています"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| p.display().to_string()),
        }
    }

    /// APIキーを含むので、ディレクトリは 0700、ファイルは 0600 で書く
    pub fn save(&self) -> Result<PathBuf> {
        let p = path()?;
        let dir = p.parent().expect("config path has a parent");
        std::fs::create_dir_all(dir)?;
        let tmp = p.with_extension(format!("tmp{}", std::process::id()));
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(&tmp, &p)?;
        Ok(p)
    }

    fn save_and_report(&self) -> Result<()> {
        let p = self.save()?.display().to_string();
        eprintln!("{}", tr!("Saved: {p}", "保存しました: {p}"));
        Ok(())
    }
}

/// APIキーを解決する。見つからず端末上で対話できるなら、その場でセットアップに入る。
pub async fn resolve_key(cfg: &mut Config) -> Result<String> {
    if let Some(k) = std::env::var(KEY_ENV).ok().filter(|k| !k.trim().is_empty()) {
        return Ok(k);
    }
    if let Some(k) = cfg.api_key.clone().filter(|k| !k.trim().is_empty()) {
        return Ok(k);
    }
    if std::io::stdin().is_terminal() && std::io::stderr().is_terminal() {
        eprintln!("{}\n", tr!("jfd: no API key yet — starting first-time setup.", "jfd: APIキーが未設定なので、初回セットアップを始めます。"));
        setup(cfg).await?;
        eprintln!();
        return cfg.api_key.clone().context("no API key");
    }
    bail!(tr!(
        "No API key. Run `jfd setup` or set the {KEY_ENV} environment variable",
        "APIキーが未設定です。`jfd setup` を実行するか、環境変数 {KEY_ENV} を設定してください"
    ))
}

fn mask(k: &str) -> String {
    let n = k.chars().count();
    if n <= 8 {
        return "*".repeat(n);
    }
    let head: String = k.chars().take(4).collect();
    let tail: String = k.chars().skip(n - 4).collect();
    format!("{head}…{tail}")
}

fn prompt(label: &str, default: &str) -> Result<String> {
    eprint!("{label} [{default}]: ");
    std::io::stderr().flush()?;
    let mut s = String::new();
    std::io::stdin().lock().read_line(&mut s)?;
    let s = s.trim();
    Ok(if s.is_empty() { default.to_string() } else { s.to_string() })
}

fn prompt_key() -> Result<String> {
    Ok(rpassword::prompt_password(tr!("TypeSafe API key (input hidden): ", "TypeSafe APIキー (入力は表示されません): "))?
        .trim()
        .to_string())
}

/// 1問だけの小さなリクエストでキーと接続先を確かめる
async fn verify(key: &str, cfg: &Config) -> Result<()> {
    let base = cfg.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL);
    let model = cfg.model.clone().unwrap_or_else(|| DEFAULT_MODEL.to_string());
    let client = jev::Client::new(base, key.to_string(), model)?;
    let qs = BTreeMap::from([(
        "ok".to_string(),
        Question::Noul { instructions: json!("Is this a greeting?"), criteria: None },
    )]);
    client.evaluate(&json!("hello"), &qs).await?;
    Ok(())
}

async fn store_key(cfg: &mut Config, key: String) -> Result<()> {
    if key.is_empty() {
        bail!(tr!("API key is empty", "APIキーが空です"));
    }
    eprint!("{}", tr!("Checking the key… ", "キーを確認しています… "));
    match verify(&key, cfg).await {
        Ok(()) => eprintln!("OK"),
        Err(e) => {
            eprintln!("{}\n  {e:#}", tr!("failed", "失敗"));
            let ans = prompt(&tr!("Save it anyway? (y/N)", "それでも保存しますか? (y/N)"), "N")?;
            if !ans.eq_ignore_ascii_case("y") {
                bail!(tr!("Not saved", "保存を中止しました"));
            }
        }
    }
    cfg.api_key = Some(key);
    cfg.save_and_report()
}

fn choose_lang(cfg: &Config) -> Result<Lang> {
    let cur = cfg.lang.unwrap_or_else(Lang::detect);
    eprintln!("Language / 言語:  1) English   2) 日本語");
    let default = if cur == Lang::Ja { "2" } else { "1" };
    let ans = prompt("Choose / 選択", default)?;
    Ok(match ans.as_str() {
        "1" => Lang::En,
        "2" => Lang::Ja,
        other => Lang::parse(other).unwrap_or(cur),
    })
}

/// 対話式の初回セットアップ(再実行すれば上書き)
pub async fn setup(cfg: &mut Config) -> Result<()> {
    let lang = choose_lang(cfg)?;
    cfg.lang = Some(lang);
    i18n::set(lang);
    eprintln!();
    eprintln!("{}", tr!("Get an API key from the TypeSafe dashboard: https://typesafe.ai", "APIキーは TypeSafe のダッシュボードで発行できます: https://typesafe.ai"));
    if let Some(k) = &cfg.api_key {
        let k = mask(k);
        eprintln!("{}", tr!("Current key: {k} (press Enter to keep it)", "現在のキー: {k} (Enterだけで維持)"));
    }
    let key = match prompt_key()? {
        k if k.is_empty() => cfg.api_key.clone().unwrap_or_default(),
        k => k,
    };
    let model = prompt(&tr!("Model", "モデル"), cfg.model.as_deref().unwrap_or(DEFAULT_MODEL))?;
    cfg.model = (model != DEFAULT_MODEL).then_some(model);
    store_key(cfg, key).await
}

// ---------- サブコマンド ----------

#[derive(Parser)]
#[command(name = "setup", bin_name = "jfd setup", about = "Interactive setup: language, API key, model")]
struct SetupCli;

#[derive(Parser)]
#[command(name = "update", bin_name = "jfd update", about = "Update jfd to the latest version (GitHub release, or git pull + rebuild when installed from source)")]
struct UpdateCli {
    /// Only check whether a newer release exists
    #[arg(long)]
    check: bool,
    /// Source installs: skip git pull and just rebuild the local source
    #[arg(long)]
    no_pull: bool,
    /// Release installs: reinstall even if already up to date
    #[arg(long)]
    force: bool,
}

#[derive(Parser)]
#[command(name = "config", bin_name = "jfd config", about = "Show or change jfd settings")]
struct ConfigCli {
    #[command(subcommand)]
    cmd: Option<ConfigCmd>,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Show current settings (the key is masked)
    Show,
    /// Change the API key. Omit KEY to type it hidden; `-` reads it from stdin
    SetKey { key: Option<String> },
    /// Change the display language (en / ja)
    SetLang { lang: String },
    /// Change the default model
    SetModel { model: String },
    /// Change the API base URL
    SetBaseUrl { url: String },
    /// Change where `jfd update` finds the source
    SetSource { dir: PathBuf },
    /// Reset a setting to its default (key / lang / model / base-url / source / all)
    Unset { what: String },
    /// Check that the saved key can reach the API
    Test,
    /// Print the config file path
    Path,
}

const CONFIG_HELP_JA: &[(&str, &str)] = &[
    ("show", "現在の設定を表示する(キーは伏せ字)"),
    ("set-key", "APIキーを変更する。KEY を省略すると非表示で入力、`-` なら標準入力から読む"),
    ("set-lang", "表示言語を変更する (en / ja)"),
    ("set-model", "既定のモデルを変更する"),
    ("set-base-url", "APIのベースURLを変更する"),
    ("set-source", "`jfd update` が使うソースの場所を変更する"),
    ("unset", "設定項目を既定値に戻す (key / lang / model / base-url / source / all)"),
    ("test", "保存済みのキーでAPIに接続できるか確かめる"),
    ("path", "設定ファイルのパスを表示する"),
];

fn parse<T: CommandFactory + FromArgMatches>(argv: &[String], ja: impl FnOnce(clap::Command) -> clap::Command) -> T {
    let cmd = if i18n::ja() { ja(T::command()) } else { T::command() };
    T::from_arg_matches(&cmd.get_matches_from(argv)).unwrap_or_else(|e| e.exit())
}

/// `jfd setup|config|update ...` なら処理して true を返す。それ以外(=通常の検索)は false。
/// これらの語そのものを検索したいときは `jfd -- config` のように書く。
pub async fn dispatch(argv: &[String]) -> Result<bool> {
    let Some(first) = argv.get(1) else { return Ok(false) };
    if !matches!(first.as_str(), "setup" | "config" | "update") {
        return Ok(false);
    }
    let mut cfg = Config::load()?;
    i18n::set(cfg.lang.unwrap_or_else(Lang::detect));
    let sub = &argv[1..];
    match first.as_str() {
        "setup" => {
            parse::<SetupCli>(sub, |c| c.about("言語・APIキー・モデルを対話式に設定する"));
            if !std::io::stdin().is_terminal() {
                bail!(tr!(
                    "`jfd setup` needs a terminal. In scripts use: echo \"$KEY\" | jfd config set-key -",
                    "`jfd setup` は端末で実行してください。スクリプトからは: echo \"$KEY\" | jfd config set-key -"
                ));
            }
            setup(&mut cfg).await?;
            eprintln!("\n{}", tr!(
                "All set. Try: jfd \"README files that explain installation\"",
                "準備完了です。例: jfd \"インストール手順を説明しているREADME\""
            ));
        }
        "update" => {
            let u: UpdateCli = parse(sub, |c| {
                c.about("jfd を最新版に更新する(GitHub のリリース、またはソースから入れた場合は git pull + 再ビルド)")
                    .mut_arg("check", |a| a.help("新しいリリースがあるか確認するだけ"))
                    .mut_arg("no_pull", |a| a.help("ソース版: git pull をせず、手元のソースを再ビルドするだけにする"))
                    .mut_arg("force", |a| a.help("リリース版: 最新でも入れ直す"))
            });
            update::run(&cfg, update::Opts { check: u.check, no_pull: u.no_pull, force: u.force }).await?;
        }
        _ => {
            let c: ConfigCli = parse(sub, |c| {
                CONFIG_HELP_JA
                    .iter()
                    .fold(c, |c, (name, h)| c.mut_subcommand(*name, |s| s.about(*h)))
                    .about("jfd の設定を表示・変更する")
            });
            config_cmd(&mut cfg, c.cmd.unwrap_or(ConfigCmd::Show)).await?;
        }
    }
    Ok(true)
}

async fn config_cmd(cfg: &mut Config, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Show => {
            let env_key = std::env::var(KEY_ENV).ok().filter(|k| !k.is_empty());
            let key = match (&env_key, &cfg.api_key) {
                (Some(k), _) => format!("{} ({KEY_ENV})", mask(k)),
                (None, Some(k)) => format!("{} ({})", mask(k), tr!("config file", "設定ファイル")),
                (None, None) => tr!("not set — run `jfd setup`", "未設定 — `jfd setup` を実行してください"),
            };
            let lang = match cfg.lang {
                Some(l) => l.code().to_string(),
                None => format!("{} ({})", Lang::detect().code(), tr!("from locale", "ロケールから自動")),
            };
            println!("config file : {}", path()?.display());
            println!("api key     : {key}");
            println!("lang        : {lang}");
            println!("model       : {}", cfg.model.as_deref().unwrap_or(DEFAULT_MODEL));
            println!("base url    : {}", cfg.base_url.as_deref().unwrap_or(DEFAULT_BASE_URL));
            let upd = match update::mode(cfg) {
                update::Mode::Source(d) => format!("{} ({})", tr!("from source", "ソースから"), d.display()),
                update::Mode::Release => format!("{} (github.com/{})", tr!("GitHub releases", "GitHub のリリース"), update::repo()),
            };
            println!("update      : {upd}");
        }
        ConfigCmd::SetKey { key } => {
            let key = match key.as_deref() {
                None => prompt_key()?,
                Some("-") => {
                    let mut s = String::new();
                    std::io::stdin().lock().read_line(&mut s)?;
                    s.trim().to_string()
                }
                Some(k) => {
                    eprintln!("{}", tr!(
                        "jfd: note: a key passed as an argument stays in your shell history",
                        "jfd: 注意: 引数で渡したキーはシェル履歴に残ります"
                    ));
                    k.trim().to_string()
                }
            };
            store_key(cfg, key).await?;
        }
        ConfigCmd::SetLang { lang } => {
            let l = Lang::parse(&lang).with_context(|| format!("en / ja: {lang}"))?;
            cfg.lang = Some(l);
            i18n::set(l);
            cfg.save_and_report()?;
        }
        ConfigCmd::SetModel { model } => {
            cfg.model = Some(model);
            cfg.save_and_report()?;
        }
        ConfigCmd::SetBaseUrl { url } => {
            cfg.base_url = Some(url);
            cfg.save_and_report()?;
        }
        ConfigCmd::SetSource { dir } => {
            let dir = std::fs::canonicalize(&dir).with_context(|| dir.display().to_string())?;
            cfg.source_dir = Some(dir);
            cfg.save_and_report()?;
        }
        ConfigCmd::Unset { what } => {
            match what.as_str() {
                "key" | "api-key" => cfg.api_key = None,
                "lang" => cfg.lang = None,
                "model" => cfg.model = None,
                "base-url" => cfg.base_url = None,
                "source" => cfg.source_dir = None,
                "all" => *cfg = Config::default(),
                other => bail!(tr!(
                    "Unknown setting: {other} (key / lang / model / base-url / source / all)",
                    "不明な項目です: {other} (key / lang / model / base-url / source / all)"
                )),
            }
            cfg.save_and_report()?;
        }
        ConfigCmd::Test => {
            let key = std::env::var(KEY_ENV)
                .ok()
                .filter(|k| !k.is_empty())
                .or(cfg.api_key.clone())
                .with_context(|| tr!("No API key. Run `jfd setup`", "APIキーが未設定です。`jfd setup` を実行してください"))?;
            verify(&key, cfg).await?;
            eprintln!("{}", tr!("OK: connected to the API", "OK: APIに接続できました"));
        }
        ConfigCmd::Path => println!("{}", path()?.display()),
    }
    Ok(())
}
