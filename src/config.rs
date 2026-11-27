//! 配置与状态持久化

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub update_url: String,
    pub market: String,
    pub resolution: String,
    pub check_interval: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            update_url: String::new(),
            market: "zh-CN".into(),
            resolution: "auto".into(),
            check_interval: 1800,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct State {
    pub current_index: i32,
    pub last_date: String,
    pub last_path: String,
    pub last_title: String,
    pub last_copyright: String,
    pub last_set_at: String,
    pub original_background_uris: Vec<String>,
    /// 启动前的系统壁纸轮播配置（JSON），供恢复用
    pub original_slideshow: String,
    /// 壁纸来源模式：daily=跟随每日壁纸，favorite=用户选定/收藏的壁纸
    pub wallpaper_mode: String,
    /// 每一天的元信息，键为 idx (0..=7)
    pub meta: std::collections::HashMap<String, MetaInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct MetaInfo {
    pub enddate: String,
    pub title: String,
    pub copyright: String,
    pub copyrightlink: String,
    pub quiz: String,
}

/// 历史归档条目：Bing 接口只给最近 8 天，把每天拉到的壁纸存下来，
/// 用得越久历史越长。按 enddate 去重。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HistoryItem {
    pub meta: MetaInfo,
    pub image_url: String,
}

/// 归档最多保留天数，防止无限增长（一年足够）
pub const HISTORY_MAX: usize = 365;

pub fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

pub fn config_path() -> PathBuf {
    home_dir().join(".config/lithescape/config.json")
}

pub fn state_path() -> PathBuf {
    home_dir().join(".local/state/lithescape/state.json")
}

pub fn save_dir() -> PathBuf {
    home_dir().join("Pictures/BingWallpaper")
}

pub fn cache_dir() -> PathBuf {
    home_dir().join(".cache/lithescape/thumbs")
}

/// 历史归档文件路径
pub fn history_path() -> PathBuf {
    home_dir().join(".local/state/lithescape/history.json")
}

pub fn autostart_path() -> PathBuf {
    home_dir().join(".config/autostart/lithescape.desktop")
}

/// 收藏文件路径：与历史归档分开，不受 365 天裁剪影响，长期保留。
pub fn favorites_path() -> PathBuf {
    home_dir().join(".local/state/lithescape/favorites.json")
}

/// 读取收藏列表，按日期倒序（最新在前）。
pub fn load_favorites() -> Vec<HistoryItem> {
    let mut items: Vec<HistoryItem> = fs::read_to_string(favorites_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    items.retain(|h| !h.meta.enddate.is_empty());
    items.sort_by(|a, b| b.meta.enddate.cmp(&a.meta.enddate));
    items
}

pub fn save_favorites(items: &[HistoryItem]) {
    if let Ok(s) = serde_json::to_string_pretty(items) {
        let _ = atomic_write(&favorites_path(), &s);
    }
}

/// 指定日期是否已收藏
pub fn is_favorite(enddate: &str) -> bool {
    load_favorites().iter().any(|h| h.meta.enddate == enddate)
}

/// 当前生效的壁纸是否是用户收藏的壁纸。
/// 依据 state.last_date（设置壁纸时写入的 enddate）判断，
/// 启动与定时检查用它决定是否跳过「每日壁纸」。
pub fn current_is_favorite() -> bool {
    let st = load_state();
    !st.last_date.is_empty() && is_favorite(&st.last_date)
}

/// 当前是否处于「收藏 / 用户选定壁纸」模式。
/// 老版本 state 没有 wallpaper_mode 字段，首次升级时按当前壁纸是否已收藏推断一次。
pub fn is_favorite_mode() -> bool {
    let st = load_state();
    if !st.wallpaper_mode.is_empty() {
        return st.wallpaper_mode == "favorite";
    }
    current_is_favorite()
}

/// 收藏壁纸自动更换配置
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct FavSlideshow {
    pub enabled: bool,
    /// 间隔数量，配合 unit 使用
    pub interval: u64,
    /// 单位：min / hour / day
    pub unit: String,
    /// 上次切换的 Unix 秒（字符串，容错且便于直接查看）
    pub last_switch: String,
    /// 上次切换到的壁纸 enddate，用于确定下一张
    pub last_enddate: String,
}

impl Default for FavSlideshow {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: 30,
            unit: "min".into(),
            last_switch: String::new(),
            last_enddate: String::new(),
        }
    }
}

pub fn fav_slideshow_path() -> PathBuf {
    home_dir().join(".local/state/lithescape/fav-slideshow.json")
}

pub fn load_fav_slideshow() -> FavSlideshow {
    fs::read_to_string(fav_slideshow_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_fav_slideshow(cfg: &FavSlideshow) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = atomic_write(&fav_slideshow_path(), &s);
    }
}

/// 把「数值 + 单位」换算成秒，限制在 1 分钟 ~ 365 天之间
pub fn fav_slideshow_secs(cfg: &FavSlideshow) -> u64 {
    let n = cfg.interval.max(1);
    let unit = match cfg.unit.as_str() {
        "hour" => 3600,
        "day" => 86400,
        _ => 60,
    };
    n.saturating_mul(unit).clamp(60, 365 * 86400)
}

/// 切换收藏状态，返回切换后是否已收藏
pub fn toggle_favorite(item: HistoryItem) -> bool {
    let mut all = load_favorites();
    if let Some(pos) = all.iter().position(|h| h.meta.enddate == item.meta.enddate) {
        all.remove(pos);
        save_favorites(&all);
        false
    } else {
        all.push(item);
        save_favorites(&all);
        true
    }
}

fn atomic_write(path: &Path, content: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, content)?;
    fs::rename(&tmp, path)
}

pub fn load_config() -> Config {
    fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

#[allow(dead_code)]
pub fn save_config(cfg: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(cfg) {
        let _ = atomic_write(&config_path(), &s);
    }
}

pub fn load_state() -> State {
    fs::read_to_string(state_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

pub fn save_state(state: &State) {
    if let Ok(s) = serde_json::to_string_pretty(state) {
        let _ = atomic_write(&state_path(), &s);
    }
}

/// 读取历史归档
pub fn load_history() -> Vec<HistoryItem> {
    fs::read_to_string(history_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// 保存历史归档
pub fn save_history(items: &[HistoryItem]) {
    if let Ok(s) = serde_json::to_string_pretty(items) {
        let _ = atomic_write(&history_path(), &s);
    }
}

/// 把新拉取的壁纸合并进归档：按 enddate 去重，按日期倒序（最新在前），
/// 超过 HISTORY_MAX 的旧记录丢弃。返回是否有变化。
pub fn merge_history(fresh: Vec<HistoryItem>) -> bool {
    let mut all = load_history();
    let before = all.len();
    // 用 enddate 作键：新数据覆盖同日期旧数据
    for item in fresh {
        if item.meta.enddate.is_empty() {
            continue;
        }
        if let Some(old) = all.iter_mut().find(|h| h.meta.enddate == item.meta.enddate) {
            *old = item;
        } else {
            all.push(item);
        }
    }
    // 日期倒序（enddate 是 YYYYMMDD，字符串比较即可）
    all.sort_by(|a, b| b.meta.enddate.cmp(&a.meta.enddate));
    if all.len() > HISTORY_MAX {
        all.truncate(HISTORY_MAX);
    }
    let changed = all.len() != before || all.iter().any(|h| h.meta.enddate.is_empty());
    save_history(&all);
    changed
}

pub fn autostart_enabled() -> bool {
    autostart_path().exists()
}

pub fn set_autostart(enabled: bool) -> bool {
    let path = autostart_path();
    if enabled {
        let content = "[Desktop Entry]\nType=Application\nName=LitheScape | 轻格壁纸\n\
Comment=Bing daily wallpaper tray app for deepin\nExec=/usr/bin/lithescape\n\
Hidden=false\nNoDisplay=true\nX-GNOME-Autostart-enabled=true\n";
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if fs::write(&path, content).is_err() {
            return false;
        }
    } else if path.exists() && fs::remove_file(&path).is_err() {
        return false;
    }
    autostart_enabled() == enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(enddate: &str) -> HistoryItem {
        HistoryItem {
            meta: MetaInfo {
                enddate: enddate.into(),
                title: "晴天".into(),
                copyright: "© test".into(),
                ..Default::default()
            },
            image_url: "https://www.bing.com/th?id=OHR.Sunny_ZH-CN1_UHD.jpg".into(),
        }
    }

    #[test]
    fn favorites_toggle_and_persist() {
        // 用临时 HOME 隔离，避免污染用户真实收藏
        let tmp = std::env::temp_dir().join(format!("lithescape-fav-test-{}", std::process::id()));
        let _ = fs::create_dir_all(&tmp);
        std::env::set_var("HOME", &tmp);

        assert!(load_favorites().is_empty());
        assert!(!is_favorite("20261127"));

        // 收藏 → 落盘 → 再读仍存在
        assert!(toggle_favorite(sample("20261127")));
        assert!(is_favorite("20261127"));
        assert_eq!(load_favorites().len(), 1);

        // 同一天重复收藏不会增加条目
        assert!(!toggle_favorite(sample("20261127")));
        assert_eq!(load_favorites().len(), 0);

        // 不同日期独立存放，按日期倒序
        assert!(toggle_favorite(sample("20261126")));
        assert!(toggle_favorite(sample("20261127")));
        let all = load_favorites();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].meta.enddate, "20261127");

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fav_slideshow_unit_conversion() {
        let cfg = FavSlideshow::default();
        assert_eq!(fav_slideshow_secs(&cfg), 30 * 60);

        let hour = FavSlideshow {
            interval: 2,
            unit: "hour".into(),
            ..Default::default()
        };
        assert_eq!(fav_slideshow_secs(&hour), 2 * 3600);

        let day = FavSlideshow {
            interval: 1,
            unit: "day".into(),
            ..Default::default()
        };
        assert_eq!(fav_slideshow_secs(&day), 86400);

        // 未知单位按分钟处理；0 与超大值被夹到合法范围
        let weird = FavSlideshow {
            interval: 0,
            unit: "???".into(),
            ..Default::default()
        };
        assert_eq!(fav_slideshow_secs(&weird), 60);
        let huge = FavSlideshow {
            interval: 9999,
            unit: "day".into(),
            ..Default::default()
        };
        assert_eq!(fav_slideshow_secs(&huge), 365 * 86400);
    }
}
