//! `jfd update`: 2通りの入れ方に合わせて更新する。
//!   - ソースから入れた場合(git のチェックアウトがある): git pull → cargo install --path
//!   - GitHub Releases のバイナリを入れた場合: 最新版をダウンロードし、SHA-256 を確かめて自分自身を置き換える
use crate::config::Config;
use crate::tr;
use anyhow::{bail, Context as _, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

const VERSION: &str = env!("CARGO_PKG_VERSION");
const TARGET: &str = env!("JFD_TARGET");

pub fn repo() -> &'static str {
    env!("CARGO_PKG_REPOSITORY").trim_start_matches("https://github.com/")
}

pub enum Mode {
    Source(PathBuf),
    Release,
}

/// 設定で指定されたソース、またはビルド時のソース(git チェックアウトに限る)があればソース方式
pub fn mode(cfg: &Config) -> Mode {
    if let Some(d) = &cfg.source_dir {
        return Mode::Source(d.clone());
    }
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    // `cargo install --git` の作業コピー (~/.cargo/git/checkouts/…) は自分のソースではないので除く
    let in_cargo_home = d.components().any(|c| c.as_os_str() == ".cargo");
    if d.join("Cargo.toml").is_file() && d.join(".git").exists() && !in_cargo_home {
        Mode::Source(d)
    } else {
        Mode::Release
    }
}

pub struct Opts {
    pub check: bool,
    pub no_pull: bool,
    pub force: bool,
}

pub async fn run(cfg: &Config, o: Opts) -> Result<()> {
    if o.check {
        return check().await;
    }
    match mode(cfg) {
        Mode::Source(dir) => from_source(&dir, o.no_pull),
        Mode::Release => from_release(o.force).await,
    }
}

// ---------- GitHub Releases ----------

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

fn http() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder().user_agent(concat!("jfd/", env!("CARGO_PKG_VERSION"))).build()?)
}

async fn latest(client: &reqwest::Client) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{}/releases/latest", repo());
    let r = client.get(&url).send().await?;
    if r.status() == reqwest::StatusCode::NOT_FOUND {
        bail!(tr!("No release published yet at {url}", "まだリリースが公開されていません: {url}"));
    }
    Ok(r.error_for_status()?.json().await?)
}

/// "1.2.3" 同士を数値で比べる
fn newer(a: &str, b: &str) -> bool {
    let p = |s: &str| s.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) > p(b)
}

async fn check() -> Result<()> {
    let rel = latest(&http()?).await?;
    let v = rel.tag_name.trim_start_matches('v');
    if newer(v, VERSION) {
        println!("{}", tr!("jfd {v} is available (you have {VERSION}). Run `jfd update`.", "jfd {v} が出ています(現在 {VERSION})。`jfd update` で更新できます。"));
    } else {
        println!("{}", tr!("jfd {VERSION} is up to date.", "jfd {VERSION} は最新です。"));
    }
    Ok(())
}

async fn from_release(force: bool) -> Result<()> {
    let client = http()?;
    let rel = latest(&client).await?;
    let v = rel.tag_name.trim_start_matches('v').to_string();
    if !newer(&v, VERSION) && !force {
        eprintln!("{}", tr!("jfd {VERSION} is up to date.", "jfd {VERSION} は最新です。"));
        return Ok(());
    }
    let name = format!("jfd-{TARGET}");
    let find = |n: &str| rel.assets.iter().find(|a| a.name == n).map(|a| a.browser_download_url.clone());
    let (Some(bin_url), Some(sum_url)) = (find(&name), find(&format!("{name}.sha256"))) else {
        let r = repo();
        bail!(tr!(
            "No prebuilt binary for {TARGET}. Install from source: cargo install --git https://github.com/{r}",
            "{TARGET} 向けのビルド済みバイナリがありません。ソースから入れてください: cargo install --git https://github.com/{r}"
        ));
    };
    eprintln!("{}", tr!("==> downloading jfd {v} ({TARGET})", "==> jfd {v} をダウンロード中 ({TARGET})"));
    let bin = client.get(&bin_url).send().await?.error_for_status()?.bytes().await?;
    let sum = client.get(&sum_url).send().await?.error_for_status()?.text().await?;
    let want = sum.split_whitespace().next().unwrap_or_default().to_ascii_lowercase();
    let got: String = Sha256::digest(&bin).iter().map(|b| format!("{b:02x}")).collect();
    if want != got {
        bail!(tr!("Checksum mismatch; not installed", "チェックサムが一致しないため、入れ替えを中止しました"));
    }
    replace_self(&bin)?;
    eprintln!("{}", tr!("Updated: {VERSION} → {v}", "更新しました: {VERSION} → {v}"));
    Ok(())
}

/// 同じディレクトリに書いてから rename で差し替える(実行中のバイナリでも unix なら安全)
fn replace_self(bin: &[u8]) -> Result<()> {
    let exe = std::env::current_exe()?.canonicalize()?;
    let tmp = exe.with_file_name(format!(".jfd-update-{}", std::process::id()));
    let d = exe.parent().map_or(String::new(), |p| p.display().to_string());
    std::fs::write(&tmp, bin).with_context(|| tr!("cannot write to {d}", "{d} に書き込めません"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    }
    if let Err(e) = std::fs::rename(&tmp, &exe) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e).with_context(|| exe.display().to_string());
    }
    Ok(())
}

// ---------- ソースから ----------

fn run_cmd(cmd: &mut Command) -> Result<()> {
    let name = format!("{:?}", cmd.get_program());
    let st = cmd.status().with_context(|| tr!("could not run {name}", "{name} を実行できません"))?;
    if !st.success() {
        bail!(tr!("{name} failed ({st})", "{name} が失敗しました ({st})"));
    }
    Ok(())
}

fn has_git_remote(dir: &Path) -> bool {
    dir.join(".git").exists()
        && Command::new("git")
            .arg("-C")
            .arg(dir)
            .arg("remote")
            .output()
            .is_ok_and(|o| o.status.success() && !o.stdout.trim_ascii().is_empty())
}

fn from_source(dir: &Path, no_pull: bool) -> Result<()> {
    if !dir.join("Cargo.toml").is_file() {
        let d = dir.display();
        bail!(tr!(
            "Source not found at {d}. Point to it with `jfd config set-source <DIR>`, or `jfd config unset source` to update from GitHub releases",
            "ソースが見つかりません: {d}。`jfd config set-source <DIR>` で場所を指定するか、`jfd config unset source` で GitHub のリリースから更新してください"
        ));
    }
    eprintln!("jfd {VERSION} — {}", dir.display());
    if !no_pull && has_git_remote(dir) {
        eprintln!("{}", tr!("==> git pull", "==> git pull (最新のソースを取得)"));
        run_cmd(Command::new("git").arg("-C").arg(dir).args(["pull", "--ff-only"]))?;
    }
    eprintln!("{}", tr!("==> cargo install (rebuild and reinstall)", "==> cargo install (再ビルドして入れ直し)"));
    run_cmd(Command::new("cargo").arg("install").arg("--locked").arg("--path").arg(dir))?;
    eprintln!("{}", tr!("Done. Check with `jfd --version`.", "完了しました。`jfd --version` で確認できます。"));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn version_compare() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.0"));
        assert!(!newer("0.1.0", "0.1.0"));
        assert!(!newer("0.1.0", "0.2.0"));
    }
}
