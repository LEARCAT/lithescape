//! 大图画廊：本地回环 HTTP 服务 + PyQt5 原生窗口
//!
//! 后端只监听 127.0.0.1，提供数据/图片/设壁纸/收藏等接口；前端是内嵌的 PyQt5
//! 脚本释放到缓存目录后以独立进程常驻，通过本地 TCP 控制端口接收显示命令。
//! 原生窗口不属于浏览器进程：不进浏览器 dock、关浏览器不受影响、唤回毫秒级。

use crate::bing;
use crate::config::cache_dir;
use crate::tray::{PreviewItem, Shared};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 系统主题查询结果带 15 秒短缓存：gsettings 单次约 70ms，
/// 画廊每次请求数据都同步查一次会明显拖慢弹出；短 TTL 仍允许切换主题后自动跟随。
static DARK_VALUE: AtomicBool = AtomicBool::new(false);
static DARK_CHECKED_AT: AtomicI64 = AtomicI64::new(0);

/// 历史归档与收藏列表按文件戳缓存：数据接口和内嵌图片每次请求都要解析这两个 JSON，
/// 原实现重复读盘 + 反序列化。文件被改动（mtime/大小变化）时缓存自动失效。
type Stamp = (i64, u64);
type ItemCache = Mutex<(Stamp, Vec<crate::config::HistoryItem>)>;
static HISTORY_CACHE: OnceLock<ItemCache> = OnceLock::new();
static FAVORITES_CACHE: OnceLock<ItemCache> = OnceLock::new();

/// PyQt5 画廊窗口控制端口（仅本地回环）。Python 端 QTcpServer 监听此端口。
const QT_CTRL_PORT: u16 = 28765;

/// 内嵌的 PyQt5 画廊脚本，运行时释放到缓存目录。
const QT_GALLERY_SCRIPT: &str = include_str!("gallery_qt.py");

fn unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 文件戳：修改时间（纳秒）+ 大小，足以识别 atomic_write 后的内容变化。
fn file_stamp(path: &Path) -> Stamp {
    match std::fs::metadata(path) {
        Ok(m) => {
            let stamp = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);
            (stamp, m.len())
        }
        Err(_) => (0, 0),
    }
}

fn cached_items(cache: &'static OnceLock<ItemCache>, path: &Path, load: fn() -> Vec<crate::config::HistoryItem>) -> Vec<crate::config::HistoryItem> {
    let stamp = file_stamp(path);
    let mutex = cache.get_or_init(|| Mutex::new(((0, 0), Vec::new())));
    let mut guard = mutex.lock().unwrap_or_else(|e| e.into_inner());
    if guard.0 != stamp {
        let items = load();
        *guard = (stamp, items);
    }
    guard.1.clone()
}

fn cached_history() -> Vec<crate::config::HistoryItem> {
    cached_items(&HISTORY_CACHE, &crate::config::history_path(), crate::config::load_history)
}

fn cached_favorites() -> Vec<crate::config::HistoryItem> {
    cached_items(&FAVORITES_CACHE, &crate::config::favorites_path(), crate::config::load_favorites)
}

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
    // 预热 PyQt5 常驻进程：import PyQt5 + 加载数据/图片约数百毫秒，
    // 放到软件启动时后台做，点托盘时只剩 show 命令，首次也能快速弹出。
    // 进程只起不显示窗口（无弹窗），完全不打扰桌面。
    std::thread::spawn(move || {
        if qt_ctrl_alive() {
            return;
        }
        // 不再延迟：软件启动后立即后台预热，确保用户点托盘时进程已就绪。
        spawn_qt_gallery(port);
    });
    Ok(port)
}

/// PyQt5 脚本释放路径（~/.cache/lithescape/gallery_qt.py，随版本更新覆盖）
fn qt_script_path() -> PathBuf {
    cache_dir()
        .parent()
        .map(|p| p.join("gallery_qt.py"))
        .unwrap_or_else(|| cache_dir().join("gallery_qt.py"))
}

/// 释放内嵌的 PyQt5 脚本到缓存目录。内容变化才重写，避免每次启动都写盘。
fn ensure_qt_script() -> Option<PathBuf> {
    let path = qt_script_path();
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return None;
        }
    }
    let want = QT_GALLERY_SCRIPT.as_bytes();
    let fresh = std::fs::read(&path).map(|b| b == want).unwrap_or(false);
    if !fresh && std::fs::write(&path, want).is_err() {
        return None;
    }
    Some(path)
}

/// 控制端口是否已有 PyQt5 画廊进程在监听（连得上即视为常驻进程存活）
fn qt_ctrl_alive() -> bool {
    TcpStream::connect(("127.0.0.1", QT_CTRL_PORT))
        .map(|_| true)
        .unwrap_or(false)
}

/// 防止预热线程与 open() 同时拉起两个 Python 进程。
static QT_SPAWNING: AtomicBool = AtomicBool::new(false);

/// 启动 PyQt5 画廊常驻进程。脚本里带 --noproxy 无关；用 python3 执行。
fn spawn_qt_gallery(port: u16) -> bool {
    if QT_SPAWNING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .is_err()
    {
        return false;
    }
    let result = (|| {
        let script = match ensure_qt_script() {
            Some(s) => s,
            None => return false,
        };
        use std::process::Stdio;
        std::process::Command::new("python3")
            .arg(&script)
            .arg(port.to_string())
            .arg(QT_CTRL_PORT.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .is_ok()
    })();
    // 起进程成功就一直保持占用（常驻进程在跑）；失败则释放，允许 open() 重试
    if !result {
        QT_SPAWNING.store(false, Ordering::Release);
    }
    result
}

/// 向常驻的 PyQt5 画廊发一行控制命令（show\tall / show\tfav / quit）
fn send_qt_cmd(cmd: &str) -> bool {
    let mut stream = match TcpStream::connect_timeout(
        &"127.0.0.1:28765".parse().unwrap(),
        Duration::from_millis(200),
    ) {
        Ok(s) => s,
        Err(_) => return false,
    };
    use std::io::Write;
    let _ = stream.set_write_timeout(Some(Duration::from_millis(200)));
    stream.write_all(cmd.as_bytes()).is_ok()
}

/// 在原生画廊窗口显示壁纸。favorites=true 进入「我的收藏」标签。
/// 窗口为 PyQt5 常驻进程：首次点击拉起进程，之后只发控制命令，唤回毫秒级。
pub fn open(port: u16, favorites: bool) {
    let view = if favorites { "fav" } else { "all" };
    let cmd = format!("show\t{view}\n");
    // 直接发命令即完成存活探测：原实现先 connect 探测再 connect 发命令，
    // 白白多一次本地 TCP 往返；本地回环连接被拒是瞬时的，不会引入了望延迟。
    if send_qt_cmd(&cmd) {
        return;
    }
    // 控制端口没人监听：拉起常驻进程，等它起来后再发命令
    if !spawn_qt_gallery(port) {
        eprintln!("[warn] 启动 PyQt5 画廊失败（需要 python3 + python3-pyqt5）");
        return;
    }
    // 25ms 轮询，首击等待从 100ms 粒度降到 25ms。
    for _ in 0..60 {
        std::thread::sleep(Duration::from_millis(25));
        if send_qt_cmd(&cmd) {
            return;
        }
    }
    eprintln!("[warn] PyQt5 画廊控制端口未就绪");
}


/// 实时从本地归档构造画廊列表（按日期倒序，index 0 最新）。
/// 归档为空时回退到内存快照，保证首次启动也能显示。
fn current_items(shared: &Shared) -> Vec<PreviewItem> {
    let history = cached_history();
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

    if path == "/" || path.starts_with("/index.html") {
        // 旧版浏览器画廊页面已由 PyQt5 原生窗口取代，主程序不再打开浏览器；
        // 这里只留一个简短提示，避免误访问本地端口时 404。图片/数据接口仍供原生窗口使用。
        const HINT: &str = concat!(
            "<!doctype html><meta charset=utf-8><title>LitheScape</title>",
            "<p style='font:14px sans-serif;color:#666;padding:24px'>",
            "请从桌面托盘图标打开画廊。</p>"
        );
        respond(stream, 200, "text/html; charset=utf-8", HINT.as_bytes());
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
            if matches!(v.as_str(), "sec" | "min" | "hour" | "day" | "login" | "wake") {
                if c.unit != v && (v == "login" || v == "wake") {
                    // 登录/唤醒模式只在程序启动时切一次，清掉旧时间戳保证下次启动生效。
                    c.last_switch.clear();
                    c.last_enddate.clear();
                }
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
    } else if path == "/data" {
        // PyQt5 画廊的数据接口：返回内嵌页面同款 items 数组 + 主题。
        // 数据按「最旧 → 今天」排列，index 仍是真实序号（/img /set /fav 按它查找）。
        let json = gallery_data_json(shared);
        respond(
            stream,
            200,
            "application/json; charset=utf-8",
            json.as_bytes(),
        );
    } else if path == "/view" {
        // 旧版网页端点，PyQt5 前端不再使用，保留兼容返回 ok
        respond(stream, 200, "text/plain", b"ok");
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
    let now = unix_secs();
    let last = DARK_CHECKED_AT.load(Ordering::Acquire);
    // 15 秒内直接复用结果，省掉每次约 70ms 的 gsettings 同步等待
    if last != 0 && now >= last && now - last < 15 {
        return DARK_VALUE.load(Ordering::Acquire);
    }
    use std::process::Command;
    let dark = Command::new("gsettings")
        .args(["get", "com.deepin.dde.appearance", "gtk-theme"])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_ascii_lowercase().contains("dark"))
        .unwrap_or(false);
    DARK_VALUE.store(dark, Ordering::Release);
    DARK_CHECKED_AT.store(now, Ordering::Release);
    dark
}

/// 构造画廊 items JSON 数组（HTML 内嵌数据与 /data 接口共用）。
/// 按「更早 → 今天」排列，index 仍是真实序号（/img /set /fav 按它查找）。
fn gallery_items_json(shared: &Shared) -> String {
    let previews = current_items(shared);
    // 已收藏日期集合：按 enddate 判定，历史归档更新也不会丢收藏状态
    let favorites = cached_favorites();
    let fav_dates: std::collections::HashSet<&str> = favorites
        .iter()
        .map(|h| h.meta.enddate.as_str())
        .collect();
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
    format!("[{}]", items.join(","))
}

/// PyQt5 画廊的数据接口：items + 系统主题
fn gallery_data_json(shared: &Shared) -> String {
    let items = gallery_items_json(shared);
    let theme = if system_dark() { "dark" } else { "light" };
    format!("{{\"items\":{items},\"theme\":\"{theme}\"}}")
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
    fn gallery_data_has_items_and_theme() {
        let s = shared();
        let json = gallery_data_json(&s);
        assert!(json.contains("\"items\""));
        assert!(json.contains("\"theme\""));
        assert!(json.contains("\"dark\"") || json.contains("\"light\""));
    }

    #[test]
    fn qt_script_has_gallery_interactions() {
        // 原生画廊已取代旧浏览器页面：校验内嵌 PyQt5 脚本覆盖关键交互，
        // 防止清理/改写时误删收藏、自动更换、缩略图滚动、失焦关闭等逻辑。
        for marker in [
            "WIN_W, WIN_H = 620, 560", "THUMB_GAP = 10",
            "class FovCheck", "class CheckIcon", "class ThumbScrollArea",
            "def show_gallery", "def focusOutEvent", "def _check_focus",
            "def _workarea_bottom_sample", "self.toast_timer.start(1000)",
            "def toggle_fav_by_index", "def _save_fov",
        ] {
            assert!(QT_GALLERY_SCRIPT.contains(marker), "缺少脚本标记: {}", marker);
        }
    }

    #[test]
    fn root_route_returns_short_hint() {
        // 根路径只返回简短提示，旧版浏览器画廊页面已移除，避免误访问时 404。
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut client = std::net::TcpStream::connect(addr).unwrap();
        let (mut server, _) = listener.accept().unwrap();
        let worker = std::thread::spawn(move || handle(&mut server, &shared()));
        client.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        let mut resp = String::new();
        client.read_to_string(&mut resp).unwrap();
        worker.join().unwrap();
        assert!(resp.contains("HTTP/1.1 200 OK"));
        assert!(resp.contains("text/html"));
        // 提示页足够短，不再内嵌整套画廊 HTML/CSS/JS。
        assert!(resp.len() < 600, "根路由响应过长: {} bytes", resp.len());
    }
}
