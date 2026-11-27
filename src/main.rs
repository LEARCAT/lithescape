//! LitheScape 轻格壁纸 —— deepin V23/V25 系统托盘程序（Rust / zbus SNI）

mod bing;
mod config;
mod gallery;
mod tray;
mod updater;

use tray::{dispatch_click, BusyState, DbMenu, Shared, SniItem};
use zbus::zvariant::ObjectPath;
use zbus::Connection;

/// 菜单点击 channel：DBus 回调只投递 id，主循环取出来异步处理，
/// 避免耗时操作阻塞 SNI 连接而拖累整个托盘。
type ClickSender = std::sync::Arc<std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<i32>>>>;

/// 版本号单一来源：直接取 Cargo.toml 的 version，避免多处硬编码不同步
/// （参考 LitheZones 的做法）
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// 单实例锁：固定 DBus 名称，重复启动时由 DBus 自动拒绝第二个实例
const SINGLE_INSTANCE_NAME: &str = "com.deepin.LitheScape";

#[zbus::proxy(
    interface = "org.kde.StatusNotifierWatcher",
    default_service = "org.kde.StatusNotifierWatcher",
    default_path = "/StatusNotifierWatcher"
)]
trait StatusNotifierWatcher {
    fn register_status_notifier_item(&self, service: &str) -> zbus::Result<()>;
    #[zbus(property)]
    fn is_status_notifier_host_registered(&self) -> zbus::Result<bool>;
}

#[tokio::main]
async fn main() -> zbus::Result<()> {
    // 命令行参数：--restore-slideshow 恢复系统壁纸轮播后退出
    if std::env::args().any(|a| a == "--restore-slideshow") {
        let st = config::load_state();
        if bing::restore_slideshow(&st.original_slideshow) {
            eprintln!("[info] 已恢复系统壁纸自动更换");
        } else {
            eprintln!("[warn] 无保存的轮播配置，或恢复失败");
        }
        return Ok(());
    }

    let (click_tx, mut click_rx) = tokio::sync::mpsc::unbounded_channel::<i32>();
    let click_tx: ClickSender = std::sync::Arc::new(std::sync::Mutex::new(Some(click_tx)));

    let cfg = config::load_config();
    let shared = Shared {
        cfg: std::sync::Arc::new(std::sync::Mutex::new(cfg)),
        busy: BusyState::default(),
        previews: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        gallery_port: std::sync::Arc::new(std::sync::Mutex::new(0)),
    };

    // 启动本地画廊服务（127.0.0.1 随机端口），供「壁纸预览」打开大图
    match gallery::start(shared.clone()) {
        Ok(port) => {
            if let Ok(mut p) = shared.gallery_port.lock() {
                *p = port;
            }
            eprintln!("[info] 画廊服务已启动于 127.0.0.1:{port}");
        }
        Err(e) => eprintln!("[warn] 画廊服务启动失败: {e}"),
    }

    // 禁用 deepin 系统自带的壁纸自动更换，避免与轻格壁纸冲突
    {
        let original = bing::disable_system_slideshow();
        if !original.is_empty() {
            let mut st = config::load_state();
            st.original_slideshow = original;
            config::save_state(&st);
            eprintln!(
                "[info] 已禁用系统壁纸自动更换（原配置已保存，可用 --restore-slideshow 恢复）"
            );
        }
    }

    let session = Connection::session().await?;

    // 单实例锁：flags=1（DO_NOT_QUEUE），若名称已被占用说明已有实例在运行，直接退出。
    // DBus RequestName 返回 1=成功, 2=已有人占用, 3=排队中。
    let name_reply: u32 = session
        .call_method(
            Some("org.freedesktop.DBus"),
            ObjectPath::try_from("/org/freedesktop/DBus").unwrap(),
            Some("org.freedesktop.DBus"),
            "RequestName",
            &(SINGLE_INSTANCE_NAME, 1u32),
        )
        .await?
        .body()
        .deserialize::<u32>()
        .unwrap_or(0);
    if name_reply == 2 {
        eprintln!("[info] 轻格壁纸已在运行，本次启动退出（避免多实例争抢状态）");
        return Ok(());
    }

    // SNI 服务名要求唯一，通常用 org.kde.StatusNotifierItem-<pid>
    let service_name = format!("org.kde.StatusNotifierItem-{}", std::process::id());
    session
        .call_method(
            Some("org.freedesktop.DBus"),
            ObjectPath::try_from("/org/freedesktop/DBus").unwrap(),
            Some("org.freedesktop.DBus"),
            "RequestName",
            &(service_name.as_str(), 0u32),
        )
        .await?;

    // 注册两个对象：托盘图标 + 菜单
    let item_path = ObjectPath::try_from("/StatusNotifierItem").unwrap();
    let menu_path = ObjectPath::try_from("/Menu").unwrap();
    session
        .object_server()
        .at(item_path.clone(), SniItem::new(shared.clone()))
        .await?;
    session
        .object_server()
        .at(menu_path, DbMenu::new(shared.clone(), click_tx))
        .await?;

    // 通知 watcher（deepin 的 dock 实现了它）
    match StatusNotifierWatcherProxy::new(&session).await {
        Ok(proxy) => {
            if let Err(e) = proxy.register_status_notifier_item(&service_name).await {
                eprintln!("[warn] 注册托盘失败: {e}");
            }
        }
        Err(e) => eprintln!("[warn] 找不到 StatusNotifierWatcher: {e}"),
    }

    tray::spawn_preview_loader(shared.clone());
    tray::spawn_periodic(shared.clone());

    eprintln!("[info] 轻格壁纸托盘已启动 v{VERSION}");

    // 主循环：菜单点击（来自 DBus 回调投递）与 Ctrl+C 一起处理。
    // 点击处理在独立 tokio 任务里同步派发，DBus 回调早已返回，
    // 因此即使壁纸下载/命令执行卡住，也不会阻塞 SNI 连接、不会拖累托盘。
    loop {
        tokio::select! {
            Some(id) = click_rx.recv() => {
                let s = shared.clone();
                tokio::task::spawn_blocking(move || dispatch_click(&s, id));
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[info] 收到退出信号");
                break;
            }
        }
    }
    Ok(())
}
