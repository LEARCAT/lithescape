//! LitheScape 轻格壁纸 —— deepin V25 系统托盘程序（Rust / zbus SNI）

mod bing;
mod config;
mod gallery;
mod tray;
mod updater;

use tray::{dispatch_click, BusyState, DbMenu, Shared, SniItem};
use zbus::zvariant::ObjectPath;
use zbus::Connection;
use std::process::Stdio;

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

/// 拉起当前可执行文件的新实例（setsid 脱离会话，父进程退出后仍存活）。
fn spawn_current_binary() {
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[warn] 自动启动失败：无法定位可执行文件: {e}");
            return;
        }
    };
    if std::process::Command::new("setsid")
        .arg(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
    {
        eprintln!("[info] 已自动启动新版: {}", exe.display());
        return;
    }
    let _ = std::process::Command::new(&exe)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[tokio::main]
async fn main() -> zbus::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // 命令行参数：--restore-slideshow 恢复系统壁纸轮播后退出
    if args.iter().any(|a| a == "--restore-slideshow") {
        let st = config::load_state();
        if bing::restore_slideshow(&st.original_slideshow) {
            eprintln!("[info] 已恢复系统壁纸自动更换");
        } else {
            eprintln!("[warn] 无保存的轮播配置，或恢复失败");
        }
        return Ok(());
    }

    if args.iter().any(|a| a == "--version") {
        println!("lithescape v{VERSION}");
        return Ok(());
    }

    // 更新助手模式：旧进程已退出并释放 D-Bus 名称，这里只负责安装 + 启动新版。
    if let Some(pos) = args.iter().position(|a| a == "--apply-update") {
        if let Some(deb) = args.get(pos + 1) {
            let path = std::path::PathBuf::from(deb);
            let (msg, ok) = updater::apply_downloaded(&path);
            bing::notify("软件更新", &msg);
            if ok {
                spawn_current_binary();
            }
        }
        return Ok(());
    }

    // 诊断：手动跑一次更新检查，打印结果，不下载不安装。
    if args.iter().any(|a| a == "--check-update") {
        match updater::startup_hint(VERSION) {
            Some(msg) => println!("需要更新：{msg}"),
            None => println!("当前已是最新版本 v{VERSION}，无需更新"),
        }
        return Ok(());
    }

    let (click_tx, mut click_rx) = tokio::sync::mpsc::unbounded_channel::<i32>();
    let (update_tx, mut update_rx) = tokio::sync::mpsc::unbounded_channel::<std::path::PathBuf>();
    let click_tx: ClickSender = std::sync::Arc::new(std::sync::Mutex::new(Some(click_tx)));

    let cfg = config::load_config();
    let shared = Shared {
        cfg: std::sync::Arc::new(std::sync::Mutex::new(cfg)),
        busy: BusyState::default(),
        previews: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        gallery_port: std::sync::Arc::new(std::sync::Mutex::new(0)),
        update_tx: Some(update_tx),
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
        .at(item_path.clone(), SniItem::new(shared.clone(), click_tx.clone()))
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
    // 启动即切今日壁纸（后台线程，不阻塞托盘）
    tray::spawn_startup_apply(shared.clone());
    tray::spawn_periodic(shared.clone());
    tray::spawn_favorite_rotation(shared.clone());

    // 启动时静默检查一次更新：只有发现新版本才弹系统通知，
    // 已是最新或网络不可达都不提示。放在独立线程，不阻塞托盘启动。
    tokio::task::spawn_blocking(|| {
        if let Some(hint) = updater::startup_hint(VERSION) {
            bing::notify("软件更新", &hint);
        }
    });

    eprintln!("[info] 轻格壁纸托盘已启动 v{VERSION}");

    // 主循环：菜单点击、更新包下载完成与 Ctrl+C 一起处理。
    // 点击处理在独立 tokio 任务里同步派发，DBus 回调早已返回，
    // 因此即使壁纸下载/命令执行卡住，也不会阻塞 SNI 连接、不会拖累托盘。
    let mut pending_update: Option<std::path::PathBuf> = None;
    loop {
        tokio::select! {
            Some(id) = click_rx.recv() => {
                let s = shared.clone();
                tokio::task::spawn_blocking(move || dispatch_click(&s, id));
            }
            Some(deb) = update_rx.recv() => {
                eprintln!("[info] 更新包已下载，退出旧版后安装: {}", deb.display());
                pending_update = Some(deb);
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("[info] 收到退出信号");
                break;
            }
        }
    }

    // 本进程已准备退出、D-Bus 名称即将释放；现在拉起独立助手安装 deb。
    // 助手装完再启动新版，全程不会与旧实例争抢单实例锁。
    if let Some(deb) = pending_update {
        let exe = std::env::current_exe()
            .unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/lithescape"));
        let mut cmd = std::process::Command::new("setsid");
        cmd.arg(&exe)
            .arg("--apply-update")
            .arg(&deb)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if cmd.spawn().is_err() {
            let _ = std::process::Command::new(&exe)
                .arg("--apply-update")
                .arg(&deb)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn();
        }
        eprintln!("[info] 已拉起更新助手: {}", deb.display());
    }
    Ok(())
}
