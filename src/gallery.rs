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

/// 在浏览器打开画廊
pub fn open(port: u16) {
    bing::open_url(&format!("http://127.0.0.1:{}/", port));
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

    if path == "/" || path.starts_with("/index.html") {
        let html = page_html(shared);
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
    } else if path == "/favicon.ico" {
        respond(stream, 404, "image/x-icon", b"");
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
fn page_html(shared: &Shared) -> String {
    let previews = current_items(shared);
    let mut items = Vec::new();
    for item in &previews {
        let story = if item.meta.copyrightlink.is_empty() {
            String::new()
        } else {
            item.meta.copyrightlink.clone()
        };
        items.push(format!(
            "{{\"i\":{},\"t\":{},\"d\":{},\"c\":{},\"s\":{}}}",
            item.index,
            escape_json(&item.title),
            escape_json(&item.date),
            escape_json(&item.meta.copyright),
            escape_json(&story)
        ));
    }
    let data = format!("[{}]", items.join(","));

    format!(
        r#"<!DOCTYPE html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
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
  <div id="info">
    <div id="title">加载中…</div>
    <div id="date"></div>
    <button class="btn" id="story" title="查看图片故事">图片故事</button>
    <button class="btn main" id="setbtn">设为壁纸</button>
  </div>
  <div id="thumbs"></div>
</div>
<script>
const DATA = {data};
let cur = 0;
const $ = id => document.getElementById(id);
const photo = $('photo');

function fmtDate(d) {{
  if (!d || d.length < 8) return '';
  return d.slice(0,4) + '-' + d.slice(4,6) + '-' + d.slice(6,8);
}}

function loadMain() {{
  if (!DATA.length) return;
  const it = DATA[cur];
  photo.classList.add('loading');
  const img = new Image();
  img.onload = () => {{ photo.src = img.src; photo.classList.remove('loading'); }};
  img.onerror = () => {{ setTimeout(loadMain, 1200); }};
  img.src = '/img/' + it.i + '.jpg?t=' + Date.now();
}}

function render() {{
  if (!DATA.length) {{
    $('title').textContent = '壁纸列表加载中，几秒后自动刷新…';
    setTimeout(() => location.reload(), 3000);
    return;
  }}
  const it = DATA[cur];
  $('title').textContent = (cur === 0 ? '今天 · ' : cur + ' 天前 · ') + it.t;
  $('date').textContent = fmtDate(it.d);
  $('prev').disabled = cur >= DATA.length - 1;
  $('next').disabled = cur <= 0;
  $('story').disabled = !it.s;
  document.querySelectorAll('.thumb').forEach((el, k) => el.classList.toggle('active', k === cur));
  loadMain();
}}

function go(step) {{
  const next = cur + step;
  if (next < 0 || next >= DATA.length) return;
  cur = next;
  render();
}}

$('prev').onclick = () => go(1);
$('next').onclick = () => go(-1);
document.addEventListener('keydown', e => {{
  if (e.key === 'ArrowRight') go(-1);
  if (e.key === 'ArrowLeft') go(1);
}});
$('story').onclick = () => {{ if (DATA[cur].s) window.open(DATA[cur].s, '_blank'); }};
$('setbtn').onclick = async () => {{
  const b = $('setbtn');
  b.disabled = true; b.textContent = '设置中…';
  try {{
    const r = await fetch('/set/' + DATA[cur].i);
    const j = await r.json();
    // 结果由程序发桌面通知（会自动消失），这里只给按钮短暂反馈，不弹窗
    b.textContent = j.ok ? '✓ 已设为壁纸' : '✗ 设置失败';
  }} catch (e) {{
    b.textContent = '✗ 设置失败';
  }}
  setTimeout(() => {{ b.disabled = false; b.textContent = '设为壁纸'; }}, 1500);
}};

const box = $('thumbs');
DATA.forEach((it, k) => {{
  const im = document.createElement('img');
  im.className = 'thumb';
  im.alt = fmtDate(it.d);
  im.title = (k === 0 ? '今天' : k + ' 天前') + ' · ' + it.t;
  im.src = '/img/' + it.i + '.jpg';
  im.onerror = () => {{ im.style.opacity = '.2'; }};
  im.onclick = () => {{ cur = k; render(); }};
  box.appendChild(im);
}});
render();
</script>
</body>
</html>"#,
        data = data
    )
}
