//! 大图画廊：本地回环 HTTP 服务 + 内嵌 HTML/JS
//!
//! 参考必应壁纸官方的交互：一张大图 + 左右箭头 / 键盘切换 + 缩略图导航。
//! 只监听 127.0.0.1，零外部依赖，浏览器由 xdg-open 调起。

use crate::bing;
use crate::config::cache_dir;
use crate::tray::{PreviewItem, Shared};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;

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

/// 在浏览器打开画廊。favorites=true 时默认进入「我的收藏」标签。
pub fn open(port: u16, favorites: bool) {
    let path = if favorites { "/?view=fav" } else { "/" };
    bing::open_url(&format!("http://127.0.0.1:{}{path}", port));
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
        // 结果用桌面通知发出（与托盘其它通知一致，会自动消失），
        // 不再用浏览器 alert 弹窗（那个弹窗标题带 127.0.0.1:端口，很突兀）
        let idx: i32 = rest.parse().unwrap_or(0);
        let (ok, msg) = set_wallpaper(shared, idx);
        if ok {
            bing::notify("壁纸已更新", &msg);
        } else {
            bing::notify("设置失败", &msg);
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
    let cfg = shared.cfg.lock().map(|c| c.clone()).unwrap_or_default();
    if !shared.begin_busy() {
        return (false, "正在更换壁纸，请稍候".into());
    }
    let _guard = shared.busy_guard();
    let result = bing::apply_from_preview(&item.meta, &item.image_url, idx, &cfg);
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

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<link rel="icon" type="image/png" href="/favicon.png">
<title>LitheScape 壁纸画廊</title>
<style>
*{{box-sizing:border-box;margin:0;padding:0}}
body{{font-family:"Noto Sans CJK SC","Microsoft YaHei",sans-serif;background:#111318;color:#e8eaed;height:100vh;overflow:hidden;display:flex;flex-direction:column}}
#stage{{flex:1;position:relative;display:flex;align-items:center;justify-content:center;min-height:0}}
#photo{{max-width:100%;max-height:100%;object-fit:contain;box-shadow:0 12px 48px rgba(0,0,0,.6);transition:opacity .18s}}
#photo.loading{{opacity:.25}}
.nav{{position:absolute;top:50%;transform:translateY(-50%);width:52px;height:52px;border-radius:50%;border:1px solid #454950;background:rgba(24,26,31,.82);color:#fff;font-size:30px;line-height:1;cursor:pointer;display:flex;align-items:center;justify-content:center;transition:background .15s,z-index 0s}}
.nav:hover{{background:rgba(56,60,68,.95)}}
.nav:disabled{{opacity:.25;cursor:default}}
#prev{{left:26px}}
#next{{right:26px}}
#bar{{background:#1b1e24;border-top:1px solid #2e323a;padding:12px 26px 14px}}
#info{{display:flex;align-items:center;gap:14px;margin-bottom:10px;min-height:34px}}
#title{{flex:1;font-size:15px;font-weight:500;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}}
#date{{font-size:12px;color:#9aa0a8;white-space:nowrap}}
.btn{{padding:7px 15px;border-radius:6px;border:1px solid #454950;background:#262a31;color:#e8eaed;font-size:13px;cursor:pointer;white-space:nowrap}}
.btn:hover{{background:#333842}}
.btn.main{{background:#107c9e;border-color:#107c9e;color:#fff}}
.btn.main:hover{{background:#1a8fb0}}
.btn:disabled{{opacity:.45;cursor:default}}
#thumbs{{display:flex;gap:9px;overflow-x:auto;padding-bottom:3px}}
.thumb{{width:112px;height:63px;object-fit:cover;border-radius:5px;border:2px solid transparent;cursor:pointer;opacity:.55;flex-shrink:0;background:#2a2e35;transition:opacity .15s,border-color .15s}}
.thumb:hover{{opacity:.85}}
.thumb.active{{opacity:1;border-color:#107c9e}}
#hint{{position:absolute;top:16px;left:50%;transform:translateX(-50%);font-size:12px;color:#9aa0a8;background:rgba(24,26,31,.8);padding:6px 14px;border-radius:20px}}
#tabs{{display:flex;gap:6px;margin-bottom:10px}}
.tab{{padding:6px 15px;border:1px solid transparent;border-radius:5px;background:transparent;color:#9aa0a8;font-size:13px;cursor:pointer}}
.tab:hover{{color:#e8eaed}}
.tab.on{{background:#107c9e;border-color:#107c9e;color:#fff}}
.btn.fav-on{{color:#ffd166;border-color:#8a6d2f;background:#3a321f}}
.btn.fav-on:hover{{background:#4a4028}}
@media(max-width:760px){{#date{{display:none}}.btn{{padding:6px 10px;font-size:12px}}.thumb{{width:84px;height:48px}}}}
</style>
</head>
<body>
<div id="stage">
  <div id="hint">← → 切换 · 点缩略图选日期</div>
  <button class="nav" id="prev" title="上一张">‹</button>
  <img id="photo" alt="壁纸预览">
  <button class="nav" id="next" title="下一张">›</button>
</div>
<div id="bar">
  <div id="tabs">
    <button class="tab" id="tab-all">全部壁纸</button>
    <button class="tab" id="tab-fav">我的收藏</button>
  </div>
  <div id="info">
    <div id="title">加载中…</div>
    <div id="date"></div>
    <button class="btn" id="favbtn" title="收藏 / 取消收藏">☆ 收藏</button>
    <button class="btn" id="story" title="查看图片故事">图片故事</button>
    <button class="btn main" id="setbtn">设为壁纸</button>
  </div>
  <div id="thumbs"></div>
</div>
<script>
const DATA = {data};
// DATA 已按「最旧 → 今天」排列，默认停在最新的今天（末尾）
let cur = DATA.length ? DATA.length - 1 : 0;
let view = '{start_tab}';
const $ = id => document.getElementById(id);
const photo = $('photo');

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
  $('tab-all').classList.toggle('on', view === 'all');
  $('tab-fav').classList.toggle('on', view === 'fav');
  if (!DATA.length) {{
    $('title').textContent = '壁纸列表加载中，几秒后自动刷新…';
    setTimeout(() => location.reload(), 3000);
    return;
  }}
  if (!list.length) {{
    $('title').textContent = '还没有收藏壁纸';
    $('date').textContent = '在「全部壁纸」中点 ☆ 收藏';
    photo.removeAttribute('src');
    $('prev').disabled = $('next').disabled = true;
    $('favbtn').disabled = $('story').disabled = $('setbtn').disabled = true;
    document.querySelectorAll('.thumb').forEach(el => el.remove());
    return;
  }}
  if (cur >= list.length) cur = list.length - 1;
  const it = list[cur];
  $('title').textContent = (view === 'fav' ? '收藏 · ' : (it.i === 0 ? '今天 · ' : it.i + ' 天前 · ')) + it.t;
  $('date').textContent = fmtDate(it.d);
  $('favbtn').disabled = $('story').disabled = $('setbtn').disabled = false;
  $('favbtn').textContent = it.f ? '★ 已收藏' : '☆ 收藏';
  $('favbtn').classList.toggle('fav-on', !!it.f);
  $('story').disabled = !it.s;
  $('prev').disabled = cur <= 0;
  $('next').disabled = cur >= list.length - 1;
  document.querySelectorAll('.thumb').forEach((el, k) => el.classList.toggle('active', k === cur));
  loadMain();
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
}});
$('story').onclick = () => {{ const it = visible()[cur]; if (it && it.s) window.open(it.s, '_blank'); }};
$('setbtn').onclick = async () => {{
  const it = visible()[cur];
  if (!it) return;
  const b = $('setbtn');
  b.disabled = true; b.textContent = '设置中…';
  try {{
    const r = await fetch('/set/' + it.i);
    const j = await r.json();
    // 结果由程序发桌面通知（会自动消失），这里只给按钮短暂反馈，不弹窗
    b.textContent = j.ok ? '✓ 已设为壁纸' : '✗ 设置失败';
  }} catch (e) {{
    b.textContent = '✗ 设置失败';
  }}
  setTimeout(() => {{ b.disabled = false; b.textContent = '设为壁纸'; }}, 1500);
}};

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

function renderThumbs() {{
  const box = $('thumbs');
  box.innerHTML = '';
  visible().forEach((it, k) => {{
    const im = document.createElement('img');
    im.className = 'thumb' + (k === cur ? ' active' : '');
    im.alt = fmtDate(it.d);
    im.title = (it.f ? '★ ' : '') + (view === 'all' ? (it.i === 0 ? '今天' : it.i + ' 天前') : '收藏') + ' · ' + it.t;
    im.src = '/img/' + it.i + '.jpg' + (it.f ? '?f=1' : '');
    im.onerror = () => {{ im.style.opacity = '.2'; }};
    im.onclick = () => {{ cur = k; render(); }};
    box.appendChild(im);
  }});
}}
renderThumbs();
render();
</script>
</body>
</html>"#,
        data = data,
        start_tab = start_tab
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
    fn gallery_nav_matches_tray_direction() {
        // 画廊「上一张」往更早走（go(-1)），「下一张」往更新走（go(1)），
        // 与托盘 ID_PREV=idx+1(更早) / ID_NEXT=idx-1(更新) 语义一致。
        let html = page_html(&shared(), false);
        assert!(html.contains("$('prev').onclick = () => go(-1);"));
        assert!(html.contains("$('next').onclick = () => go(1);"));
    }
}
