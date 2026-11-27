//! 软件更新：从 Gitee / GitHub Releases 探测最新版，下载 deb 并安装。
//!
//! 设计参考 LitheZones（同项目的成熟实现）：
//!   · Gitee Release 不支持自定义附件，只能拿版本号；deb 从 GitHub Release
//!     的 assets 取，GitHub 不可用时按约定拼标准下载路径。
//!   · 安装不追求零交互：先试 sudo -n（免密静默），失败再 pkexec（弹一次
//!     系统密码框），最后退回 xdg-open 让系统安装器接管。密码不经本程序。
//!   · 所有网络/安装动作都在 spawn_blocking 线程里，不卡托盘主循环。

use serde_json::Value;
use std::cmp::Ordering;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

/// 仓库坐标（与 LitheZones 同一 owner）。改仓库时只需改这两处。
const GITEE_REPO: &str = "learcat/lithescape";
const GITHUB_REPO: &str = "LEARCAT/lithescape";

fn gitee_latest() -> String {
    format!("https://gitee.com/api/v5/repos/{GITEE_REPO}/releases/latest")
}
fn github_latest() -> String {
    format!("https://api.github.com/repos/{GITHUB_REPO}/releases/latest")
}

/// 解析形如 "v0.1.2" / "0.1.2" 的版本号，返回可比较的三段数组。
/// 无法解析时返回 None，比较逻辑保守判为"不更新"。
fn parse_version(s: &str) -> Option<[u64; 3]> {
    let s = s.trim().trim_start_matches('v').trim_start_matches('V');
    let mut parts = s.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch_str: String = parts
        .next()?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    let patch = patch_str.parse().ok()?;
    Some([major, minor, patch])
}

/// 判断 latest 是否比 current 新。解析失败返回 false（保守）。
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(a), Some(b)) => a.cmp(&b) == Ordering::Greater,
        _ => false,
    }
}

/// 用 curl 拉取 URL；失败返回 None（网络不可达是常态，不视为错误）。
fn http_get(url: &str, timeout_secs: u64) -> Option<String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "10",
            "--max-time",
            &timeout_secs.to_string(),
            "-H",
            "Accept: application/vnd.github+json",
            url,
        ])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8(out.stdout).ok()
}

/// 上游发布信息。
#[derive(Debug, Clone)]
pub struct Release {
    /// 纯版本号，如 0.1.2（去掉 v 前缀）。
    pub version: String,
    /// deb 的匿名下载地址（来自 GitHub assets 或按约定拼接）。
    pub deb_url: Option<String>,
    /// 更新说明。
    pub notes: String,
}

/// 从 GitHub Release JSON 的 assets 里取 .deb 下载地址。
fn json_deb_url(text: &str) -> Option<String> {
    let root: Value = serde_json::from_str(text).ok()?;
    let assets = root.get("assets")?.as_array()?;
    for a in assets {
        let name = a.get("name")?.as_str()?;
        if name.ends_with(".deb") {
            if let Some(url) = a.get("browser_download_url")?.as_str() {
                return Some(url.to_string());
            }
        }
    }
    None
}

/// 探测最新 Release：GitHub 优先（信息更全，带 deb 地址），Gitee 兜底。
pub fn check(current: &str) -> Option<Release> {
    let github = http_get(&github_latest(), 25).and_then(|t| {
        let root: Value = serde_json::from_str(&t).ok()?;
        let tag = root.get("tag_name")?.as_str()?;
        Some(Release {
            version: tag.trim_start_matches('v').to_string(),
            deb_url: json_deb_url(&t),
            notes: root
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    });

    let gitee = http_get(&gitee_latest(), 20).and_then(|t| {
        let root: Value = serde_json::from_str(&t).ok()?;
        let tag = root.get("tag_name")?.as_str()?;
        Some(Release {
            version: tag.trim_start_matches('v').to_string(),
            deb_url: None,
            notes: root
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    });

    let mut release = match (github, gitee) {
        (Some(gh), _) => gh,
        (None, Some(gt)) => gt,
        (None, None) => return None,
    };
    if release.deb_url.is_none() {
        let v = &release.version;
        release.deb_url = Some(format!(
            "https://github.com/{GITHUB_REPO}/releases/download/v{v}/lithescape_{v}_amd64.deb"
        ));
    }
    if is_newer(&release.version, current) {
        Some(release)
    } else {
        None
    }
}

/// 缓存目录：~/.cache/lithescape/。
fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/LEARCAT".to_string());
    let dir = PathBuf::from(home).join(".cache/lithescape");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 下载 deb 到缓存目录，返回本地路径。
pub fn download(release: &Release) -> Result<PathBuf, String> {
    let url = release
        .deb_url
        .as_ref()
        .ok_or_else(|| "该版本没有提供 deb 下载地址".to_string())?;
    let name = format!("lithescape_{}_amd64.deb", release.version);
    let path = cache_dir()
        .ok_or_else(|| "无法创建缓存目录".to_string())?
        .join(name);
    let tmp = path.with_extension("deb.part");

    let status = Command::new("curl")
        .args([
            "-fL",
            "--connect-timeout",
            "10",
            "--max-time",
            "300",
            "-o",
            &tmp.to_string_lossy(),
            url,
        ])
        .status()
        .map_err(|e| format!("调用 curl 下载失败: {e}"))?;
    if !status.success() {
        let _ = fs::remove_file(&tmp);
        return Err("下载失败（网络不可达或下载地址失效）".into());
    }
    let meta = fs::metadata(&tmp).map_err(|e| e.to_string())?;
    if meta.len() < 500_000 {
        let _ = fs::remove_file(&tmp);
        return Err("下载文件过小，可能不是有效的 deb 包".into());
    }
    fs::rename(&tmp, &path).map_err(|e| e.to_string())?;
    Ok(path)
}

/// 安装 deb：依次尝试 sudo -n、pkexec、xdg-open。
/// 返回 (是否成功, 提示文案)。
pub fn install(path: &PathBuf) -> (bool, String) {
    let deb = path.to_string_lossy();

    if Command::new("sudo")
        .args(["-n", "dpkg", "-i", &deb])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (true, "已更新到新版本，重启程序后生效".to_string());
    }

    if Command::new("pkexec")
        .args(["dpkg", "-i", &deb])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (true, "已更新到新版本，重启程序后生效".to_string());
    }

    for opener in ["xdg-open", "deepin-deb-installer"] {
        if Command::new(opener)
            .arg(&*deb)
            .spawn()
            .map(|_| true)
            .unwrap_or(false)
        {
            return (
                true,
                "deb 已下载，已打开系统安装器，请手动完成安装".to_string(),
            );
        }
    }

    (false, "更新包已下载，但无法自动安装，请手动安装".to_string())
}

/// 供菜单点击调用：检查 → 下载 → 安装，全程同步（调用方应放 spawn_blocking）。
pub fn run_update(current: &str) -> String {
    let release = match check(current) {
        Some(r) => r,
        None => return "当前已是最新版本，或暂时无法连接更新服务器".to_string(),
    };
    let path = match download(&release) {
        Ok(p) => p,
        Err(e) => return format!("下载更新失败：{e}"),
    };
    let (ok, msg) = install(&path);
    if !ok {
        return format!("{msg}，位置: {}", path.display());
    }
    let brief = release
        .notes
        .lines()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .chars()
        .take(60)
        .collect::<String>();
    if brief.is_empty() {
        format!("发现新版本 {}：{}", release.version, msg)
    } else {
        format!("发现新版本 {}（{}）：{}", release.version, brief, msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_basic() {
        assert_eq!(parse_version("0.1.2"), Some([0, 1, 2]));
        assert_eq!(parse_version("v0.1.3"), Some([0, 1, 3]));
        assert_eq!(parse_version("V1.2.3-beta"), Some([1, 2, 3]));
        assert_eq!(parse_version("garbage"), None);
    }

    #[test]
    fn is_newer_cases() {
        assert!(is_newer("v0.1.3", "0.1.2"));
        assert!(is_newer("0.2.0", "0.1.99"));
        assert!(!is_newer("0.1.2", "0.1.2"));
        assert!(!is_newer("0.1.1", "0.1.2"));
        assert!(!is_newer("bad", "0.1.2"));
    }

    #[test]
    fn json_deb_url_from_assets() {
        let text = r#"{"assets":[{"name":"source.zip","browser_download_url":"https://x/zip"},{"name":"lithescape_0.1.2_amd64.deb","browser_download_url":"https://x/lithescape_0.1.2_amd64.deb"}]}"#;
        assert_eq!(
            json_deb_url(text).as_deref(),
            Some("https://x/lithescape_0.1.2_amd64.deb")
        );
    }
}

#[cfg(test)]
mod live_check_tests {
    use super::*;

    /// 真实网络探测（可选）：确认两个端点可达且能拼出下载地址。
    #[test]
    #[ignore]
    fn live_release_probe() {
        let r = check("0.0.0").expect("应能探测到线上 Release");
        assert!(!r.version.is_empty());
        assert!(r
            .deb_url
            .as_ref()
            .map(|u| u.ends_with(".deb"))
            .unwrap_or(false));
        eprintln!("live version={} url={}", r.version, r.deb_url.unwrap_or_default());
    }
}
