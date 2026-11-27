//! LitheScape 轻格壁纸：API、下载、壁纸设置

use crate::config::{
    load_history, load_state, save_dir, save_state, Config, HistoryItem, MetaInfo, State,
};
use reqwest::blocking::Client;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

/// 单个外部命令的最长等待时间。DDE 的 DBus 服务在异常时可能永不回复，
/// 若用 Command::output() 无限等待，后台线程会永久卡住并让 busy 无法复位，
/// 表现为「托盘点击没反应」。所有外部命令都必须经此封装。
const CMD_TIMEOUT: Duration = Duration::from_secs(8);

const API_URL: &str = "https://www.bing.com/HPImageArchive.aspx";
const BING_BASE: &str = "https://www.bing.com";
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36";

/// 必应支持的图片尺寸档（从大到小），用于按屏幕宽度自适应选图
const RESOLUTIONS: &[(&str, u32)] = &[
    ("UHD", 2560),
    ("1920x1080", 1600),
    ("1366x768", 1100),
    ("1280x720", 0),
];

fn client() -> Client {
    Client::builder()
        .timeout(Duration::from_secs(30))
        .user_agent(UA)
        .build()
        .unwrap_or_else(|_| Client::new())
}

/// 解析最终使用的分辨率档。
/// auto / 4K / UHD 一律优先 4K（_UHD.jpg 实测 3840x2160），
/// 下载失败时由 download_best_image 逐档回退；用户显式配置较低档时尊重配置。
fn effective_resolution(cfg: &Config) -> String {
    let r = cfg.resolution.trim();
    if r.is_empty()
        || r.eq_ignore_ascii_case("auto")
        || r.eq_ignore_ascii_case("4k")
        || r.eq_ignore_ascii_case("uhd")
    {
        "UHD".to_string()
    } else {
        r.to_string()
    }
}

/// 拉取必应图片元数据；index=0 今天，1..=7 过去几天
pub fn fetch_metadata(cfg: &Config, index: i32) -> Result<(MetaInfo, String), String> {
    let url = format!("{}?format=js&idx={}&n=1&mkt={}", API_URL, index, cfg.market);
    let resp = client()
        .get(&url)
        .send()
        .map_err(|e| format!("请求失败: {e}"))?;
    let json: serde_json::Value = resp.json().map_err(|e| format!("解析失败: {e}"))?;

    let image = json
        .get("images")
        .and_then(|v| v.get(0))
        .ok_or_else(|| "接口未返回图片数据".to_string())?;

    let s = |k: &str| -> String {
        image
            .get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };

    let meta = MetaInfo {
        enddate: s("enddate"),
        title: s("title"),
        copyright: s("copyright"),
        copyrightlink: s("copyrightlink"),
        quiz: s("quiz"),
    };

    let urlbase = s("urlbase");
    let res = effective_resolution(cfg);
    let image_url = image_url_for(&urlbase, &s("url"), &res);

    Ok((meta, image_url))
}

/// 根据 urlbase 和尺寸档拼出直链
fn image_url_for(urlbase: &str, raw_url: &str, res: &str) -> String {
    if urlbase.is_empty() {
        format!("{}{}", BING_BASE, raw_url)
    } else if res.eq_ignore_ascii_case("UHD") || res.eq_ignore_ascii_case("4K") {
        format!("{}{}_UHD.jpg", BING_BASE, urlbase)
    } else {
        format!("{}{}_{}.jpg", BING_BASE, urlbase, res)
    }
}

/// 从直链反推 urlbase（去掉末尾的 _<档位>.jpg）
fn urlbase_from(image_url: &str) -> Option<&str> {
    for (name, _) in RESOLUTIONS {
        let suffix = format!("_{name}.jpg");
        if let Some(base) = image_url.strip_suffix(&suffix) {
            return Some(base);
        }
    }
    None
}

/// 按从高到低生成候选直链；旧历史里存的 1080p 链接也会被升级到 UHD。
fn candidate_urls(image_url: &str) -> Vec<String> {
    match urlbase_from(image_url) {
        Some(base) => RESOLUTIONS
            .iter()
            .map(|(name, _)| format!("{base}_{name}.jpg"))
            .collect(),
        None => vec![image_url.to_string()],
    }
}

/// 依次尝试各分辨率档，返回实际下载成功的直链。
fn download_best_image(candidates: &[String], dest: &Path) -> Result<String, String> {
    let mut last = String::new();
    for url in candidates {
        match download_image_min(url, dest, 20_000) {
            Ok(()) => return Ok(url.clone()),
            Err(e) => last = e,
        }
    }
    Err(format!("所有分辨率均下载失败：{last}"))
}

/// 从实际直链判断分辨率档，用于缓存文件名。
fn resolution_from_url(image_url: &str) -> String {
    for (name, _) in RESOLUTIONS {
        if image_url.ends_with(&format!("_{name}.jpg")) {
            return (*name).to_string();
        }
    }
    "unknown".to_string()
}

/// 批量拉取最近 N 天元数据（一次 API 调用）
pub fn fetch_recent(cfg: &Config, n: i32) -> Result<Vec<(MetaInfo, String)>, String> {
    let url = format!("{}?format=js&idx=0&n={}&mkt={}", API_URL, n, cfg.market);
    let resp = client()
        .get(&url)
        .send()
        .map_err(|e| format!("请求失败: {e}"))?;
    let json: serde_json::Value = resp.json().map_err(|e| format!("解析失败: {e}"))?;
    let images = json
        .get("images")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "接口未返回图片数据".to_string())?;
    let res = effective_resolution(cfg);
    let mut out = Vec::new();
    for image in images {
        let s = |k: &str| -> String {
            image
                .get(k)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        let meta = MetaInfo {
            enddate: s("enddate"),
            title: s("title"),
            copyright: s("copyright"),
            copyrightlink: s("copyrightlink"),
            quiz: s("quiz"),
        };
        let url = image_url_for(&s("urlbase"), &s("url"), &res);
        out.push((meta, url));
    }
    Ok(out)
}

fn slug(url: &str) -> String {
    let last = url.rsplit('=').next().unwrap_or("daily");
    last.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

pub fn image_path(meta: &MetaInfo, image_url: &str) -> PathBuf {
    let date = if meta.enddate.is_empty() {
        "daily".to_string()
    } else {
        meta.enddate.clone()
    };
    let res = resolution_from_url(image_url);
    let name = format!("bing_{}_{}_{}.jpg", date, slug(image_url), res);
    save_dir().join(name)
}

/// 带最小字节数的下载（缩略图阈值更低）
pub fn download_image_min(url: &str, dest: &Path, min_bytes: usize) -> Result<(), String> {
    let resp = client()
        .get(url)
        .header("Referer", "https://www.bing.com/")
        .send()
        .map_err(|e| format!("下载失败: {e}"))?;

    let bytes = resp.bytes().map_err(|e| format!("读取失败: {e}"))?;
    if bytes.len() < min_bytes {
        return Err(format!("图片过小: {} 字节", bytes.len()));
    }
    if bytes.len() < 2 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return Err("下载内容不是 JPEG".into());
    }

    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("创建目录失败: {e}"))?;
    }
    let tmp = dest.with_extension("jpg.part");
    let mut file = fs::File::create(&tmp).map_err(|e| format!("写入失败: {e}"))?;
    file.write_all(&bytes)
        .map_err(|e| format!("写入失败: {e}"))?;
    drop(file);
    fs::rename(&tmp, dest).map_err(|e| format!("保存失败: {e}"))?;
    Ok(())
}

/// 带超时地执行外部命令并捕获输出。
/// 关键点：把子进程放进独立进程组，超时后整组杀掉，避免 dbus-send/gsettings
/// 挂在 DDE 无响应的 DBus 调用上时留下僵尸进程或继续占用资源。
fn cmd_output(argv: &[&str]) -> Option<std::process::Output> {
    use std::os::unix::process::CommandExt;
    use std::sync::mpsc;

    let mut command = Command::new(argv[0]);
    command.args(&argv[1..]);
    // 独立进程组，便于超时后连带子进程一起清理
    command.process_group(0);
    let child = match command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let pid = child.id();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });
    match rx.recv_timeout(CMD_TIMEOUT) {
        Ok(Ok(out)) => Some(out),
        _ => {
            // 杀掉整个进程组（负 pid）
            let _ = Command::new("kill")
                .args(["-9", &format!("-{pid}")])
                .output();
            let _ = Command::new("kill").args(["-9", &pid.to_string()]).output();
            eprintln!("[warn] 命令超时已终止: {} (pid {pid})", argv[0]);
            None
        }
    }
}

fn cmd(argv: &[&str]) -> Option<String> {
    cmd_output(argv).map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

fn gv_quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

pub fn current_background_uris() -> Vec<String> {
    let out = cmd(&[
        "gsettings",
        "get",
        "com.deepin.dde.appearance",
        "background-uris",
    ])
    .unwrap_or_default();
    let inner = out.trim().trim_start_matches('[').trim_end_matches(']');
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 设置壁纸，DBus 优先，gsettings 保底。返回是否成功
pub fn set_wallpaper(path: &Path) -> bool {
    let uri = format!("file://{}", path.display());
    let mut ok = false;

    if let Some(out) = cmd(&[
        "dbus-send",
        "--session",
        "--dest=org.deepin.dde.Appearance1",
        "--type=method_call",
        "--print-reply",
        "/org/deepin/dde/Appearance1",
        "org.deepin.dde.Appearance1.SetCurrentWorkspaceBackground",
        &format!("string:{uri}"),
    ]) {
        if !out.contains("Error") {
            ok = true;
        }
    }

    let value = format!("[{}]", gv_quote(&uri));
    if let Some(o) = cmd_output(&[
        "gsettings",
        "set",
        "com.deepin.dde.appearance",
        "background-uris",
        &value,
    ]) {
        if o.status.success() {
            ok = true;
        }
    }

    // 校验最终生效值，未同步则补写
    if ok && !current_background_uris().iter().any(|u| u == &uri) {
        let _ = cmd(&[
            "gsettings",
            "set",
            "com.deepin.dde.appearance",
            "background-uris",
            &value,
        ]);
    }
    ok
}

/// 执行一次换壁纸，返回展示用的描述文字
pub fn apply(index: i32, force: bool, cfg: &Config) -> Result<String, String> {
    // Bing 接口只给最近 8 天（idx 再大返回重复数据）。翻页到更早的日期时，
    // 从本地历史归档读取；只有「今天」或强制刷新才实时请求接口。
    let (meta, mut image_url) = if force || index == 0 {
        let fetched = fetch_metadata(cfg, index)?;
        // 顺手归档，日积月累形成比 8 天更长的历史
        crate::config::merge_history(vec![HistoryItem {
            meta: fetched.0.clone(),
            image_url: fetched.1.clone(),
        }]);
        fetched
    } else if let Some(h) = load_history().get(index as usize) {
        (h.meta.clone(), h.image_url.clone())
    } else {
        // 归档尚无此日期（首次运行），回退到接口；idx 超过 7 时接口会返回重复数据
        fetch_metadata(cfg, index)?
    };
    let dest = image_path(&meta, &image_url);

    if !dest.exists() || force {
        let candidates = candidate_urls(&image_url);
        let used = download_best_image(&candidates, &dest)?;
        if used != image_url {
            image_url = used;
            // 旧归档里的 1080p 链接升级为实际可用的最高档，下次直接用 4K。
            crate::config::merge_history(vec![HistoryItem {
                meta: meta.clone(),
                image_url: image_url.clone(),
            }]);
        }
    }

    let mut state: State = load_state();
    if state.original_background_uris.is_empty() {
        state.original_background_uris = current_background_uris();
    }
    if !set_wallpaper(&dest) {
        return Err("壁纸设置失败，DDE 接口与 gsettings 均不可用".into());
    }

    state.current_index = index;
    state.last_date = meta.enddate.clone();
    state.last_path = dest.display().to_string();
    state.last_title = meta.title.clone();
    state.last_copyright = meta.copyright.clone();
    state.last_set_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    // index 0 = 每日壁纸；翻页到历史日期则是用户选定壁纸
    state.wallpaper_mode = if index == 0 { "daily" } else { "favorite" }.into();
    state.meta.insert(index.to_string(), meta.clone());
    save_state(&state);

    let _ = state;
    Ok(if meta.copyright.is_empty() {
        meta.title
    } else {
        meta.copyright
    })
}

/// 从预览项直接设置壁纸（元数据已拉取，避免重复请求）
pub fn apply_from_preview(
    meta: &MetaInfo,
    image_url: &str,
    index: i32,
    _cfg: &Config,
) -> Result<String, String> {
    let dest = image_path(meta, image_url);
    if !dest.exists() {
        let candidates = candidate_urls(image_url);
        let _ = download_best_image(&candidates, &dest)?;
    }
    let mut state: State = load_state();
    if state.original_background_uris.is_empty() {
        state.original_background_uris = current_background_uris();
    }
    if !set_wallpaper(&dest) {
        return Err("壁纸设置失败，DDE 接口与 gsettings 均不可用".into());
    }
    state.current_index = index;
    state.last_date = meta.enddate.clone();
    state.last_path = dest.display().to_string();
    state.last_title = meta.title.clone();
    state.last_copyright = meta.copyright.clone();
    state.last_set_at = chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
    // 从画廊选中的壁纸属于用户选定，不再被每日壁纸自动覆盖
    state.wallpaper_mode = "favorite".into();
    state.meta.insert(index.to_string(), meta.clone());
    save_state(&state);

    Ok(if meta.copyright.is_empty() {
        meta.title.clone()
    } else {
        meta.copyright.clone()
    })
}

/// 应用一张收藏壁纸（收藏自动更换用）。
/// 从历史归档里找回这张壁纸的真实序号，让托盘「图片故事 / 上一张 / 下一张」
/// 在收藏轮播状态下依然按真实日期工作；归档已裁剪时用 0 兜底。
pub fn apply_favorite(item: &crate::config::HistoryItem) -> Result<String, String> {
    let index = load_history()
        .iter()
        .position(|h| h.meta.enddate == item.meta.enddate)
        .map(|i| i as i32)
        .unwrap_or(0);
    apply_from_preview(&item.meta, &item.image_url, index, &Config::default())
}

/// 读取系统壁纸轮播配置（DBus 属性 WallpaperSlideShow，形如 {"DP-0&&1":"900"}）
pub fn get_slideshow() -> String {
    cmd(&[
        "dbus-send",
        "--session",
        "--dest=org.deepin.dde.Appearance1",
        "--type=method_call",
        "--print-reply",
        "/org/deepin/dde/Appearance1",
        "org.freedesktop.DBus.Properties.Get",
        "string:org.deepin.dde.Appearance1",
        "string:WallpaperSlideShow",
    ])
    .and_then(|out| {
        // 输出末行形如: variant       string "{"DP-0&&1":"900"}"
        let line = out.lines().last()?;
        let start = line.find('"')? + 1;
        let end = line.rfind('"')?;
        if start <= end {
            Some(line[start..end].to_string())
        } else {
            None
        }
    })
    .unwrap_or_default()
}

/// 禁用 deepin 系统自带的壁纸自动更换（把每个监控的轮播间隔设为 0）
/// 返回原配置 JSON，供将来恢复；已禁用或读取失败时返回空串
pub fn disable_system_slideshow() -> String {
    let raw = get_slideshow();
    if raw.is_empty() || raw == "{}" {
        return String::new();
    }
    let map: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(&raw) {
        Ok(m) => m,
        Err(_) => return String::new(),
    };
    if map.is_empty() {
        return String::new();
    }
    // 若所有监控都已是 0（已禁用），说明系统轮播本来就没在跑，
    // 不返回配置，避免把禁用状态误当"原值"覆盖掉真正的恢复点
    if map.values().all(|v| v.as_str() == Some("0")) {
        return String::new();
    }
    for (monitor, interval) in &map {
        // 已经是 0 的跳过
        if interval.as_str() == Some("0") {
            continue;
        }
        // DBus 方法期望裸监控名（如 DP-0），它自己会补 &&<工作区> 后缀
        let bare = monitor.split("&&").next().unwrap_or(monitor);
        let _ = cmd(&[
            "dbus-send",
            "--session",
            "--dest=org.deepin.dde.Appearance1",
            "--type=method_call",
            "--print-reply",
            "/org/deepin/dde/Appearance1",
            "org.deepin.dde.Appearance1.SetWallpaperSlideShow",
            &format!("string:{}", bare),
            "string:0",
        ]);
    }
    raw
}

/// 恢复系统壁纸轮播配置
#[allow(dead_code)]
pub fn restore_slideshow(raw: &str) -> bool {
    if raw.is_empty() {
        return false;
    }
    let map: serde_json::Map<String, serde_json::Value> = match serde_json::from_str(raw) {
        Ok(m) => m,
        Err(_) => return false,
    };
    let mut ok = true;
    for (monitor, interval) in map {
        let secs = interval.as_str().unwrap_or("0");
        let bare = monitor.split("&&").next().unwrap_or(&monitor);
        let succeeded = cmd_output(&[
            "dbus-send",
            "--session",
            "--dest=org.deepin.dde.Appearance1",
            "--type=method_call",
            "--print-reply",
            "/org/deepin/dde/Appearance1",
            "org.deepin.dde.Appearance1.SetWallpaperSlideShow",
            &format!("string:{}", bare),
            &format!("string:{}", secs),
        ])
        .map(|o| o.status.success())
        .unwrap_or(false);
        if !succeeded {
            ok = false;
        }
    }
    ok
}

/// 恢复安装前壁纸（保留接口，供后续菜单项使用）
#[allow(dead_code)]
pub fn restore() -> Result<String, String> {
    let state: State = load_state();
    let uris = state.original_background_uris;
    if uris.is_empty() {
        return Err("没有记录安装前的壁纸".into());
    }
    let value = format!(
        "[{}]",
        uris.iter()
            .map(|u| gv_quote(u))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let out = cmd_output(&[
        "gsettings",
        "set",
        "com.deepin.dde.appearance",
        "background-uris",
        &value,
    ])
    .ok_or_else(|| "恢复失败：命令超时或执行失败".to_string())?;
    if out.status.success() {
        Ok(uris.join(", "))
    } else {
        Err("恢复失败".into())
    }
}

pub fn open_url(url: &str) {
    if url.is_empty() {
        return;
    }
    let _ = Command::new("xdg-open").arg(url).spawn();
}

pub fn notify(title: &str, body: &str) {
    let _ = Command::new("notify-send")
        .args(["-a", "轻格壁纸", "-i", "lithescape", title, body])
        .spawn();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn auto_prefers_4k() {
        let cfg = Config {
            resolution: "auto".into(),
            ..Config::default()
        };
        assert_eq!(effective_resolution(&cfg), "UHD");
        let cfg4k = Config {
            resolution: "4K".into(),
            ..Config::default()
        };
        assert_eq!(effective_resolution(&cfg4k), "UHD");
        let cfg1080 = Config {
            resolution: "1920x1080".into(),
            ..Config::default()
        };
        assert_eq!(effective_resolution(&cfg1080), "1920x1080");
    }

    #[test]
    fn uhd_url_and_fallback_order() {
        let url = "https://www.bing.com/th?id=OHR.Test_UHD.jpg";
        let urls = candidate_urls(url);
        assert_eq!(urls.len(), RESOLUTIONS.len());
        assert!(urls[0].ends_with("_UHD.jpg"));
        assert!(urls[1].ends_with("_1920x1080.jpg"));
        assert_eq!(urlbase_from(url), Some("https://www.bing.com/th?id=OHR.Test"));
    }

    #[test]
    fn old_1080p_history_upgrades_to_uhd() {
        let url = "https://www.bing.com/th?id=OHR.Test_1920x1080.jpg";
        let urls = candidate_urls(url);
        assert!(urls[0].ends_with("_UHD.jpg"));
        assert_eq!(resolution_from_url(&urls[0]), "UHD");
    }
}
