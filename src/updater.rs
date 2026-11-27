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
    /// deb 的匿名下载地址（优先 Gitee assets，缺失时 GitHub 补齐）。
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

/// 探测最新 Release：对齐 LitheZones —— Gitee 为主（版本 + deb 地址），
/// GitHub 只作双保险；两侧都取不到 deb 时按约定拼 Gitee 标准下载路径。
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
            deb_url: json_deb_url(&t),
            notes: root
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
        })
    });

    let mut release = match (gitee, github) {
        (Some(gt), gh) => {
            let mut r = gt;
            if r.deb_url.is_none() {
                r.deb_url = gh.and_then(|g| g.deb_url);
            }
            r
        }
        (None, Some(gh)) => gh,
        (None, None) => return None,
    };
    if release.deb_url.is_none() {
        let v = &release.version;
        release.deb_url = Some(format!(
            "https://gitee.com/{GITEE_REPO}/releases/download/v{v}/lithescape_{v}_amd64.deb"
        ));
    }
    if is_newer(&release.version, current) {
        Some(release)
    } else {
        None
    }
}

/// 启动时静默检查一次：只有确实发现新版本才返回提示文案。
/// 已是最新、网络不可达、版本解析失败一律返回 None，不打扰用户。
pub fn startup_hint(current: &str) -> Option<String> {
    let release = check(current)?;
    Some(format_update_hint(&release))
}

/// 从 Release 说明里摘一条简短变更摘要，供通知复用。
fn release_brief(release: &Release) -> String {
    // 优先取更新说明里的第一条变更项，避免把标题行重复念一遍。
    let lines: Vec<&str> = release
        .notes
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    lines
        .iter()
        .copied()
        .find(|l| l.starts_with('-') || l.starts_with('*'))
        .or_else(|| lines.iter().copied().find(|l| !l.contains("LitheScape")))
        .map(|l| {
            l.trim_start_matches(|c| c == '-' || c == '*')
                .trim()
                .trim_end_matches(|c: char| matches!(c, '。' | '；' | '！' | '.' | ';' | '!'))
        })
        .unwrap_or("")
        .chars()
        .take(60)
        .collect()
}

/// 把 Release 信息拼成一条系统通知文案（供启动检查与菜单更新共用风格）。
fn format_update_hint(release: &Release) -> String {
    let brief = release_brief(release);
    if brief.is_empty() {
        format!(
            "发现新版本 {}。点击托盘菜单「软件更新」下载安装。",
            release.version
        )
    } else {
        format!(
            "发现新版本 {}：{}。点击托盘菜单「软件更新」下载安装。",
            release.version, brief
        )
    }
}

/// 缓存目录：~/.cache/lithescape/。
fn cache_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/home/LEARCAT".to_string());
    let dir = PathBuf::from(home).join(".cache/lithescape");
    fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// 从 curl `-#` 进度输出里解析百分比。
/// 进度行形如 "#####          52.6%"，以 \r 刷新；取出现的最后一个百分比。
fn parse_curl_percent(chunk: &str) -> Option<u32> {
    let mut last = None;
    for token in chunk.split_whitespace() {
        if let Some(num) = token.strip_suffix('%') {
            if let Ok(v) = num.parse::<f32>() {
                last = Some(v.round() as u32);
            }
        }
    }
    last
}

/// 下载 deb 到缓存目录，返回本地路径。
/// progress: 可选进度回调（0-100），用于通知栏原地更新下载进度。
pub fn download(release: &Release, mut progress: Option<&mut dyn FnMut(u32)>) -> Result<PathBuf, String> {
    let url = release
        .deb_url
        .as_ref()
        .ok_or_else(|| "该版本没有提供 deb 下载地址".to_string())?;
    let name = format!("lithescape_{}_amd64.deb", release.version);
    let path = cache_dir()
        .ok_or_else(|| "无法创建缓存目录".to_string())?
        .join(name);
    let tmp = path.with_extension("deb.part");

    // 主地址失败时回退到 GitHub 标准地址；两端都失败才报错。
    let fallback = format!(
        "https://github.com/{GITHUB_REPO}/releases/download/v{}/lithescape_{}_amd64.deb",
        release.version, release.version
    );
    let mut ok = false;
    let candidates: Vec<&str> = if fallback == *url {
        vec![url.as_str()]
    } else {
        vec![url.as_str(), fallback.as_str()]
    };
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let mut child = Command::new("curl")
            .args([
                "-fL",
                "-#",
                "--connect-timeout",
                "10",
                "--max-time",
                "300",
                "-o",
                &tmp.to_string_lossy(),
                candidate,
            ])
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("调用 curl 下载失败: {e}"))?;

        // 读 stderr 进度（curl 用 \r 刷新），解析百分比并节流回调
        let has_progress = progress.is_some();
        if has_progress {
            if let Some(stderr) = child.stderr.as_mut() {
                use std::io::Read;
                let mut buf = [0u8; 4096];
                let mut acc = String::new();
                let mut last_report: Option<u32> = None;
                loop {
                    match stderr.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            acc.push_str(&String::from_utf8_lossy(&buf[..n]));
                            if let Some(pct) = parse_curl_percent(&acc) {
                                // 只在整数百分比变化时回调，避免刷屏
                                if last_report != Some(pct) {
                                    last_report = Some(pct);
                                    if let Some(cb) = progress.as_mut() {
                                        cb(pct);
                                    }
                                }
                            }
                            // 控制累加串长度，只保留尾部片段
                            if acc.len() > 8192 {
                                acc = acc
                                    .chars()
                                    .rev()
                                    .take(1024)
                                    .collect::<String>()
                                    .chars()
                                    .rev()
                                    .collect();
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        }
        let status = child.wait().map_err(|e| format!("等待 curl 结束失败: {e}"))?;
        if status.success() {
            ok = true;
            break;
        }
        let _ = fs::remove_file(&tmp);
    }
    if !ok {
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

/// 安装结果。只有 dpkg 真正把新二进制装上才需要自动拉起新版；
/// 交给系统安装器时 deb 尚未落地，重启只会启动旧版本，必须交给用户手动完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallOutcome {
    Installed,
    OpenedInstaller,
    Failed,
}

/// 安装 deb：依次尝试 sudo -n、pkexec、xdg-open。
/// 本函数只在独立的 --apply-update 助手进程里调用，旧进程此时已退出。
pub fn install(path: &PathBuf) -> (InstallOutcome, String) {
    let deb = path.to_string_lossy();

    if Command::new("sudo")
        .args(["-n", "dpkg", "-i", &deb])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (
            InstallOutcome::Installed,
            "已更新到新版本，即将自动启动".to_string(),
        );
    }

    if Command::new("pkexec")
        .args(["dpkg", "-i", &deb])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        return (
            InstallOutcome::Installed,
            "已更新到新版本，即将自动启动".to_string(),
        );
    }

    for opener in ["xdg-open", "deepin-deb-installer"] {
        if Command::new(opener)
            .arg(&*deb)
            .spawn()
            .map(|_| true)
            .unwrap_or(false)
        {
            return (
                InstallOutcome::OpenedInstaller,
                "deb 已下载，已打开系统安装器，安装完成后将自动启动轻格壁纸".to_string(),
            );
        }
    }

    (
        InstallOutcome::Failed,
        "更新包已下载，但无法自动安装，请手动安装".to_string(),
    )
}

/// 供托盘菜单调用：只检查 + 下载，不安装。
/// 返回 (通知文案, 下载好的 deb 路径)。主循环收到路径后先退出旧版，
/// 再由退出清理段拉起 --apply-update 助手，避免"旧版运行中就替换二进制"。
/// progress: 下载进度回调（0-100），供通知栏原地更新。
pub fn prepare_update(
    current: &str,
    progress: Option<&mut dyn FnMut(u32)>,
) -> (String, Option<PathBuf>) {
    let release = match check(current) {
        Some(r) => r,
        None => {
            return (
                "当前已是最新版本，或暂时无法连接更新服务器".to_string(),
                None,
            )
        }
    };
    match download(&release, progress) {
        Ok(p) => (
            format!("已下载新版本 {}，即将退出旧版并安装", release.version),
            Some(p),
        ),
        Err(e) => (format!("下载更新失败：{e}"), None),
    }
}

/// 读取系统已安装版本，供「等安装器装完再启动」的助手判断。
pub fn installed_version() -> Option<String> {
    let out = Command::new("dpkg-query")
        .args(["-W", "-f=${Version}", "lithescape"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}

/// 供 --apply-update 助手进程调用：安装已下载的 deb。
/// 返回 (通知文案, 安装结果)，由调用方决定立即启动还是等待安装器。
pub fn apply_downloaded(path: &PathBuf) -> (String, InstallOutcome) {
    let (outcome, msg) = install(path);
    let text = match outcome {
        InstallOutcome::Installed => {
            // dpkg 已读完 deb 并完成安装，缓存里的安装包不再需要，装完即清。
            // 只删本次这一份，历史遗留的旧包不在此动。
            if let Err(e) = fs::remove_file(path) {
                eprintln!("[warn] 清理更新缓存失败: {e}");
            }
            format!("更新完成：{msg}")
        }
        // 系统安装器仍在读取该 deb，不能删；安装失败时保留供用户手动装。
        // OpenedInstaller 的清理由 --wait-and-start 助手在装完后负责。
        InstallOutcome::OpenedInstaller | InstallOutcome::Failed => {
            format!("{msg}，位置: {}", path.display())
        }
    };
    (text, outcome)
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

    #[test]
    fn gitee_deb_url_from_assets() {
        let text = r#"{"assets":[{"name":"lithescape_0.1.4_amd64.deb","browser_download_url":"https://gitee.com/learcat/lithescape/releases/download/v0.1.4/lithescape_0.1.4_amd64.deb"},{"name":"v0.1.4.zip","browser_download_url":"https://gitee.com/x.zip"}]}"#;
        assert_eq!(
            json_deb_url(text).as_deref(),
            Some("https://gitee.com/learcat/lithescape/releases/download/v0.1.4/lithescape_0.1.4_amd64.deb")
        );
    }

    #[test]
    fn curl_percent_parsing() {
        // curl -# 用 \r 刷新，形如 "#####     52.6%\r#########  100.0%"
        assert_eq!(parse_curl_percent("###          52.6%"), Some(53));
        assert_eq!(parse_curl_percent("\r#########  100.0%"), Some(100));
        assert_eq!(parse_curl_percent("#=#=#"), None);
        // 取最后一个百分比
        assert_eq!(parse_curl_percent("  20.0%\r  87.3%"), Some(87));
        assert_eq!(parse_curl_percent(""), None);
    }

    #[test]
    fn update_hint_mentions_menu() {
        let r = Release {
            version: "0.1.3".into(),
            deb_url: None,
            notes: "轻格壁纸 LitheScape 0.1.3\n- 启动时检查更新。".into(),
        };
        let hint = format_update_hint(&r);
        assert!(hint.contains("0.1.3"));
        assert!(hint.contains("「软件更新」"));
        assert!(hint.contains("启动时检查更新"));
        assert!(!hint.contains("。。"));
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
