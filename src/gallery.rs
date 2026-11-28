//! 大图画廊：本地回环 HTTP 服务 + 内嵌 HTML/JS
//!
//! 参考必应壁纸官方的交互：一张大图 + 左右箭头 / 键盘切换 + 缩略图导航。
//! 只监听 127.0.0.1，零外部依赖，浏览器由 xdg-open 调起。

use crate::bing;
use crate::config::{cache_dir, home_dir};
use crate::tray::{PreviewItem, Shared};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

/// 默认浏览器与屏幕几何缓存：这些查询要走 xdg-settings/xdg-mime/xdotool 外部进程，
/// 每次点击画廊都重跑会拖慢弹出，缓存后只在首次计算一次。
static BROWSER_CACHE: OnceLock<Option<String>> = OnceLock::new();
static GEOMETRY_CACHE: OnceLock<(i32, i32, i32, i32)> = OnceLock::new();

/// 画廊窗口正在启动。窗口出现前 find_gallery_windows 还查不到，
/// 用这个标志挡住连续点击，避免拉起多个浏览器窗口。
static GALLERY_STARTING: AtomicBool = AtomicBool::new(false);

/// 画廊大图缓存路径（按日期，避免重复下载）
fn gallery_path(meta: &crate::config::MetaInfo) -> PathBuf {
    let date = if meta.enddate.is_empty() {
        "daily"
    } else {
        &meta.enddate
    };
    cache_dir().join(format!("gallery_{}_1920x1080.jpg", date))
}

/// 从大图直链提取 id，拼出 1920x1080 画廊图
fn gallery_url(image_url: &str) -> String {
    if let Some(pos) = image_url.rfind('_') {
        format!("{}_1920x1080.jpg", &image_url[..pos])
    } else {
        image_url.to_string()
    }
}

/// 确保画廊大图已下载（1920x1080，兼顾清晰度与加载速度）
pub fn ensure_large(item: &PreviewItem) -> Result<PathBuf, String> {
    let dest = gallery_path(&item.meta);
    if dest.exists() {
        return Ok(dest);
    }
    bing::download_image_min(&gallery_url(&item.image_url), &dest, 20_000)?;
    Ok(dest)
}

/// 本地回环服务：绑定 127.0.0.1 随机端口，在独立线程运行
pub fn start(shared: Shared) -> std::io::Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if let Ok(mut s) = stream {
                let shared = shared.clone();
                std::thread::spawn(move || handle(&mut s, &shared));
            }
        }
    });
    Ok(port)
}

/// 带缓存的默认浏览器查询（首次走 xdg 外部命令，之后直接命中缓存）
fn cached_browser_cmd() -> Option<String> {
    BROWSER_CACHE
        .get_or_init(default_browser_cmd)
        .clone()
}

/// 从 XDG 默认浏览器 association 解析出实际可执行文件路径。
/// 优先 xdg-settings，其次 xdg-mime，最后读对应 .desktop 的 Exec=。
fn default_browser_cmd() -> Option<String> {
    use std::process::Command;

    let xdg_desktop = |cmd: &str, args: &[&str]| -> Option<String> {
        let out = Command::new(cmd).args(args).output().ok()?;
        if !out.status.success() {
            return None;
        }
        let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if s.ends_with(".desktop") {
            Some(s)
        } else {
            None
        }
    };
    let desktop = xdg_desktop("xdg-settings", &["get", "default-web-browser"])
        .or_else(|| xdg_desktop("xdg-mime", &["query", "default", "x-scheme-handler/http"]))?;

    let mut dirs = vec![home_dir().join(".local/share/applications")];
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        dirs.push(Path::new(&xdg).join("applications"));
    }
    dirs.push(PathBuf::from("/usr/share/applications"));
    dirs.push(PathBuf::from("/usr/local/share/applications"));
    for dir in dirs {
        let path = dir.join(&desktop);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(_) => continue,
        };
        for line in text.lines() {
            if let Some(exec) = line.strip_prefix("Exec=") {
                // 取第一个 token 即可；%U/%u 等字段码由我们自己的 URL 参数替代
                if let Some(bin) = exec.split_whitespace().next() {
                    return Some(bin.to_string());
                }
            }
        }
    }
    None
}

/// Chromium 系浏览器支持 --app 无地址栏独立窗口；其它浏览器走 xdg-open。
fn supports_app_mode(bin: &str) -> bool {
    let name = Path::new(bin)
        .file_name()
        .map(|s| s.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    name.contains("chrome")
        || name.contains("chromium")
        || name.contains("edge")
        || name.contains("brave")
}

/// 画廊初始窗口尺寸
const WIN_W: i32 = 620;
const WIN_H: i32 = 520;

/// 右下角目标几何，给底部 dock 留出空间（首次查询后缓存，避免每次点画廊都起 xdotool）
fn gallery_geometry() -> (i32, i32, i32, i32) {
    *GEOMETRY_CACHE.get_or_init(|| {
        let out = std::process::Command::new("xdotool")
            .arg("getdisplaygeometry")
            .output()
            .ok();
        let (sw, sh) = out
            .and_then(|o| {
                let s = String::from_utf8_lossy(&o.stdout);
                let mut it = s.split_whitespace();
                Some((it.next()?.parse::<i32>().ok()?, it.next()?.parse::<i32>().ok()?))
            })
            .unwrap_or((1920, 1080));
        ((sw - WIN_W - 16).max(0), (sh - WIN_H - 72).max(0), WIN_W, WIN_H)
    })
}

/// 专用浏览器配置目录：不继承默认配置里被记住的最大化状态，
/// 让 --window-size / --window-position 在窗口创建时直接生效。
fn browser_profile() -> PathBuf {
    home_dir().join(".cache/lithescape/gallery-profile")
}

/// 在浏览器打开画廊。favorites=true 时默认进入「我的收藏」标签。
/// Chromium 系浏览器用 --app 开无地址栏窗口，启动参数直接给出右下角几何，
/// 避免“先弹出再缩小”；其它浏览器回退 xdg-open。
pub fn open(port: u16, favorites: bool) {
    use std::process::Stdio;
    use std::time::Duration;

    // 已有画廊窗口：只唤起一个，关掉可能残留的其它窗口，不再新开
    let existing = find_gallery_windows();
    if !existing.is_empty() {
        let keep = &existing[0];
        let _ = std::process::Command::new("wmctrl")
            .args(["-i", "-a", keep])
            .output();
        for id in existing.iter().skip(1) {
            let _ = std::process::Command::new("wmctrl")
                .args(["-i", "-c", id])
                .output();
        }
        return;
    }
    // 窗口还没建立但已在启动中：忽略本次点击，避免连开多个
    if GALLERY_STARTING.load(Ordering::Acquire) {
        return;
    }
    GALLERY_STARTING.store(true, Ordering::Release);

    let path = if favorites { "/?view=fav" } else { "/" };
    let url = format!("http://127.0.0.1:{}{path}", port);
    let launched = if let Some(browser) = cached_browser_cmd().filter(|b| supports_app_mode(b)) {
        let (x, y, w, h) = gallery_geometry();
        let profile = browser_profile();
        let _ = std::fs::create_dir_all(&profile);
        let args = vec![
            format!("--user-data-dir={}", profile.display()),
            "--no-first-run".to_string(),
            "--disable-session-crashed-bubble".to_string(),
            "--new-window".to_string(),
            format!("--app={url}"),
            format!("--window-size={w},{h}"),
            format!("--window-position={x},{y}"),
        ];
        let _ = std::process::Command::new(browser)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        // 兜底：个别浏览器忽略启动几何时才会在窗口出现后校正一次；
        // Edge 用专用配置时参数已生效，这一步是同位置同尺寸的空操作。
        std::thread::spawn(move || {
            for _ in 0..30 {
                std::thread::sleep(Duration::from_millis(200));
                if let Some(id) = find_gallery_window() {
                    place_window(&id, x, y, w, h);
                    // 失焦即关闭：点击画廊窗口外任意区域自动收起
                    let watched = id.clone();
                    std::thread::spawn(move || watch_gallery_focus(watched));
                    break;
                }
            }
            GALLERY_STARTING.store(false, Ordering::Release);
        });
        true
    } else {
        // 非 Chromium 浏览器走 xdg-open，无法追踪窗口，直接放行
        bing::open_url(&url);
        false
    };
    if !launched {
        GALLERY_STARTING.store(false, Ordering::Release);
    }
}

/// 从 `wmctrl -lG` 输出里找所有画廊窗口 id（可能有历史残留的多个）
fn find_gallery_windows() -> Vec<String> {
    let out = match std::process::Command::new("wmctrl").arg("-lG").output() {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .filter(|line| line.contains("LitheScape 壁纸画廊"))
        .filter_map(|line| line.split_whitespace().next().map(|s| s.to_string()))
        .collect()
}

fn find_gallery_window() -> Option<String> {
    find_gallery_windows().into_iter().next()
}

/// 当前活动窗口的数值 ID（xdotool 给十进制，统一成 u64 与 wmctrl 的十六进制比较）
fn active_window_id() -> Option<u64> {
    let out = std::process::Command::new("xdotool")
        .arg("getactivewindow")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<u64>().ok()
}

fn parse_window_id(s: &str) -> Option<u64> {
    let hex = s.trim().trim_start_matches("0x");
    u64::from_str_radix(hex, 16).ok()
}

/// 画廊窗口获得焦点后，持续检测活动窗口；一旦焦点离开（点击窗口外）就关闭画廊。
/// 120ms 失焦宽限：足够滤掉焦点抖动又能让点击窗口外时快速收起。
fn watch_gallery_focus(id: String) {
    use std::time::{Duration, Instant};
    let target = match parse_window_id(&id) {
        Some(v) => v,
        None => return,
    };
    let tick = Duration::from_millis(60);
    let grace = Duration::from_millis(120);

    // 等待窗口真正拿到焦点（最多约 8 秒），期间窗口消失则退出
    let mut focused = false;
    for _ in 0..40 {
        if !find_gallery_windows()
            .iter()
            .any(|w| parse_window_id(w) == Some(target))
        {
            return;
        }
        if active_window_id() == Some(target) {
            focused = true;
            break;
        }
        std::thread::sleep(tick);
    }
    if !focused {
        return;
    }

    // 已聚焦：失焦超过宽限时间即关闭，回到焦点则重置计时
    let mut lost_since: Option<Instant> = None;
    loop {
        std::thread::sleep(tick);
        if !find_gallery_windows()
            .iter()
            .any(|w| parse_window_id(w) == Some(target))
        {
            return; // 窗口已被用户或其它逻辑关闭
        }
        if active_window_id() == Some(target) {
            lost_since = None;
            continue;
        }
        let since = *lost_since.get_or_insert_with(Instant::now);
        if since.elapsed() >= grace {
            let _ = std::process::Command::new("wmctrl")
                .args(["-i", "-c", &id])
                .output();
            return;
        }
    }
}

/// 兜底校正窗口几何（正常启动时参数已生效，这里通常是空操作）
fn place_window(id: &str, x: i32, y: i32, w: i32, h: i32) {
    use std::process::Command;
    // 若浏览器忽略了启动参数并继承最大化状态，wmctrl -e 对最大化窗口无效，
    // 先取消最大化再设定几何。
    let _ = Command::new("wmctrl")
        .args(["-i", "-r", id, "-b", "remove,maximized_vert,maximized_horz"])
        .output();
    let geo = format!("0,{x},{y},{w},{h}");
    let _ = Command::new("wmctrl")
        .args(["-i", "-r", id, "-e", &geo])
        .output();
}

/// 实时从本地归档构造画廊列表（按日期倒序，index 0 最新）。
/// 归档为空时回退到内存快照，保证首次启动也能显示。
fn current_items(shared: &Shared) -> Vec<PreviewItem> {
    let history = crate::config::load_history();
    if !history.is_empty() {
        return history
            .into_iter()
            .enumerate()
            .map(|(i, h)| PreviewItem {
                index: i as i32,
                title: h.meta.title.clone(),
                date: h.meta.enddate.clone(),
                image_url: h.image_url,
                meta: h.meta,
            })
            .collect();
    }
    shared
        .previews
        .lock()
        .map(|p| p.clone())
        .unwrap_or_default()
}

fn handle(stream: &mut TcpStream, shared: &Shared) {
    let mut buf = [0u8; 2048];
    let n = match stream.read(&mut buf) {
        Ok(n) => n,
        Err(_) => return,
    };
    let req = String::from_utf8_lossy(&buf[..n]);
    let raw_path = req.split_whitespace().nth(1).unwrap_or("/");
    // 浏览器会给图片 URL 加 ?t=时间戳 绕过缓存，必须先剥掉查询串再解析索引
    let path = raw_path.split('?').next().unwrap_or(raw_path);
    let query = raw_path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let fav_view = query.contains("view=fav");

    if path == "/" || path.starts_with("/index.html") {
        let html = page_html(shared, fav_view);
        respond(stream, 200, "text/html; charset=utf-8", html.as_bytes());
    } else if let Some(rest) = path.strip_prefix("/img/") {
        // /img/<index>.jpg
        let idx: i32 = rest.trim_end_matches(".jpg").parse().unwrap_or(0);
        let previews = current_items(shared);
        if let Some(item) = previews.iter().find(|p| p.index == idx) {
            let file = gallery_path(&item.meta);
            if let Ok(bytes) = std::fs::read(&file) {
                respond(stream, 200, "image/jpeg", &bytes);
                return;
            }
        }
        respond(stream, 404, "image/jpeg", b"");
    } else if let Some(rest) = path.strip_prefix("/set/") {
        // /set/<index> —— 设为壁纸
        // 默认发桌面通知（托盘入口沿用）；画廊「切到哪张自动用哪张」带 quiet=1，
        // 避免连续切换时通知中心堆积，改由画廊内 toast 反馈。
        let idx: i32 = rest.parse().unwrap_or(0);
        let quiet = query_param(&query, "quiet").as_deref() == Some("1");
        let (ok, msg) = set_wallpaper(shared, idx);
        if !quiet {
            if ok {
                bing::notify("壁纸已更新", &msg);
            } else {
                bing::notify("设置失败", &msg);
            }
        }
        let json = serde_json::json!({ "ok": ok, "msg": msg }).to_string();
        respond(
            stream,
            200,
            "application/json; charset=utf-8",
            json.as_bytes(),
        );
    } else if let Some(rest) = path.strip_prefix("/fav/") {
        // /fav/<index> —— 收藏 / 取消收藏，返回切换后的状态
        let idx: i32 = rest.parse().unwrap_or(0);
        let (ok, fav) = toggle_favorite(shared, idx);
        let json = serde_json::json!({ "ok": ok, "fav": fav }).to_string();
        respond(
            stream,
            200,
            "application/json; charset=utf-8",
            json.as_bytes(),
        );
    } else if path == "/favcfg" {
        // 读取收藏自动更换设置
        let c = crate::config::load_fav_slideshow();
        let json = serde_json::json!({
            "enabled": c.enabled,
            "interval": c.interval,
            "unit": c.unit,
        })
        .to_string();
        respond(
            stream,
            200,
            "application/json; charset=utf-8",
            json.as_bytes(),
        );
    } else if path == "/favcfg/set" {
        // 保存收藏自动更换设置；首次开启时立即切到一张收藏壁纸，
        // 让程序进入「收藏壁纸」状态，之后轮播线程才会接管。
        let mut c = crate::config::load_fav_slideshow();
        let was_enabled = c.enabled;
        if let Some(v) = query_param(&query, "enabled") {
            c.enabled = v == "1" || v.eq_ignore_ascii_case("true");
        }
        if let Some(v) = query_param(&query, "interval") {
            if let Ok(n) = v.parse::<u64>() {
                c.interval = n;
            }
        }
        if let Some(v) = query_param(&query, "unit") {
            if matches!(v.as_str(), "min" | "hour" | "day") {
                c.unit = v;
            }
        }
        crate::config::save_fav_slideshow(&c);
        if c.enabled && !was_enabled {
            crate::tray::start_favorite_rotation();
        }
        let json = serde_json::json!({
            "ok": true,
            "enabled": c.enabled,
            "interval": c.interval,
            "unit": c.unit,
        })
        .to_string();
        respond(
            stream,
            200,
            "application/json; charset=utf-8",
            json.as_bytes(),
        );
    } else if path == "/favicon.ico" || path == "/favicon.png" {
        // 网页标签图标用软件自身图标（内嵌 32x32 PNG，不依赖安装路径）
        const FAVICON: &[u8] =
            include_bytes!("../deb/usr/share/icons/hicolor/32x32/apps/lithescape.png");
        respond(stream, 200, "image/png", FAVICON);
    } else {
        respond(stream, 404, "text/plain", b"not found");
    }
}

fn set_wallpaper(shared: &Shared, idx: i32) -> (bool, String) {
    if shared.is_busy() {
        return (false, "正在更换壁纸，请稍候".into());
    }
    let previews = current_items(shared);
    let item = match previews.iter().find(|p| p.index == idx) {
        Some(i) => i.clone(),
        None => return (false, "图片数据尚未加载".into()),
    };
    if !shared.begin_busy() {
        return (false, "正在更换壁纸，请稍候".into());
    }
    let _guard = shared.busy_guard();
    let result = bing::apply_from_preview(&item.meta, &item.image_url, idx);
    match result {
        Ok(msg) => (true, msg),
        Err(e) => (false, e),
    }
}

/// 切换某张壁纸的收藏状态，返回 (是否成功, 切换后是否已收藏)
fn toggle_favorite(shared: &Shared, idx: i32) -> (bool, bool) {
    let previews = current_items(shared);
    let item = match previews.iter().find(|p| p.index == idx) {
        Some(i) => i,
        None => return (false, false),
    };
    let hist = crate::config::HistoryItem {
        meta: item.meta.clone(),
        image_url: item.image_url.clone(),
    };
    let fav = crate::config::toggle_favorite(hist);
    (true, fav)
}

/// 取查询串参数（值均为简单 ASCII，无需 URL 解码）
fn query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        if k == key {
            Some(v.to_string())
        } else {
            None
        }
    })
}

fn respond(stream: &mut TcpStream, status: u16, content_type: &str, body: &[u8]) {
    let reason = if status == 200 { "OK" } else { "Not Found" };
    let header = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        status,
        reason,
        content_type,
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn escape_json(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// 读取 deepin 系统主题：gtk-theme 含 dark 即为深色。
/// 不依赖浏览器的 prefers-color-scheme（deepin 下 Edge 未必跟随），
/// 由服务端直接注入主题，保证「跟随系统」真实生效。
fn system_dark() -> bool {
    use std::process::Command;
    Command::new("gsettings")
        .args(["get", "com.deepin.dde.appearance", "gtk-theme"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("dark"))
        .unwrap_or(false)
}

/// 生成画廊 HTML：数据内嵌为 JS 对象，图片走本地服务 /img/
/// fav_view=true 时默认进入「我的收藏」标签。
fn page_html(shared: &Shared, fav_view: bool) -> String {
    let previews = current_items(shared);
    // 已收藏日期集合：按 enddate 判定，历史归档更新也不会丢收藏状态
    let favorites = crate::config::load_favorites();
    let fav_dates: std::collections::HashSet<&str> = favorites
        .iter()
        .map(|h| h.meta.enddate.as_str())
        .collect();
    // 画廊按「更早 → 今天」排列（与托盘「上一张=更早、下一张=更新」一致）：
    // previews 本身是今天在前、越往后越早，这里反转成最旧在前、今天在末尾。
    // 注意只反转展示顺序，index 仍是壁纸的真实序号，/img //set //fav 按 index 查找不受影响。
    let mut items = Vec::new();
    for item in previews.iter().rev() {
        let story = if item.meta.copyrightlink.is_empty() {
            String::new()
        } else {
            item.meta.copyrightlink.clone()
        };
        let fav = fav_dates.contains(item.date.as_str());
        items.push(format!(
            "{{\"i\":{},\"t\":{},\"d\":{},\"c\":{},\"s\":{},\"f\":{}}}",
            item.index,
            escape_json(&item.title),
            escape_json(&item.date),
            escape_json(&item.meta.copyright),
            escape_json(&story),
            if fav { "true" } else { "false" }
        ));
    }
    let data = format!("[{}]", items.join(","));
    let start_tab = if fav_view { "fav" } else { "all" };
    let theme = if system_dark() { "dark" } else { "light" };

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN" data-theme="{theme}">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="icon" type="image/png" href="/favicon.png">
<title>LitheScape 壁纸画廊</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
:root{{
  --bg:#eef1f4; --surface:#fff; --text:#1f2328; --muted:#6b7280;
  --border:#dfe3e8; --accent:#2a7f9e; --accent-2:#1a8fb0;
  --btn:#f4f6f8; --btn-hover:#e7ebef; --thumb:#e8ecf0;
  --shadow:0 8px 28px rgba(20,30,45,.16); --navbg:rgba(255,255,255,.86);
  --fav:#b8860b; --favbg:#fff6d9; --favborder:#e8c766;
  --ok:#16a34a; --gutter:10px;
}}
:root[data-theme=dark]{{
  --bg:#111318; --surface:#1b1e24; --text:#e8eaed; --muted:#9aa0a8;
  --border:#2e323a; --accent:#107c9e; --accent-2:#1a8fb0;
  --btn:#262a31; --btn-hover:#333842; --thumb:#2a2e35;
  --shadow:0 12px 48px rgba(0,0,0,.6); --navbg:rgba(24,26,31,.82);
  --fav:#ffd166; --favbg:#3a321f; --favborder:#8a6d2f;
  --ok:#4ade80;
}}
@media(prefers-color-scheme:dark){{
  :root:not([data-theme=light]){{
    --bg:#111318; --surface:#1b1e24; --text:#e8eaed; --muted:#9aa0a8;
    --border:#2e323a; --accent:#107c9e; --accent-2:#1a8fb0;
    --btn:#262a31; --btn-hover:#333842; --thumb:#2a2e35;
    --shadow:0 12px 48px rgba(0,0,0,.6); --navbg:rgba(24,26,31,.82);
    --fav:#ffd166; --favbg:#3a321f; --favborder:#8a6d2f;
    --ok:#4ade80;
  }}
}}
body{{font-family:"Noto Sans CJK SC","Microsoft YaHei",sans-serif;background:var(--bg);color:var(--text);height:100vh;overflow:hidden;display:flex;flex-direction:column}}
#stage{{flex:1;position:relative;display:flex;align-items:center;justify-content:center;min-height:0;padding:8px var(--gutter)}}
#photo{{max-width:100%;max-height:100%;object-fit:contain;border-radius:8px;box-shadow:var(--shadow);transition:opacity .18s}}
#photo.loading{{opacity:.25}}
.nav{{position:absolute;top:50%;transform:translateY(-50%);width:42px;height:42px;border-radius:50%;border:1px solid var(--border);background:var(--navbg);color:var(--text);font-size:0;line-height:1;cursor:pointer;display:flex;align-items:center;justify-content:center;transition:background .15s,z-index 0s}}
/* SVG 路径的几何中心与 viewBox 中心重合，彻底避免 ‹ › 字形侧距造成的视觉偏心 */
.nav svg{{width:20px;height:20px;display:block}}
.nav:hover{{background:var(--btn-hover)}}
.nav:disabled{{opacity:.25;cursor:default}}
#prev{{left:14px}}
#next{{right:14px}}
#bar{{background:var(--surface);border-top:1px solid var(--border);padding:9px var(--gutter) 11px}}
#tabs{{display:flex;gap:5px;margin-bottom:7px}}
.tab{{padding:5px 12px;border:1px solid transparent;border-radius:5px;background:transparent;color:var(--muted);font-size:12px;cursor:pointer}}
.tab:hover{{color:var(--text)}}
.tab.on{{background:var(--accent);border-color:var(--accent);color:#fff}}
#info{{display:flex;align-items:center;gap:9px;margin-bottom:8px;min-height:30px}}
#title{{flex:1;font-size:13px;font-weight:500;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
#date{{font-size:11px;color:var(--muted);white-space:nowrap}}
.btn{{padding:6px 10px;border-radius:6px;border:1px solid var(--border);background:var(--btn);color:var(--text);font-size:12px;cursor:pointer;white-space:nowrap}}
.btn:hover{{background:var(--btn-hover)}}
.btn.main{{background:var(--accent);border-color:var(--accent);color:#fff}}
.btn.main:hover{{background:var(--accent-2)}}
.btn:disabled{{opacity:.45;cursor:default}}
/* 收藏按钮：未收藏与「图片故事」同款默认样式；已收藏用「全部壁纸」的蓝色 */
#favbtn.fav-on{{background:var(--accent);border-color:var(--accent);color:#fff}}
#favbtn.fav-on:hover{{background:var(--accent-2)}}
#thumbs{{display:flex;gap:7px;overflow-x:auto;padding-bottom:3px;justify-content:flex-start}}
.thumb-wrap{{position:relative;flex-shrink:0;line-height:0}}
/* cover：容器与壁纸同为 16:9，铺满且不留白边、不裁切画面 */
.thumb{{width:92px;height:52px;object-fit:cover;border-radius:5px;border:2px solid transparent;cursor:pointer;opacity:.55;display:block;background:var(--thumb);transition:opacity .15s,border-color .15s}}
.thumb-wrap:hover .thumb{{opacity:.85}}
.thumb.active{{opacity:1;border-color:var(--accent)}}
/* 收藏图标：统一饱满心形，未收藏半透明白、已收藏白色实心（与描边同色） */
.thumb-fav{{position:absolute;right:6px;bottom:5px;width:14px;height:14px;border:0;background:none;padding:0;cursor:pointer;filter:drop-shadow(0 1px 3px rgba(0,0,0,.9))}}
.thumb-fav svg,#photo-fav svg{{width:100%;height:100%;display:block}}
.thumb-fav .heart,#photo-fav .heart{{fill:rgba(255,255,255,.30);stroke:#fff;stroke-width:1.8;stroke-linejoin:round;transition:fill .18s}}
.thumb-fav.on .heart,#photo-fav.on .heart{{fill:#fff}}
#photo-fav{{position:absolute;display:none;width:24px;height:24px;border:0;background:none;padding:0;cursor:pointer;filter:drop-shadow(0 2px 6px rgba(0,0,0,.9));z-index:5;transition:transform .15s}}
#photo-fav:hover{{transform:scale(1.12)}}
#favcfg{{display:none;align-items:center;gap:6px;white-space:nowrap}}
#fovtoggle{{display:inline-flex;align-items:center;gap:4px;cursor:pointer;user-select:none;color:var(--muted);font-size:12px}}
#fovtoggle input{{width:14px;height:14px;accent-color:var(--accent);cursor:pointer;margin:0}}
#fovmore{{display:none;align-items:center;gap:5px}}
#fovnum{{width:48px;padding:4px 5px;border-radius:5px;border:1px solid var(--border);background:var(--btn);color:var(--text);font-size:11px}}
#fovunit{{padding:4px 5px;border-radius:5px;border:1px solid var(--border);background:var(--btn);color:var(--text);font-size:11px}}
#fovsave{{padding:5px 9px;font-size:11px}}
#toast{{position:fixed;left:50%;bottom:18px;transform:translateX(-50%) translateY(16px);background:var(--surface);border:1px solid var(--border);border-radius:10px;padding:9px 16px;font-size:12px;box-shadow:var(--shadow);opacity:0;pointer-events:none;transition:.22s;z-index:50;display:flex;align-items:center;gap:9px;white-space:nowrap}}
#toast.show{{opacity:1;transform:translateX(-50%) translateY(0)}}
#toast .ok{{width:19px;height:19px;border-radius:50%;background:var(--ok);color:#fff;display:flex;align-items:center;justify-content:center;font-size:12px}}
@media(min-width:880px){{
  :root{{--gutter:64px}}
  #stage{{padding:18px var(--gutter)}}
  #photo{{border-radius:12px}}
  .nav{{width:52px;height:52px}}
  .nav svg{{width:26px;height:26px}}
  #prev{{left:26px}} #next{{right:26px}}
  #bar{{padding:14px var(--gutter) 16px}}
  #tabs{{gap:6px;margin-bottom:10px}}
  .tab{{padding:6px 15px;font-size:13px}}
  #info{{gap:14px;margin-bottom:10px;min-height:34px}}
  #title{{font-size:15px}}
  #date{{font-size:12px}}
  .btn{{padding:7px 15px;font-size:13px}}
  #thumbs{{gap:9px}}
  .thumb{{width:112px;height:63px}}
  .thumb-fav{{width:16px;height:16px}}
  #photo-fav{{width:28px;height:28px}}
  #fovtoggle,#fovnum,#fovunit,#fovsave,#fovmore{{font-size:12px}}
}}
</style>
</head>
<body>
<div id="stage">
  <button class="nav" id="prev" title="上一张" aria-label="上一张"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M16 5 L8 12 L16 19" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg></button>
  <img id="photo" alt="壁纸预览">
  <button class="nav" id="next" title="下一张" aria-label="下一张"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M8 5 L16 12 L8 19" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"/></svg></button>
  <button id="photo-fav" title="收藏 / 取消收藏" aria-label="收藏"><svg viewBox="0 0 24 24" aria-hidden="true"><path class="heart" d="M12 21.35l-1.45-1.32C5.4 15.36 2 12.28 2 8.5 2 5.42 4.42 3 7.5 3c1.74 0 3.41.81 4.5 2.09C13.09 3.81 14.76 3 16.5 3 19.58 3 22 5.42 22 8.5c0 3.78-3.4 6.86-8.55 11.54L12 21.35z"/></svg></button>
</div>
<div id="bar">
  <div id="tabs">
    <button class="tab" id="tab-all">全部壁纸</button>
    <button class="tab" id="tab-fav">我的收藏</button>
  </div>
  <div id="info">
    <div id="title">加载中…</div>
    <div id="date"></div>
    <div id="favcfg">
      <label id="fovtoggle"><input type="checkbox" id="foven"> 自动更换</label>
      <span id="fovmore">每 <input type="number" id="fovnum" min="1" max="365" value="30"><select id="fovunit"><option value="min">分钟</option><option value="hour">小时</option><option value="day">天</option></select><button class="btn" id="fovsave">保存</button></span>
    </div>
    <button class="btn" id="favbtn" title="收藏 / 取消收藏">收藏</button>
    <button class="btn" id="story" title="查看图片故事">图片故事</button>
  </div>
  <div id="thumbs"></div>
</div>
<div id="toast"><span class="ok">✓</span><span id="toast-msg"></span></div>
<script>
const DATA = {data};
// DATA 已按「最旧 → 今天」排列，默认停在最新的今天（末尾）
let cur = DATA.length ? DATA.length - 1 : 0;
let view = '{start_tab}';
const $ = id => document.getElementById(id);
const photo = $('photo');
// 饱满心形，缩略图与大图共用同一路径；收藏态只由 .on 切换填充色
const HEART_PATH = 'M12 21.35l-1.45-1.32C5.4 15.36 2 12.28 2 8.5 2 5.42 4.42 3 7.5 3c1.74 0 3.41.81 4.5 2.09C13.09 3.81 14.76 3 16.5 3 19.58 3 22 5.42 22 8.5c0 3.78-3.4 6.86-8.55 11.54L12 21.35z';
const heartSvg = () => '<svg viewBox="0 0 24 24" aria-hidden="true"><path class="heart" d="' + HEART_PATH + '"/></svg>';
let toastTimer = 0;
function toast(msg) {{
  $('toast-msg').textContent = msg;
  $('toast').classList.add('show');
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => $('toast').classList.remove('show'), 2200);
}}

// 箭头按「当前图片的实际显示框」居中，而不是按窗口留白居中；
// 图片尺寸随窗口变化，load / resize 时重算。
function placeNav() {{
  if (!photo.complete || !photo.naturalWidth) return;
  const stage = $('stage').getBoundingClientRect();
  const p = photo.getBoundingClientRect();
  const half = $('prev').offsetWidth / 2;
  const cy = p.top - stage.top + p.height / 2;
  $('prev').style.top = cy + 'px';
  $('next').style.top = cy + 'px';
  // 略微外移到图片边缘之外，视觉上不压住画面
  $('prev').style.left = Math.max(half - 8, p.left - stage.left - 6) + 'px';
  $('next').style.right = Math.max(half - 8, stage.right - p.right - 6) + 'px';
  // 大图收藏图标贴在图片右下角内侧
  const pf = $('photo-fav');
  pf.style.left = (p.right - stage.left - pf.offsetWidth - 8) + 'px';
  pf.style.top = (p.bottom - stage.top - pf.offsetHeight - 8) + 'px';
  // 下方按钮栏 / 缩略图按图片实际左右边缘对齐（contain 居中后的真实留白，
  // 不是 CSS 固定边距），大小窗、不同宽高比都严丝合缝。
  const bar = $('bar');
  bar.style.paddingLeft = (p.left - stage.left) + 'px';
  bar.style.paddingRight = (stage.right - p.right) + 'px';
  // 下方栏宽度跟着变了，缩略图间距重新自适应
  layoutThumbs();
}}
// 缩略图一行张数随窗口宽度自适应：以约 100px 的理想尺寸计算能放几张，
// 小窗约 6 张，放大/全屏时自动增加张数铺满；尺寸保持在 52~140px 的舒适区间。
// 张数不足时保持同样尺寸靠左排，间距仍为 10px，不硬拉开。
// 缩略图容器与壁纸同为 16:9，用 object-fit:cover 铺满，不留白边。
function layoutThumbs() {{
  const box = $('thumbs');
  const wraps = box.children;
  if (!wraps.length) return;
  const width = box.clientWidth;
  const gap = 10;
  const ideal = 100;
  const show = Math.max(1, Math.round((width + gap) / (ideal + gap)));
  let tw = (width - (show - 1) * gap) / show;
  tw = Math.max(52, Math.min(140, tw));
  box.style.gap = gap + 'px';
  const h = Math.round(tw * 9 / 16);
  const fs = Math.max(10, Math.min(14, Math.round(tw * 0.16)));
  Array.from(wraps).forEach(w => {{
    const im = w.querySelector('.thumb');
    if (im) {{ im.style.width = tw + 'px'; im.style.height = h + 'px'; }}
    const fv = w.querySelector('.thumb-fav');
    if (fv) {{ fv.style.width = fs + 'px'; fv.style.height = fs + 'px'; }}
  }});
  scrollThumbIntoView();
}}
// 当前缩略图滚入可见范围，避免一屏只放 6 张时看不到正在预览的那张
function scrollThumbIntoView() {{
  const box = $('thumbs');
  const active = box.children[cur];
  if (!active) return;
  const a = active.getBoundingClientRect();
  const b = box.getBoundingClientRect();
  if (a.left < b.left) box.scrollLeft -= (b.left - a.left);
  else if (a.right > b.right) box.scrollLeft += (a.right - b.right);
}}
// 缩放后布局是异步的，等两帧拿到稳定的图片几何再定位，避免用旧尺寸算偏
function placeNavSoon() {{
  requestAnimationFrame(() => requestAnimationFrame(placeNav));
  setTimeout(placeNav, 80);
}}
photo.onload = placeNavSoon;
window.addEventListener('resize', placeNavSoon);

// 画廊模式：当前显示哪张就自动应用哪张，全程静默（不弹通知也不弹提示）。
// 串行执行，快速连点时等当前请求完成后追到最新一张；失败稍后重试。
let lastApplied = -1;
let applying = false;
async function applyCurrent() {{
  const list = visible();
  if (!list.length || applying) return;
  const it = list[cur];
  if (!it || it.i === lastApplied) return;
  applying = true;
  try {{
    const r = await fetch('/set/' + it.i + '?quiet=1');
    const j = await r.json();
    if (j.ok) {{
      lastApplied = it.i;
    }} else {{
      setTimeout(applyCurrent, 800);
    }}
  }} catch (e) {{
    setTimeout(applyCurrent, 1200);
  }}
  applying = false;
  const now = visible()[cur];
  if (now && now.i !== lastApplied) applyCurrent();
}}

// 收藏自动更换：默认只显示「自动更换」开关，开启后在同一行展开时间设置
function updateFovMore() {{
  $('fovmore').style.display = $('foven').checked ? 'inline-flex' : 'none';
}}
async function saveFovCfg(feedback) {{
  const n = Math.max(1, Math.min(365, parseInt($('fovnum').value, 10) || 30));
  const q = '?enabled=' + ($('foven').checked ? 1 : 0)
          + '&interval=' + n + '&unit=' + encodeURIComponent($('fovunit').value);
  let ok = false;
  try {{
    const r = await fetch('/favcfg/set' + q);
    const j = await r.json();
    ok = !!j.ok;
  }} catch (e) {{}}
  if (feedback) {{
    const b = $('fovsave');
    b.textContent = ok ? '✓' : '!';
    setTimeout(() => {{ b.textContent = '保存'; }}, 1200);
  }}
}}
fetch('/favcfg').then(r=>r.json()).then(c=>{{
  $('foven').checked = !!c.enabled;
  $('fovnum').value = c.interval;
  $('fovunit').value = c.unit;
  updateFovMore();
}}).catch(()=>{{}});
$('foven').onchange = () => {{ updateFovMore(); saveFovCfg(false); }};
$('fovsave').onclick = () => saveFovCfg(true);

// 当前标签下可见的壁纸：全部 或 只看已收藏
function visible() {{
  return view === 'fav' ? DATA.filter(x => x.f) : DATA;
}}

function fmtDate(d) {{
  if (!d || d.length < 8) return '';
  return d.slice(0,4) + '-' + d.slice(4,6) + '-' + d.slice(6,8);
}}

function loadMain() {{
  const list = visible();
  if (!list.length) return;
  const it = list[cur];
  photo.classList.add('loading');
  const img = new Image();
  img.onload = () => {{ photo.src = img.src; photo.classList.remove('loading'); }};
  img.onerror = () => {{ setTimeout(loadMain, 1200); }};
  img.src = '/img/' + it.i + '.jpg?t=' + Date.now();
}}

function render() {{
  const list = visible();
  $('favcfg').style.display = view === 'fav' ? 'inline-flex' : 'none';
  $('tab-all').classList.toggle('on', view === 'all');
  $('tab-fav').classList.toggle('on', view === 'fav');
  if (!DATA.length) {{
    $('title').textContent = '壁纸列表加载中，几秒后自动刷新…';
    setTimeout(() => location.reload(), 3000);
    return;
  }}
  if (!list.length) {{
    $('title').textContent = '还没有收藏壁纸';
    $('date').textContent = '在「全部壁纸」中点「收藏」按钮';
    photo.removeAttribute('src');
    $('photo-fav').style.display = 'none';
    $('prev').disabled = $('next').disabled = true;
    $('favbtn').disabled = $('story').disabled = true;
    document.querySelectorAll('.thumb-wrap').forEach(el => el.remove());
    return;
  }}
  if (cur >= list.length) cur = list.length - 1;
  const it = list[cur];
  $('title').textContent = (view === 'fav' ? '收藏 · ' : (it.i === 0 ? '今天 · ' : it.i + ' 天前 · ')) + it.t;
  $('date').textContent = fmtDate(it.d);
  const pf = $('photo-fav');
  pf.style.display = 'block';
  pf.classList.toggle('on', !!it.f);
  pf.title = it.f ? '取消收藏' : '收藏';
  $('favbtn').disabled = $('story').disabled = false;
  $('favbtn').textContent = it.f ? '已收藏' : '收藏';
  $('favbtn').classList.toggle('fav-on', !!it.f);
  $('story').disabled = !it.s;
  $('prev').disabled = cur <= 0;
  $('next').disabled = cur >= list.length - 1;
  document.querySelectorAll('.thumb').forEach((el, k) => el.classList.toggle('active', k === cur));
  scrollThumbIntoView();
  loadMain();
  applyCurrent();
}}

function go(step) {{
  const list = visible();
  const next = cur + step;
  if (next < 0 || next >= list.length) return;
  cur = next;
  render();
}}

function switchView(v) {{
  view = v;
  // 反转后最新的Today在末尾，切标签默认停在最新一张
  const list = visible();
  cur = list.length ? list.length - 1 : 0;
  renderThumbs();
  render();
}}
$('tab-all').onclick = () => switchView('all');
$('tab-fav').onclick = () => switchView('fav');

// 与托盘一致：左箭头「上一张」往更早走（cur-1），右箭头「下一张」往更新走（cur+1）
$('prev').onclick = () => go(-1);
$('next').onclick = () => go(1);
document.addEventListener('keydown', e => {{
  if (e.key === 'ArrowRight') go(1);
  if (e.key === 'ArrowLeft') go(-1);
  if (e.key === 'Escape') window.close();
}});
$('story').onclick = () => {{ const it = visible()[cur]; if (it && it.s) window.open(it.s, '_blank'); }};

$('favbtn').onclick = async () => {{
  const it = visible()[cur];
  if (!it) return;
  const b = $('favbtn');
  b.disabled = true;
  try {{
    const r = await fetch('/fav/' + it.i);
    const j = await r.json();
    if (j.ok) {{
      it.f = j.fav;
      syncFavIcons(it, j.fav);
      toast(j.fav ? '已保存到收藏夹' : '已取消收藏');
      if (view === 'fav' && !j.fav) {{
        // 在收藏页取消收藏：从当前列表移除，避免索引错位
        renderThumbs();
        if (cur >= visible().length) cur = Math.max(0, visible().length - 1);
        render();
        return;
      }}
      render();
    }}
  }} catch (e) {{}}
  b.disabled = false;
}};

$('photo-fav').onclick = () => {{ const it = visible()[cur]; if (it) setFav(it, $('photo-fav')); }};

// 收藏状态同步到大图图标、缩略图图标、底部收藏按钮
function syncFavIcons(it, fav) {{
  const pf = $('photo-fav');
  pf.classList.toggle('on', fav);
  pf.title = fav ? '取消收藏' : '收藏';
  const list = visible();
  document.querySelectorAll('.thumb-wrap').forEach((w, k) => {{
    if (list[k] !== it) return;
    const b = w.querySelector('.thumb-fav');
    b.classList.toggle('on', fav);
    b.title = fav ? '取消收藏' : '收藏';
  }});
  if (list[cur] === it) {{
    $('favbtn').textContent = fav ? '已收藏' : '收藏';
    $('favbtn').classList.toggle('fav-on', fav);
  }}
}}

// 切换收藏：大图图标与缩略图图标共用
async function setFav(it, btn) {{
  if (btn.disabled) return;
  btn.disabled = true;
  try {{
    const r = await fetch('/fav/' + it.i);
    const j = await r.json();
    if (j.ok) {{
      it.f = j.fav;
      syncFavIcons(it, j.fav);
      if (view === 'fav' && !j.fav) {{
        // 在收藏页取消收藏：列表少一张，重建缩略图并校正当前位置
        renderThumbs();
        if (cur >= visible().length) cur = Math.max(0, visible().length - 1);
        render();
      }}
    }}
  }} catch (e) {{}}
  btn.disabled = false;
}}

function renderThumbs() {{
  const box = $('thumbs');
  box.innerHTML = '';
  visible().forEach((it, k) => {{
    const wrap = document.createElement('div');
    wrap.className = 'thumb-wrap';
    const im = document.createElement('img');
    im.className = 'thumb' + (k === cur ? ' active' : '');
    im.alt = fmtDate(it.d);
    im.title = (view === 'all' ? (it.i === 0 ? '今天' : it.i + ' 天前') : '收藏') + ' · ' + it.t;
    im.src = '/img/' + it.i + '.jpg' + (it.f ? '?f=1' : '');
    im.onerror = () => {{ im.style.opacity = '.2'; }};
    im.onclick = () => {{ cur = k; render(); }};
    const fv = document.createElement('button');
    fv.className = 'thumb-fav' + (it.f ? ' on' : '');
    fv.innerHTML = heartSvg();
    fv.title = it.f ? '取消收藏' : '收藏';
    fv.onclick = () => setFav(it, fv);
    wrap.appendChild(im);
    wrap.appendChild(fv);
    box.appendChild(wrap);
  }});
  layoutThumbs();
}}
renderThumbs();
render();
</script>
</body>
</html>"#,
        data = data,
        start_tab = start_tab,
        theme = theme
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::tray::BusyState;
    use std::sync::Arc;

    fn shared() -> Shared {
        Shared {
            cfg: Arc::new(std::sync::Mutex::new(Config::default())),
            busy: BusyState::default(),
            previews: Arc::new(std::sync::Mutex::new(Vec::new())),
            gallery_port: Arc::new(std::sync::Mutex::new(0)),
            update_tx: None,
        }
    }

    #[test]
    fn page_has_favorite_tabs_and_buttons() {
        let s = shared();
        let all = page_html(&s, false);
        assert!(all.contains("id=\"tab-all\""));
        assert!(all.contains("id=\"tab-fav\""));
        assert!(all.contains("id=\"favbtn\""));
        assert!(all.contains("/fav/"));
        assert!(all.contains("let view = 'all';"));

        let fav = page_html(&s, true);
        assert!(fav.contains("let view = 'fav';"));
    }

    #[test]
    fn page_uses_software_favicon() {
        let html = page_html(&shared(), false);
        assert!(html.contains("/favicon.png"));
        assert!(html.contains("rel=\"icon\""));
    }

    #[test]
    fn page_has_favorite_slideshow_settings() {
        let html = page_html(&shared(), true);
        assert!(html.contains("id=\"favcfg\""));
        assert!(html.contains("id=\"foven\""));
        assert!(html.contains("id=\"fovnum\""));
        assert!(html.contains("id=\"fovunit\""));
        assert!(html.contains("value=\"min\""));
        assert!(html.contains("value=\"hour\""));
        assert!(html.contains("value=\"day\""));
        assert!(html.contains("fetch('/favcfg')"));
        assert!(html.contains("fetch('/favcfg/set'"));
        // 设置条收进信息行，位于「已收藏」按钮左侧；默认折叠只显示开关
        let info = html.find("id=\"info\"").unwrap();
        let cfg = html.find("id=\"favcfg\"").unwrap();
        let favbtn = html.find("id=\"favbtn\"").unwrap();
        assert!(info < cfg && cfg < favbtn);
        assert!(html.contains("id=\"fovmore\""));
        assert!(html.contains("function updateFovMore()"));
        assert!(html.contains("#fovmore{display:none"));
    }

    #[test]
    fn favcfg_route_returns_settings_json() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let worker = std::thread::spawn(move || handle(&mut server, &shared()));
        client
            .write_all(b"GET /favcfg HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .unwrap();
        let mut resp = String::new();
        client.read_to_string(&mut resp).unwrap();
        worker.join().unwrap();
        assert!(resp.contains("HTTP/1.1 200 OK"));
        assert!(resp.contains("\"enabled\""));
        assert!(resp.contains("\"interval\""));
        assert!(resp.contains("\"unit\""));
    }

    #[test]
    fn query_param_parses_simple_values() {
        assert_eq!(query_param("enabled=1&interval=45&unit=hour", "unit"), Some("hour".into()));
        assert_eq!(query_param("enabled=1&interval=45", "missing"), None);
    }

    #[test]
    fn window_id_parses_wmctrl_hex() {
        // wmctrl 给 0x 十六进制 ID；xdotool 给十进制，由 active_window_id 按 u64 解析。
        // 两边统一成数值后即可比较（0x0b000003 == 184549379）。
        assert_eq!(super::parse_window_id("0x0b000003"), Some(184549379));
        assert_eq!("184549379".parse::<u64>().ok(), super::parse_window_id("0x0b000003"));
        assert_eq!(super::parse_window_id("  "), None);
    }

    #[test]
    fn page_auto_applies_wallpaper_without_button() {
        let html = page_html(&shared(), false);
        // 「设为壁纸」按钮移除，切换后自动应用
        assert!(!html.contains("id=\"setbtn\""));
        assert!(!html.contains("设为壁纸"));
        assert!(html.contains("async function applyCurrent()"));
        assert!(html.contains("fetch('/set/' + it.i + '?quiet=1')"));
        assert!(html.contains("applyCurrent();\n}"));
        // 箭头按图片实际显示框居中，且用 CSS 图形避免字形偏心
        assert!(html.contains("function placeNav()"));
        assert!(html.contains("photo.onload = placeNavSoon"));
        assert!(html.contains("window.addEventListener('resize', placeNavSoon)"));
        assert!(html.contains(".nav svg"));
        assert!(!html.contains("id=\"hint\""));
        assert!(!html.contains("点缩略图选日期"));
        assert!(html.contains("M16 5 L8 12 L16 19"));
        assert!(html.contains("M8 5 L16 12 L8 19"));
        // 缩略图右下角收藏图标，点图标不切换当前预览图
        assert!(html.contains(".thumb-fav{"));
        assert!(html.contains("fv.className = 'thumb-fav'"));
        assert!(html.contains("id=\"photo-fav\""));
        assert!(html.contains("function syncFavIcons("));
        assert!(html.contains("async function setFav("));
        assert!(html.contains("p.left - stage.left - 6"));
        assert!(html.contains("max(half - 8"));
        assert!(html.contains("pf.offsetWidth"));
        // 下方按钮栏 / 缩略图与上方大图共用同一横向边距，两侧对齐
        assert!(html.contains("--gutter:10px"));
        assert!(html.contains("padding:8px var(--gutter)"));
        assert!(html.contains("padding:9px var(--gutter) 11px"));
        assert!(html.contains(":root{--gutter:64px}"));
        assert!(html.contains("bar.style.paddingLeft = (p.left - stage.left)"));
        assert!(html.contains("bar.style.paddingRight = (stage.right - p.right)"));
        // 饱满心形：缩略图与大图共用路径，收藏态只切换填充色
        assert!(html.contains("class=\"heart\""));
        assert!(html.contains("const heartSvg = () =>"));
        assert!(html.contains("fv.innerHTML = heartSvg()"));
        assert!(html.contains(".thumb-fav.on .heart,#photo-fav.on .heart{fill:#fff}"));
        // 缩略图铺满无白边 + 间距自适应（多则铺满两侧，少则靠左小间距）
        assert!(html.contains("object-fit:cover;border-radius:5px"));
        assert!(html.contains("function layoutThumbs()"));
        assert!(html.contains("const ideal = 100;"));
        assert!(html.contains("Math.round((width + gap) / (ideal + gap))"));
        assert!(html.contains("let tw = (width - (show - 1) * gap) / show;"));
        assert!(html.contains("function scrollThumbIntoView()"));
        assert!(html.contains("const fs = Math.max(10, Math.min(14, Math.round(tw * 0.16)));"));
        assert!(html.contains(".thumb-fav{position:absolute;right:6px;bottom:5px;width:14px;height:14px"));
        assert!(html.contains("  layoutThumbs();\n}\nrenderThumbs();"));
        // 大图收藏图标缩小
        assert!(html.contains("#photo-fav{position:absolute;display:none;width:24px;height:24px"));
        // 收藏按钮用「全部壁纸」的蓝色，且不带星号
        assert!(html.contains("#favbtn.fav-on{background:var(--accent)"));
        assert!(!html.contains("#favbtn{color:var(--accent)"));
        assert!(!html.contains("★ 已收藏"));
        assert!(!html.contains("☆ 收藏"));
    }

    #[test]
    fn gallery_nav_matches_tray_direction() {
        // 画廊「上一张」往更早走（go(-1)），「下一张」往更新走（go(1)），
        // 与托盘 ID_PREV=idx+1(更早) / ID_NEXT=idx-1(更新) 语义一致。
        let html = page_html(&shared(), false);
        assert!(html.contains("$('prev').onclick = () => go(-1);"));
        assert!(html.contains("$('next').onclick = () => go(1);"));
    }
}
