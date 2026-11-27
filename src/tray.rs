//! SNI (StatusNotifierItem) 托盘 + DBusMenu 菜单
//!
//! 直接使用 zbus 实现标准协议，不依赖 GTK / AppIndicator。
//! deepin V25 的 dock 同时支持 SNI，图标会在托盘区显示。

use crate::config::{
    autostart_enabled, fav_slideshow_secs, is_favorite_mode, load_fav_slideshow, load_favorites,
    load_state, save_fav_slideshow, set_autostart, Config, MetaInfo, State,
};
use crate::{bing, VERSION};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use zbus::interface;
use zbus::zvariant::{ObjectPath, OwnedValue, Value};
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

// 菜单项 ID
const ID_NOW: i32 = 1;
const ID_PREV: i32 = 2;
const ID_NEXT: i32 = 3;
const ID_STORY: i32 = 4;
const ID_AUTO: i32 = 7;
const ID_UPDATE: i32 = 8;
const ID_QUIT: i32 = 9;
const ID_PREVIEW: i32 = 10; // 壁纸预览
const ID_FAVORITES: i32 = 11; // 壁纸收藏
const ID_FAV_LOOP: i32 = 12; // 收藏自动更换
const SEP_1: i32 = 101;
const SEP_2: i32 = 102;
const SEP_3: i32 = 103;
const SEP_4: i32 = 104;

/// 一条预览记录
#[derive(Clone)]
pub struct PreviewItem {
    pub index: i32,
    pub title: String,
    pub date: String,
    pub image_url: String,
    pub meta: MetaInfo,
}

/// 忙碌状态的最大持续时间。超过后强制复位，防止任何意外（线程 panic、
/// 外部命令无响应、异常退出）导致 busy 永久为 true、菜单永久变灰。
const BUSY_STUCK_TIMEOUT: Duration = Duration::from_secs(90);

/// 忙碌状态：记录开始时间，超时自动失效。
#[derive(Clone, Default)]
pub struct BusyState {
    since: Arc<Mutex<Option<Instant>>>,
}

impl BusyState {
    pub fn set(&self, v: bool) {
        if let Ok(mut s) = self.since.lock() {
            *s = if v { Some(Instant::now()) } else { None };
        }
    }

    pub fn is_set(&self) -> bool {
        let mut s = match self.since.lock() {
            Ok(s) => s,
            Err(e) => e.into_inner(),
        };
        match *s {
            Some(t) if t.elapsed() > BUSY_STUCK_TIMEOUT => {
                eprintln!("[warn] 忙碌状态超时，已强制复位");
                *s = None;
                false
            }
            Some(_) => true,
            None => false,
        }
    }
}

/// RAII 守卫：无论正常结束、提前返回还是 panic，离开作用域都清除 busy。
pub struct BusyGuard(Shared);

impl Drop for BusyGuard {
    fn drop(&mut self) {
        self.0.set_busy(false);
    }
}

#[derive(Clone)]
pub struct Shared {
    pub cfg: Arc<Mutex<Config>>,
    pub busy: BusyState,
    pub previews: Arc<Mutex<Vec<PreviewItem>>>,
    pub gallery_port: Arc<Mutex<u16>>,
    /// 更新包下载完成后回传给主循环，主循环退出后再交给助手安装。
    pub update_tx: Option<UnboundedSender<PathBuf>>,
}

impl Shared {
    pub fn set_busy(&self, v: bool) {
        self.busy.set(v);
    }
    pub fn is_busy(&self) -> bool {
        self.busy.is_set()
    }
    /// 进入忙碌状态；若已在忙碌中返回 false（避免重复任务）。
    pub fn begin_busy(&self) -> bool {
        if self.is_busy() {
            return false;
        }
        self.set_busy(true);
        true
    }
    pub fn busy_guard(&self) -> BusyGuard {
        BusyGuard(self.clone())
    }
}

fn menu_item(id: i32, label: &str, enabled: bool, toggle: Option<bool>) -> OwnedValue {
    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    props.insert("label".into(), Value::new(label).try_to_owned().unwrap());
    props.insert(
        "type".into(),
        Value::new("standard").try_to_owned().unwrap(),
    );
    props.insert(
        "enabled".into(),
        Value::new(enabled).try_to_owned().unwrap(),
    );
    if let Some(t) = toggle {
        props.insert(
            "toggle-state".into(),
            Value::new(if t { 1i32 } else { 0i32 })
                .try_to_owned()
                .unwrap(),
        );
    }
    Value::new((id, props, Vec::<OwnedValue>::new()))
        .try_to_owned()
        .unwrap()
}

fn separator(id: i32) -> OwnedValue {
    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    props.insert(
        "type".into(),
        Value::new("separator").try_to_owned().unwrap(),
    );
    Value::new((id, props, Vec::<OwnedValue>::new()))
        .try_to_owned()
        .unwrap()
}

fn menu_layout() -> (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>) {
    let state: State = load_state();
    let idx = state.current_index;
    let autostart = autostart_enabled();
    let fav_loop = load_fav_slideshow().enabled;
    // 翻页边界由本地归档实际天数决定（Bing 只给 8 天，归档会随时间增长）
    let total = crate::config::load_history().len() as i32;
    let max_idx = (total - 1).max(0);

    // 不因 busy 把整组操作置灰：Qt 菜单在项被禁用时会忽略点击，
    // 用户感知就是「托盘点击没反应」。改为全部可用，由 handle_click
    // 内部提示「正在处理」，busy 也有 90 秒超时兜底。
    let children = vec![
        menu_item(ID_NOW, "每日壁纸", true, None),
        menu_item(ID_PREV, "上一张", idx < max_idx, None),
        menu_item(ID_NEXT, "下一张", idx > 0, None),
        separator(SEP_1),
        menu_item(ID_PREVIEW, "壁纸预览", true, None),
        menu_item(ID_FAVORITES, "壁纸收藏", true, None),
        menu_item(ID_FAV_LOOP, "收藏自动更换", true, Some(fav_loop)),
        separator(SEP_4),
        menu_item(ID_STORY, "图片故事", true, None),
        separator(SEP_2),
        menu_item(ID_AUTO, "开机自启", true, Some(autostart)),
        menu_item(ID_UPDATE, "软件更新", true, None),
        separator(SEP_3),
        menu_item(ID_QUIT, "退出", true, None),
    ];

    let mut props: HashMap<String, OwnedValue> = HashMap::new();
    props.insert(
        "type".into(),
        Value::new("standard").try_to_owned().unwrap(),
    );
    (0, props, children)
}

pub struct SniItem {
    #[allow(dead_code)]
    shared: Shared,
    /// 菜单/托盘点击统一投递给主循环，绝不在 DBus 回调里做耗时操作
    click_tx: ClickSender,
}

impl SniItem {
    pub fn new(shared: Shared, click_tx: ClickSender) -> Self {
        Self { shared, click_tx }
    }
}

#[interface(name = "org.kde.StatusNotifierItem")]
impl SniItem {
    #[zbus(property, name = "Category")]
    fn category(&self) -> String {
        "ApplicationStatus".into()
    }

    #[zbus(property, name = "Id")]
    fn id(&self) -> String {
        "lithescape".into()
    }

    #[zbus(property, name = "Title")]
    fn title(&self) -> String {
        let state: State = load_state();
        if state.last_title.is_empty() {
            "LitheScape 轻格壁纸".into()
        } else {
            format!("轻格壁纸 - {}", state.last_title)
        }
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        "Active".into()
    }

    #[zbus(property, name = "IconName")]
    fn icon_name(&self) -> String {
        "lithescape".into()
    }

    /// 内嵌 48x48 ARGB32 像素，绕过图标主题缓存问题，确保 dock 一定能显示
    #[zbus(property, name = "IconPixmap")]
    fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
        const ARGB: &[u8] = include_bytes!("../icon-48.argb");
        vec![(48, 48, ARGB.to_vec())]
    }

    // IconName + IconPixmap 双保险：主题可find则用主题，否则用内嵌像素

    #[zbus(property, name = "Menu")]
    fn menu(&self) -> ObjectPath<'static> {
        ObjectPath::try_from("/Menu").unwrap()
    }

    #[zbus(property, name = "ItemIsMenu")]
    fn item_is_menu(&self) -> bool {
        false
    }

    #[zbus(property, name = "ToolTip")]
    fn tool_tip(
        &self,
    ) -> (
        String,
        Vec<(i32, i32, i32, i32)>,
        String,
        HashMap<String, OwnedValue>,
    ) {
        let state: State = load_state();
        (
            "lithescape".into(),
            vec![(0, 0, 0, 0)],
            if state.last_title.is_empty() {
                "LitheScape 轻格壁纸".into()
            } else {
                state.last_title
            },
            HashMap::new(),
        )
    }

    /// 左键点击托盘 —— 打开壁纸预览（投递给主循环异步处理，不阻塞 SNI 连接）
    fn activate(&self, _x: i32, _y: i32) {
        if let Some(tx) = self.click_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ID_PREVIEW);
        }
    }

    /// 中键点击托盘 —— 打开图片故事（同样走主循环，避免回调内耗时）
    fn secondary_activate(&self, _x: i32, _y: i32) {
        if let Some(tx) = self.click_tx.lock().unwrap().as_ref() {
            let _ = tx.send(ID_STORY);
        }
    }

    fn context_menu(&self, _x: i32, _y: i32) {
        // DBusMenu 由客户端通过 Menu 属性拉取，无需额外动作
    }

    #[zbus(signal)]
    async fn new_title(signal_ctxt: &zbus::object_server::SignalContext<'_>) -> zbus::Result<()>;
}

/// 菜单点击发送端：由 event() 持有，只负责投递 id，绝不在 DBus 回调里执行业务逻辑。
type ClickSender = Arc<Mutex<Option<UnboundedSender<i32>>>>;

pub struct DbMenu {
    click_tx: ClickSender,
}

impl DbMenu {
    pub fn new(_shared: Shared, click_tx: ClickSender) -> Self {
        Self { click_tx }
    }
}

/// 在主 tokio 循环里处理菜单点击。
/// 与 DBus 回调解耦：所有耗时操作（网络、命令、文件 IO）都在这里或子线程执行，
/// 即使卡住也不会阻塞 SNI 连接，不会拖累整个托盘。
pub fn dispatch_click(shared: &Shared, id: i32) {
    let state: State = load_state();
    let idx = state.current_index;
    let cfg = shared.cfg.lock().map(|c| c.clone()).unwrap_or_default();
    let sh = shared.clone(); // owned 副本，供各子线程使用

    match id {
        ID_NOW => {
            if !shared.begin_busy() {
                bing::notify("请稍候", "正在更换壁纸，完成后再试");
                return;
            }
            std::thread::spawn(move || {
                let _guard = sh.busy_guard();
                match bing::apply(0, true, &cfg) {
                    Ok(msg) => bing::notify("壁纸已更新", &msg),
                    Err(e) => bing::notify("更换失败", &e),
                }
            });
        }
        ID_PREV if idx < crate::config::load_history().len() as i32 - 1 => {
            if !shared.begin_busy() {
                bing::notify("请稍候", "正在切换壁纸，完成后再试");
                return;
            }
            std::thread::spawn(move || {
                let _guard = sh.busy_guard();
                match bing::apply(idx + 1, false, &cfg) {
                    Ok(msg) => bing::notify("已切换到上一张", &msg),
                    Err(e) => bing::notify("切换失败", &e),
                }
            });
        }
        ID_NEXT if idx > 0 => {
            if !shared.begin_busy() {
                bing::notify("请稍候", "正在切换壁纸，完成后再试");
                return;
            }
            std::thread::spawn(move || {
                let _guard = sh.busy_guard();
                match bing::apply(idx - 1, false, &cfg) {
                    Ok(msg) => bing::notify("已切换到下一张", &msg),
                    Err(e) => bing::notify("切换失败", &e),
                }
            });
        }
        ID_PREVIEW | ID_FAVORITES => {
            let favorites = id == ID_FAVORITES;
            let label = if favorites { "壁纸收藏" } else { "壁纸预览" };
            let port = shared.gallery_port.lock().map(|p| *p).unwrap_or(0);
            if port == 0 {
                bing::notify(label, "画廊服务未启动，请重启程序");
                return;
            }
            let items = shared
                .previews
                .lock()
                .map(|p| p.clone())
                .unwrap_or_default();
            if items.is_empty() {
                bing::notify(label, "壁纸列表加载中，请稍后再点一次");
            }
            // 后台预热大图：收藏页要显示所有壁纸（含已收藏），因此同样预热全部，
            // 打开后页面按标签即时切换，无需等待网络。
            std::thread::spawn(move || {
                for item in &items {
                    let _ = crate::gallery::ensure_large(item);
                }
            });
            crate::gallery::open(port, favorites);
        }
        ID_STORY => {
            if let Some(meta) = state.meta.get(&idx.to_string()) {
                bing::open_url(&meta.copyrightlink);
            }
        }
        ID_FAV_LOOP => {
            let mut fav_cfg = load_fav_slideshow();
            let enabling = !fav_cfg.enabled;
            fav_cfg.enabled = enabling;
            // 开启时清空计时，让轮播线程在数秒内先切到一张收藏壁纸
            if enabling {
                fav_cfg.last_switch.clear();
            }
            save_fav_slideshow(&fav_cfg);
            if enabling && load_favorites().is_empty() {
                bing::notify(
                    "收藏自动更换",
                    "已开启，但还没有收藏壁纸；请先在「壁纸收藏」中添加",
                );
            } else {
                if enabling {
                    start_favorite_rotation();
                }
                bing::notify(
                    "收藏自动更换",
                    if enabling {
                        "已开启，将按设定间隔更换收藏壁纸"
                    } else {
                        "已关闭"
                    },
                );
            }
        }
        ID_AUTO => {
            let current = autostart_enabled();
            if !set_autostart(!current) {
                bing::notify("设置失败", "无法修改开机自启");
            } else {
                bing::notify("开机自启", if current { "已关闭" } else { "已开启" });
            }
        }
        ID_UPDATE => {
            let tx = shared.update_tx.clone();
            std::thread::spawn(move || check_update(&cfg, tx));
        }
        ID_QUIT => {
            std::process::exit(0);
        }
        _ => {}
    }
}

#[interface(name = "com.canonical.dbusmenu")]
impl DbMenu {
    #[zbus(property, name = "Version")]
    fn version(&self) -> u32 {
        3
    }

    #[zbus(property, name = "TextDirection")]
    fn text_direction(&self) -> String {
        "ltr".into()
    }

    #[zbus(property, name = "Status")]
    fn status(&self) -> String {
        "normal".into()
    }

    #[zbus(out_args("revision", "layout"))]
    fn get_layout(
        &self,
        parent_id: i32,
        _recursion_depth: i32,
        _property_names: Vec<String>,
    ) -> (u32, (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>)) {
        if parent_id != 0 {
            return (1, (parent_id, HashMap::new(), Vec::new()));
        }
        (1, menu_layout())
    }

    /// DBusMenu Event：只把点击 id 投递给主循环，立即返回。
    ///
    /// 关键：绝不能在这个 DBus 回调里做下载壁纸、执行命令、读写文件等耗时操作，
    /// 否则会阻塞整个 SNI 连接；dock 轮询这个连接时超时，会拖累整个托盘区。
    /// 参考 LitheZones 的做法：回调里只发 channel，业务逻辑交给主 tokio 循环。
    fn event(
        &self,
        id: i32,
        event_id: &str,
        _data: OwnedValue,
        _timestamp: u32,
    ) -> zbus::fdo::Result<bool> {
        if event_id == "clicked" {
            if let Some(tx) = self.click_tx.lock().unwrap().as_ref() {
                let _ = tx.send(id);
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn about_to_show(&self, id: i32) -> zbus::fdo::Result<bool> {
        Ok(id == 0)
    }

    #[zbus(signal)]
    async fn layout_updated(
        signal_ctxt: &zbus::object_server::SignalContext<'_>,
        revision: u32,
        parent: i32,
    ) -> zbus::Result<()>;
}

/// 软件更新：检查 + 下载，只发头尾两条通知，最后把 deb 路径回传主循环。
/// 旧进程随后退出释放 D-Bus 名称，由 --apply-update 助手安装。
///
/// deepin V25 的通知中心不渲染 value 进度条，且 replaces_id 不会原地替换，
/// 逐百分比推送会堆一长串通知卡片。因此这里只发两条：
///   头：「正在检查更新…」点击立即反馈
///   尾：下载完成 / 已是最新 / 失败，一条结果
/// 下载过程静默，不弹通知。
fn check_update(_cfg: &Config, update_tx: Option<UnboundedSender<PathBuf>>) {
    // 1) 点击即反馈：立刻弹「正在检查更新」，避免用户以为没反应
    let id = bing::notify_progress("软件更新", "正在检查更新…", 0, 0).unwrap_or(0);

    // 2) 下载阶段静默，不逐百分比推送通知
    let (msg, deb) = crate::updater::prepare_update(VERSION, None);

    // 3) 只发一条结果通知（标准桌面替换头那条；deepin 上呈现为头尾两条）
    let final_msg = if deb.is_some() {
        format!("{msg}，正在安装…")
    } else {
        msg
    };
    let _ = bing::notify_progress("软件更新", &final_msg, 100, id);

    if let Some(path) = deb {
        if let Some(tx) = update_tx {
            let _ = tx.send(path);
        }
    }
}

/// 后台预加载壁纸元数据并定时刷新（大图在用户打开画廊时才下载）。
/// Bing 接口只给最近 8 天，这里把拉到的数据合并进本地归档，
/// 画廊数据源 = 归档全部历史，因此用得越久可浏览的天数越多。
pub fn spawn_preview_loader(shared: Shared) {
    std::thread::spawn(move || loop {
        let cfg = shared.cfg.lock().map(|c| c.clone()).unwrap_or_default();
        // Bing 单次上限 8 条，取满
        if let Ok(recent) = bing::fetch_recent(&cfg, 8) {
            let fresh: Vec<crate::config::HistoryItem> = recent
                .into_iter()
                .map(|(meta, image_url)| crate::config::HistoryItem { meta, image_url })
                .collect();
            crate::config::merge_history(fresh);
        }
        // 从归档重建预览列表：按日期倒序，index 0 为最新
        let history = crate::config::load_history();
        let items: Vec<PreviewItem> = history
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
        if !items.is_empty() {
            if let Ok(mut p) = shared.previews.lock() {
                *p = items;
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(3600));
    });
}

/// 启动时执行一次：把壁纸切换为必应每日壁纸（index 0）。
/// 但用户当前用的是收藏壁纸、或已开启收藏自动更换时，绝不覆盖用户的选择。
/// 放在独立线程，不阻塞托盘注册；用 busy 锁避免与用户点击/定时器冲突。
/// 成功不弹通知（静默完成），失败才提示用户。
pub fn spawn_startup_apply(shared: Shared) {
    std::thread::spawn(move || {
        // 让预览加载器先把元数据/历史归档建好，启动换壁纸随后进行
        std::thread::sleep(std::time::Duration::from_secs(1));
        if is_favorite_mode() {
            eprintln!("[info] 当前为用户选定 / 收藏壁纸，启动不切换每日壁纸");
            return;
        }
        // 当前是每日壁纸（或首次运行）：自动切到当日壁纸。
        // 收藏轮播线程只在当前为收藏壁纸时才动作，不会覆盖这里。
        if !shared.begin_busy() {
            return;
        }
        let _guard = shared.busy_guard();
        let cfg = shared.cfg.lock().map(|c| c.clone()).unwrap_or_default();
        match bing::apply(0, false, &cfg) {
            Ok(_) => eprintln!("[info] 启动已切换为每日壁纸"),
            Err(e) => bing::notify("轻格壁纸", &format!("启动时设置壁纸失败：{e}")),
        }
    });
}

/// 开启收藏自动更换时立即切到一张收藏壁纸。
/// 必须先进入「收藏壁纸」状态，轮播线程才会接管；否则用户当前还是每日壁纸，
/// 轮播线程按设计不动作，开启后要等到用户手动收藏一张才生效。
pub fn start_favorite_rotation() {
    let favorites = load_favorites();
    if favorites.is_empty() {
        return;
    }
    let mut fav_cfg = load_fav_slideshow();
    let pos = favorites
        .iter()
        .position(|h| h.meta.enddate == fav_cfg.last_enddate)
        .map(|p| (p + 1) % favorites.len())
        .unwrap_or(0);
    let target = favorites[pos].clone();
    match bing::apply_favorite(&target) {
        Ok(_) => {
            fav_cfg.last_switch = chrono::Local::now().timestamp().to_string();
            fav_cfg.last_enddate = target.meta.enddate.clone();
        }
        Err(e) => {
            // 失败时清空计时，下一轮尽快重试
            fav_cfg.last_switch.clear();
            bing::notify("收藏轮播失败", &e);
        }
    }
    save_fav_slideshow(&fav_cfg);
}

/// 收藏壁纸自动更换：按用户设定的「数值 + 分钟/小时/天」间隔轮换已收藏壁纸。
/// 每 5 秒检查一次到期情况，避免开启后还要等一整个周期才生效。
pub fn spawn_favorite_rotation(shared: Shared) {
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(5));
        let mut fav_cfg = load_fav_slideshow();
        if !fav_cfg.enabled {
            continue;
        }
        // 只在「当前是收藏壁纸」时轮换；用户选用每日壁纸时保持当日壁纸，
        // 不会被收藏轮播覆盖（对应：每日壁纸 = 自动使用当日壁纸）。
        if !is_favorite_mode() {
            continue;
        }
        let favorites = load_favorites();
        if favorites.is_empty() {
            continue;
        }
        let now = chrono::Local::now().timestamp();
        let last = fav_cfg.last_switch.parse::<i64>().unwrap_or(0);
        // 从未切换过（刚开启）立即执行；否则按到期时间判断
        if last > 0 && now.saturating_sub(last) < fav_slideshow_secs(&fav_cfg) as i64 {
            continue;
        }
        if !shared.begin_busy() {
            continue;
        }
        let _guard = shared.busy_guard();

        let pos = if fav_cfg.last_enddate.is_empty() {
            0
        } else {
            favorites
                .iter()
                .position(|h| h.meta.enddate == fav_cfg.last_enddate)
                .map(|p| (p + 1) % favorites.len())
                .unwrap_or(0)
        };
        let target = favorites[pos].clone();
        // 无论成功失败都记录本次尝试，避免失败时每 5 秒反复重试、刷屏通知
        fav_cfg.last_switch = now.to_string();
        match bing::apply_favorite(&target) {
            Ok(msg) => {
                fav_cfg.last_enddate = target.meta.enddate.clone();
                save_fav_slideshow(&fav_cfg);
                eprintln!("[info] 收藏轮播已切换：{msg}");
            }
            Err(e) => {
                save_fav_slideshow(&fav_cfg);
                bing::notify("收藏轮播失败", &e);
            }
        }
    });
}

/// 定时检查新壁纸
pub fn spawn_periodic(shared: Shared) {
    std::thread::spawn(move || {
        loop {
            let interval = shared
                .cfg
                .lock()
                .map(|c| c.check_interval.max(300))
                .unwrap_or(1800);
            // 分段睡眠：让 Ctrl+C / 退出能及时响应，避免长 sleep 拖住线程
            let mut waited = 0u64;
            while waited < interval {
                let step = interval.saturating_sub(waited).min(5);
                std::thread::sleep(std::time::Duration::from_secs(step));
                waited += step;
            }
            if shared.is_busy() {
                continue;
            }
            let state: State = load_state();
            // 用户正在用收藏壁纸时不跟随每日更新；只有停留在"今天"
            // 且不是收藏壁纸时，才自动跟随新的一天
            if state.current_index != 0 || is_favorite_mode() {
                continue;
            }
            if !shared.begin_busy() {
                continue;
            }
            let _guard = shared.busy_guard();
            let cfg = shared.cfg.lock().map(|c| c.clone()).unwrap_or_default();
            match bing::apply(0, false, &cfg) {
                Ok(msg) => bing::notify("壁纸已更新", &msg),
                Err(e) => bing::notify("检查更新失败", &e),
            }
        }
    });
}
