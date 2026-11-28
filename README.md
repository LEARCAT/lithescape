# 轻格壁纸 LitheScape

一个为 **deepin V25** 编写的必应每日壁纸托盘程序。Rust 单二进制 + zbus 直连 SNI，不依赖 GTK / AppIndicator；壁纸画廊为 **PyQt5 原生窗口**，托盘常驻，自动更换壁纸。

- 当前版本：**0.2.25**
- 适用环境：deepin V25、amd64（二进制依赖 GLIBC 2.34 及以上）
- 许可证：[GPL-3.0-or-later](LICENSE)

## 仓库与下载

- Gitee 源码：<https://gitee.com/learcat/lithescape>
- GitHub 源码：<https://github.com/LEARCAT/lithescape>
- deb 下载（GitHub）：<https://github.com/LEARCAT/lithescape/releases/latest>
- Gitee Release：<https://gitee.com/learcat/lithescape/releases/latest>
  - 注意：Gitee Release 不支持自定义附件，安装包请到 GitHub Release 下载 `lithescape_<版本>_amd64.deb`；软件更新器会双端探测。

## 依赖（重要）

deb 包已在 `Depends` 中声明以下依赖，图形安装器一般会自动处理；若提示缺依赖，按下方命令补齐即可。

| 依赖 | 作用 |
| --- | --- |
| `libc6 (>= 2.34)` | Rust 主程序运行的 glibc 最低版本，deepin V25 已满足 |
| `python3` | 运行内嵌的画廊脚本（主程序首次运行时释放到 `~/.cache/lithescape/`） |
| `python3-pyqt5` | 壁纸画廊的原生窗口与绘制（PyQt5 / Qt5） |
| `xdotool` | 失焦检测兜底：查询当前活动窗口，实现点击画廊外部立即关闭 |

安装依赖（deepin / Debian 系）：

```bash
sudo apt install python3 python3-pyqt5 xdotool
```

> 画廊是常驻的 Python 进程，由 Rust 主程序通过本地 TCP 端口控制（不对外监听）。若 `python3-pyqt5` 未安装，托盘菜单仍可用，但壁纸画廊无法弹出。

## 功能

- **启动时自动切换为必应每日壁纸**（后台执行，不卡托盘；若你当前用的是收藏壁纸则不覆盖）
- 自动获取并设置必应每日壁纸，**默认优先 4K/UHD（3840x2160）**，下载失败自动回退 1920x1080 → 1366x768 → 1280x720；也可在配置里固定分辨率
- **左键点击托盘图标打开壁纸画廊，中键打开图片故事**
- 托盘菜单：每日壁纸、壁纸画廊、壁纸收藏、图片故事、软件更新、退出软件
- **壁纸画廊（PyQt5 原生窗口）**：固定 620×560，弹出定位在屏幕右下角，点击窗口外部立即关闭；大图 + 一行 6 张缩略图，左右箭头切换；日期在预览图左下角、收藏心形在右下角，同一水平线
- **全部壁纸 / 我的收藏两个标签**：每个标签各自记住上次选中的壁纸——在收藏里设的壁纸不会串到全部视图，切回全部视图会恢复你之前在全部里点过的那张
- **壁纸收藏**：每张壁纸一键收藏 / 取消收藏，收藏项独立存 `favorites.json`，不受 365 天历史裁剪影响；保存提示为绿圆白勾图标，停留 1 秒不挡操作
- **收藏自动更换**：「我的收藏」页的「自动更换」勾选框控制开关，右侧时间下拉选择间隔：30秒 / 1分钟 / 5分钟 / 10分钟 / 15分钟 / 30分钟 / 1小时 / 登录时 / 唤醒时。选择即保存。登录时 = 软件启动后换一张；唤醒时 = 点开壁纸画廊面板时换一张（不按计时器高频轮换）。配置存 `fav-slideshow.json`
- 历史归档：必应接口硬上限 8 天，程序把每天拉到的壁纸存档到本地，**用得越久可浏览的历史越多**（最多 365 天）
- 启动时自动禁用 deepin 自带壁纸轮播，退出可用 `--restore-slideshow` 恢复
- **软件更新**：托盘菜单「软件更新」自动检查 Gitee / GitHub Release，发现新版后下载 deb 并安装

> 开机自启：请在启动器里右键「轻格壁纸」图标设置，程序不再接管，避免与系统设置冲突。

## 安装

图形方式：双击 `lithescape_0.2.25_amd64.deb`，用系统安装器安装。

命令行方式：

```bash
sudo dpkg -i lithescape_0.2.25_amd64.deb
sudo apt install -f        # 如提示缺依赖，自动补齐
```

装前先退出旧实例（`pkill -f lithescape`）。启动器图标若未刷新，注销重登一次。

启动器图标左下角若出现小角标，那是 deepin 启动器的**开机自启标记**（不是安装来源问题）——在启动器里右键图标关掉开机自启即消失。

安装 / 升级完成后由 deb 的 postinst 自动拉起托盘；纯命令行安装（无图形会话）会跳过。

## 软件更新

启动时会自动静默检查一次：只有发现新版本才弹出系统通知，已是最新或网络不可达时不会打扰。通知只做提示，不自动下载安装。

点托盘菜单「软件更新」后会**立刻弹出通知**，全程在**同一条通知**上原地更新（不刷屏）：正在检查 → 发现新版本 → 下载进度条（百分比）→ 下载完成、正在安装。随后程序会：

1. 以 Gitee Release 为主读版本号，GitHub 作回退（与 LitheZones 对齐）
2. 下载 deb 到 `~/.cache/lithescape/`，装完即清理，不长期堆积
3. 旧进程先退出释放 D-Bus 单实例锁，再由独立 `--apply-update` 助手进程安装
4. 助手依次尝试 `sudo -n`（免密静默）→ `pkexec`（弹一次系统密码框）→ `xdg-open`（交给系统安装器）。密码不经过本程序。
5. 装完即自动启动由 deb 的 postinst 保证，不依赖旧托盘进程

网络不可达或已是最新版时会给出对应提示，不会卡住托盘。

## 构建

需要 Rust（rustup）、`curl` 与 `gcc`：

```bash
./build.sh        # cargo build --release，产出 ./lithescape
./package.sh      # 从 Cargo.toml 读版本号，打包出 lithescape_<版本>_amd64.deb
```

版本号单一来源为 `Cargo.toml` 的 `version`，`package.sh` 同步 `deb/DEBIAN/control`；`deb/DEBIAN/changelog` 记录各版本变更。

画廊脚本 `src/gallery_qt.py` 通过 `include_str!` 内嵌进二进制，改完 Python 需重新编译才会进入 deb。

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

壁纸归档（`history.json`）、收藏（`favorites.json`）、收藏轮播配置（`fav-slideshow.json`）与状态在 `~/.local/state/lithescape/`，大图与画廊脚本缓存在 `~/.cache/lithescape/`。
