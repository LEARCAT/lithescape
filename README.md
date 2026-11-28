# 轻格壁纸 LitheScape

一个为 **deepin V25** 编写的必应每日壁纸托盘程序。Rust 单二进制 + zbus 直连 SNI，不依赖 GTK / AppIndicator，托盘常驻，自动更换壁纸。

- 当前版本：**0.2.11**
- 适用环境：deepin V25、amd64（二进制依赖 GLIBC_2.34 及以上）
- 许可证：[GPL-3.0-or-later](LICENSE)

## 仓库与下载

- Gitee 源码：<https://gitee.com/learcat/lithescape>
- GitHub 源码：<https://github.com/LEARCAT/lithescape>
- deb 下载（Gitee）：<https://gitee.com/learcat/lithescape/releases/latest>
- deb 下载（GitHub）：<https://github.com/LEARCAT/lithescape/releases/latest>

## 功能

- **启动时自动切换为必应每日壁纸**（后台执行，不卡托盘；仅失败时提示）
- 自动获取并设置必应每日壁纸，**默认优先 4K/UHD（3840x2160）**，下载失败自动回退 1920x1080 → 1366x768 → 1280x720；也可在配置里固定分辨率
- **左键点击托盘图标直接打开壁纸预览页面**（中键打开图片故事）
- 托盘菜单：每日壁纸、上一张 / 下一张、壁纸预览、壁纸收藏、图片故事、软件更新、退出
- 壁纸预览：本地大图画廊（浏览器），按「更早 → 今天」排序，左右切换方向与托盘「上一张 / 下一张」一致，大图 + 缩略图导航，可设为壁纸，网页图标为软件自身图标
- **壁纸收藏**：画廊内「全部壁纸 / 我的收藏」两个标签，每张壁纸一键收藏或取消收藏，收藏项置顶、可设为壁纸；收藏单独存 `favorites.json`，不受 365 天历史裁剪影响
- **收藏自动更换**：在「我的收藏」页面与「已收藏」同一行，默认只显示「自动更换」开关，开启后就地展开更换时间设置（分钟 / 小时 / 天）。配置存 `fav-slideshow.json`。当前是收藏壁纸时按间隔轮换，当前是每日壁纸时始终自动使用当日壁纸，两者互不覆盖
- 历史归档：必应接口硬上限 8 天，程序把每天拉到的壁纸存档到本地，**用得越久可浏览的历史越多**（最多 365 天）
- 启动时自动禁用 deepin 自带壁纸轮播，退出可用 `--restore-slideshow` 恢复
- **软件更新**：托盘菜单「软件更新」自动检查 Gitee / GitHub Release，发现新版后下载 deb 并安装

> 开机自启：请在启动器里右键「轻格壁纸」图标设置，程序不再接管，避免与系统设置冲突。

## 安装

图形方式：双击 `lithescape_0.2.11_amd64.deb`，用系统安装器安装。

命令行方式：

```bash
sudo dpkg -i lithescape_0.2.11_amd64.deb
```

装前先退出旧实例（`pkill -f lithescape`）。启动器图标若未刷新，注销重登一次。

启动器图标左下角若出现小角标，那是 deepin 启动器的**开机自启标记**（不是安装来源问题）——在启动器里右键图标关掉开机自启即消失。

## 软件更新

启动时会自动静默检查一次：只有发现新版本才弹出系统通知，已是最新或网络不可达时不会打扰。通知只做提示，不自动下载安装。

点托盘菜单「软件更新」后会**立刻弹出通知**，全程在**同一条通知**上原地更新（不刷屏）：正在检查 → 发现新版本 → 下载进度条（百分比）→ 下载完成、正在安装。每一步都可见，不会出现"点了没反应"的错觉。随后程序会：

1. 以 Gitee Release 为主读版本号和 deb 地址，GitHub 只作双保险回退（与 LitheZones 对齐）
2. 下载 deb 到 `~/.cache/lithescape/`
3. **旧进程先退出**释放 D-Bus 单实例锁，再由独立 `--apply-update` 助手进程安装
4. 助手依次尝试 `sudo -n`（免密静默）→ `pkexec`（弹一次系统密码框）→ `xdg-open`（交给系统安装器）。密码不经过本程序。
5. **装完即自动启动由 deb 的 postinst 保证**：无论走 sudo / pkexec / 系统安装器，只要 dpkg 完成安装就按图形会话环境拉起新版托盘，不依赖旧托盘进程或助手代码版本（助手的等待逻辑作为补充兜底）。

网络不可达或已是最新版时会给出对应提示，不会卡住托盘。

## 构建

需要 Rust（rustup）与 `curl`：

```bash
./build.sh        # cargo build --release，产出 ./lithescape
./package.sh      # 从 Cargo.toml 读版本号，打包出 lithescape_<版本>_amd64.deb
```

版本号单一来源为 `Cargo.toml` 的 `version`，`package.sh` 自动同步 `deb/DEBIAN/control`；`deb/DEBIAN/changelog` 记录各版本变更。

受限环境（如安全策略拦截 cargo 设置 mtime）下 `build.sh` 会自动启用 `nomtime.c` 时间戳兼容层，正常环境无害。

## 配置

配置文件位于 `~/.config/lithescape/config.json`：

```json
{
  "market": "zh-CN",
  "resolution": "auto",
  "check_interval": 1800,
  "update_url": ""
}
```

- `market`：必应市场，如 `zh-CN`、`en-US`
- `resolution`：**默认 `auto` = 优先 4K/UHD**；也可固定为 `4K` / `UHD` / `1920x1080` / `1366x768` / `1280x720`
- `check_interval`：自动检查间隔（秒），最小 300
- `update_url`：旧版自定义更新源字段，新版软件更新走内置 Gitee / GitHub Release，无需配置

壁纸归档（`history.json`）、收藏（`favorites.json`）与状态在 `~/.local/state/lithescape/`，大图缓存在 `~/.cache/lithescape/`。
