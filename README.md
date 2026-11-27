# 轻格壁纸 LitheScape

一个为 **deepin V25** 编写的必应每日壁纸托盘程序。Rust 单二进制 + zbus 直连 SNI，不依赖 GTK / AppIndicator，托盘常驻，自动更换壁纸。

- 当前版本：**0.1.5**
- 适用环境：deepin V25、amd64（二进制依赖 GLIBC_2.34 及以上）
- 许可证：[GPL-3.0-or-later](LICENSE)

## 仓库与下载

- Gitee 源码：<https://gitee.com/learcat/lithescape>
- GitHub 源码：<https://github.com/LEARCAT/lithescape>
- deb 下载（Gitee）：<https://gitee.com/learcat/lithescape/releases/latest>
- deb 下载（GitHub）：<https://github.com/LEARCAT/lithescape/releases/latest>

## 功能

- 自动获取并设置必应每日壁纸，按屏幕分辨率自动选图（UHD / 1920x1080 / 1366x768 / 1280x720）
- 托盘菜单：每日壁纸、上一张 / 下一张、壁纸预览、图片故事、开机自启、软件更新、退出
- 壁纸预览：本地大图画廊（浏览器），大图 + 左右切换 + 缩略图导航，可设为壁纸
- 历史归档：必应接口硬上限 8 天，程序把每天拉到的壁纸存档到本地，**用得越久可浏览的历史越多**（最多 365 天）
- 启动时自动禁用 deepin 自带壁纸轮播，退出可用 `--restore-slideshow` 恢复
- 开机自启开关
- **软件更新**：托盘菜单「软件更新」自动检查 Gitee / GitHub Release，发现新版后下载 deb 并安装

## 安装

图形方式：双击 `lithescape_0.1.5_amd64.deb`，用系统安装器安装。

命令行方式：

```bash
sudo dpkg -i lithescape_0.1.5_amd64.deb
```

装前先退出旧实例（`pkill -f lithescape`）。启动器图标若未刷新，注销重登一次。

启动器图标左下角若出现小角标，那是 deepin 启动器的**开机自启标记**（不是安装来源问题）——在托盘菜单关掉「开机自启」即消失。

## 软件更新

启动时会自动静默检查一次：只有发现新版本才弹出系统通知，已是最新或网络不可达时不会打扰。通知只做提示，不自动下载安装。

点托盘菜单「软件更新」，程序会：

1. 同时探测 Gitee 与 GitHub 的最新 Release，取较高版本号；两端都能读到 deb 附件地址
2. 下载 deb 到 `~/.cache/lithescape/`，优先 Gitee 附件，失败回退 GitHub
3. 依次尝试 `sudo -n`（免密静默）→ `pkexec`（弹一次系统密码框）→ `xdg-open`（交给系统安装器）。密码不经过本程序。
4. 通过 `sudo -n` / `pkexec` 装完后**自动重启程序**；若落到系统安装器，则提示安装完成后手动重启（本进程无法感知安装器何时点完）。

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
- `resolution`：**默认 `auto`**，运行时按屏幕宽度自动选图；也可固定为 `UHD` / `1920x1080` / `1366x768` / `1280x720`
- `check_interval`：自动检查间隔（秒），最小 300
- `update_url`：旧版自定义更新源字段，新版软件更新走内置 Gitee / GitHub Release，无需配置

壁纸归档与状态在 `~/.local/state/lithescape/`，大图缓存在 `~/.cache/lithescape/`。
