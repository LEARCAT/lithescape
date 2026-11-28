# -*- coding: utf-8 -*-
"""LitheScape 壁纸画廊原生窗口（PyQt5）。

由 Rust 主程序释放到 ~/.cache/lithescape/ 后以 python3 启动，常驻运行。
数据/图片/设壁纸/收藏等全部走本地 HTTP 后端（Rust 提供）；本脚本只负责 UI。
视觉样式对齐早期浏览器版（gallery.rs 内嵌 HTML/CSS），配色/圆角/阴影/对齐一致。

进程间通信：监听 127.0.0.1:<ctrl_port> 的 QTcpServer，Rust 点托盘时发一行命令：
    show\tall      显示并切到「全部壁纸」
    show\tfav      显示并切到「我的收藏」
    quit           退出进程
窗口失焦即 hide()（保留图片在内存），再点托盘 show() 唤回，毫秒级。
"""
import sys
import json
import time
import urllib.request
import subprocess
from PyQt5.QtCore import Qt, QTimer, QUrl, QRect, QRectF, QPoint, QPointF, QEvent, pyqtSignal
from PyQt5.QtGui import (
    QPixmap, QColor, QDesktopServices, QIcon,
    QPainter, QPainterPath, QPen, QBrush, QFont,
)
from PyQt5.QtWidgets import (
    QApplication, QWidget, QLabel, QPushButton, QHBoxLayout, QVBoxLayout,
    QScrollArea, QGraphicsDropShadowEffect, QComboBox, QFrame,
)
from PyQt5.QtNetwork import QTcpServer, QHostAddress

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 0
CTRL_PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 28765
BASE = "http://127.0.0.1:%d" % PORT

LIGHT = {
    "bg": "#eef1f4", "surface": "#ffffff", "text": "#1f2328", "muted": "#6b7280",
    "border": "#dfe3e8", "accent": "#2a7f9e", "accent2": "#1a8fb0",
    "btn": "#f4f6f8", "btn_hover": "#e7ebef", "thumb": "#e8ecf0",
    "navbg": (255, 255, 255, 220),
    "shadow": (20, 30, 45, 42),
    "fav": "#b8860b", "favbg": "#fff6d9", "favborder": "#e8c766",
    "ok": "#16a34a",
}
DARK = {
    "bg": "#111318", "surface": "#1b1e24", "text": "#e8eaed", "muted": "#9aa0a8",
    "border": "#2e323a", "accent": "#107c9e", "accent2": "#1a8fb0",
    "btn": "#262a31", "btn_hover": "#333842", "thumb": "#2a2e35",
    "navbg": (24, 26, 31, 210),
    "shadow": (0, 0, 0, 150),
    "fav": "#ffd166", "favbg": "#3a321f", "favborder": "#8a6d2f",
    "ok": "#4ade80",
}

# 宽度与旧版一致保持 620；自绘标题栏占 40px，高度补回 40px，
# 让大图恢复到旧版「宽度铺满、左右只留 10px」的比例，默认显示 6 张缩略图。
WIN_W, WIN_H = 620, 560
# 与旧版浏览器画廊完全一致的小窗左右留白。
GUTTER = 10
# 缩略图行：旧版为 92px 左右、间距 10px，600px 可用宽度正好 6 张。
THUMB_GAP = 10
THUMB_IDEAL = 100
# 自动更换：勾选框单独控制开关，下拉只保留时间预设，不再塞「从不」。
# (显示文字, 间隔, 单位)
FOV_PRESETS = [
    ("30秒", 30, "sec"),
    ("1分钟", 1, "min"),
    ("5分钟", 5, "min"),
    ("10分钟", 10, "min"),
    ("15分钟", 15, "min"),
    ("30分钟", 30, "min"),
    ("1小时", 1, "hour"),
    ("登录时", 0, "login"),
    ("唤醒时", 0, "wake"),
]
def http_get(path, timeout=6):
    with urllib.request.urlopen(BASE + path, timeout=timeout) as r:
        return r.read()


def http_json(path, timeout=6):
    return json.loads(http_get(path, timeout).decode("utf-8"))


def heart_shape(size):
    """把 SVG 心形路径按 size 缩放成 QPainterPath（24x24 viewBox）。"""
    s = size / 24.0
    p = QPainterPath()
    p.moveTo(12 * s, 21.35 * s)
    p.lineTo(10.55 * s, 20.03 * s)
    p.cubicTo(5.4 * s, 15.36 * s, 2 * s, 12.28 * s, 2 * s, 8.5 * s)
    p.cubicTo(2 * s, 5.42 * s, 4.42 * s, 3 * s, 7.5 * s, 3 * s)
    p.cubicTo(9.24 * s, 3 * s, 10.91 * s, 3.81 * s, 12 * s, 5.09 * s)
    p.cubicTo(13.09 * s, 3.81 * s, 14.76 * s, 3 * s, 16.5 * s, 3 * s)
    p.cubicTo(19.58 * s, 3 * s, 22 * s, 5.42 * s, 22 * s, 8.5 * s)
    p.cubicTo(22 * s, 12.28 * s, 18.6 * s, 15.36 * s, 13.45 * s, 20.03 * s)
    p.lineTo(12 * s, 21.35 * s)
    p.closeSubpath()
    return p


def rounded_pixmap(pm, radius):
    """生成带透明圆角的 QPixmap（供 QGraphicsDropShadowEffect 按 alpha 投影）。"""
    out = QPixmap(pm.size())
    out.fill(Qt.transparent)
    p = QPainter(out)
    p.setRenderHint(QPainter.Antialiasing)
    p.setRenderHint(QPainter.SmoothPixmapTransform)
    path = QPainterPath()
    path.addRoundedRect(QRectF(0, 0, pm.width(), pm.height()), radius, radius)
    p.setClipPath(path)
    p.drawPixmap(0, 0, pm)
    p.end()
    return out


class NavButton(QPushButton):
    """紧凑圆形箭头：36px 半透明圆 + 16px SVG chevron，边界处整体灰透。"""

    def __init__(self, left, parent=None):
        super().__init__(parent)
        self.left = left
        self.hover = False
        self.setFixedSize(36, 36)
        self.setCursor(Qt.PointingHandCursor)
        self.setToolTip("上一张" if left else "下一张")
        self._c = LIGHT

    def set_colors(self, c):
        self._c = c
        self.update()

    def enterEvent(self, e):
        self.hover = True
        self.update()
        super().enterEvent(e)

    def leaveEvent(self, e):
        self.hover = False
        self.update()
        super().leaveEvent(e)

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        # 和旧版浏览器 .nav:disabled 一致：到头后整个按钮（圆 + 箭头）一起灰透。
        if not self.isEnabled():
            p.setOpacity(0.25)
        circle = QRectF(0.5, 0.5, 35.0, 35.0)
        if self.hover and self.isEnabled():
            p.setBrush(QColor(self._c["btn_hover"]))
        else:
            p.setBrush(QColor(*self._c["navbg"]))
        p.setPen(QPen(QColor(self._c["border"]), 1))
        p.drawEllipse(circle)
        # 24x24 viewBox 的 SVG 缩放到 16x16，居中在 36px 圆内。
        s = 16.0 / 24.0
        ox = oy = 10.0
        pts = ([QPointF(16, 5), QPointF(8, 12), QPointF(16, 19)] if self.left
               else [QPointF(8, 5), QPointF(16, 12), QPointF(8, 19)])
        path = QPainterPath()
        first = pts[0]
        path.moveTo(ox + first.x() * s, oy + first.y() * s)
        for q in pts[1:]:
            path.lineTo(ox + q.x() * s, oy + q.y() * s)
        p.setPen(QPen(QColor(self._c["text"]), 1.8, Qt.SolidLine,
                      Qt.RoundCap, Qt.RoundJoin))
        p.setBrush(Qt.NoBrush)
        p.drawPath(path)


class HeartButton(QPushButton):
    """描边心形：未收藏半透明白填充+白描边，已收藏实心白，带投影。"""

    def __init__(self, size, parent=None, on=False):
        super().__init__(parent)
        self.hsize = size
        self.on = on
        self.setFixedSize(size, size)
        self.setCursor(Qt.PointingHandCursor)
        self.setToolTip("收藏 / 取消收藏")
        self.setFlat(True)

    def set_on(self, on):
        self.on = on
        self.update()

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        shape = heart_shape(self.hsize - 2)
        p.translate(1, 1)
        if self.on:
            p.setBrush(QBrush(QColor("#ffffff")))
        else:
            p.setBrush(QBrush(QColor(255, 255, 255, 76)))
        pen = QPen(QColor("#ffffff"), max(1.2, self.hsize / 13.0))
        pen.setJoinStyle(Qt.RoundJoin)
        p.setPen(pen)
        p.drawPath(shape)


class ThumbCard(QWidget):
    """缩略图卡片：16:9 cover 圆角、未选中半透明、选中蓝边、右下描边心形。"""

    def __init__(self, pm, active, fav, c, on_click, on_fav):
        super().__init__()
        self.pm = pm
        self.active = active
        self.fav = fav
        self.c = c
        self.on_click = on_click
        self.on_fav = on_fav
        # 缩略图保持系统默认箭头，不改成手型（用户要求鼠标样式不变化）

    def apply(self, pm, active, fav, c):
        self.pm, self.active, self.fav, self.c = pm, active, fav, c
        self.update()

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        p.setRenderHint(QPainter.SmoothPixmapTransform)
        w, h = self.width(), self.height()
        path = QPainterPath()
        path.addRoundedRect(QRectF(0, 0, w, h), 5, 5)
        if self.pm and not self.pm.isNull():
            # cover：按 16:9 容器比例裁剪居中
            pm = self.pm
            ratio = max(w / pm.width(), h / pm.height())
            sw, sh = pm.width() * ratio, pm.height() * ratio
            src = QRect(int((pm.width() - w / ratio) / 2), int((pm.height() - h / ratio) / 2),
                        int(w / ratio), int(h / ratio))
            p.save()
            p.setClipPath(path)
            if not self.active:
                p.setOpacity(0.55)
            p.drawPixmap(QRect(0, 0, w, h), pm, src)
            p.restore()
        else:
            p.setClipPath(path)
            p.fillRect(self.rect(), QColor(self.c["thumb"]))
        if self.active:
            p.setClipPath(path)
            pen = QPen(QColor(self.c["accent"]), 2)
            p.setPen(pen)
            p.drawRect(QRect(1, 1, w - 2, h - 2))
        # 右下角心形角标（直接绘制，不创建子控件）
        fs = max(10, min(14, int(w * 0.16)))
        geo = heart_shape(fs - 2)
        p.save()
        p.translate(w - fs - 5, h - fs - 4)
        if self.fav:
            p.setBrush(QBrush(QColor("#ffffff")))
        else:
            p.setBrush(QBrush(QColor(255, 255, 255, 76)))
        p.setPen(QPen(QColor("#ffffff"), max(1.1, fs / 13.0)))
        p.drawPath(geo)
        p.restore()

    def mousePressEvent(self, e):
        # 先记下位置，松开时再判定点击，避免横向拖动时误切图/误收藏。
        self._press_pos = e.pos()

    def mouseReleaseEvent(self, e):
        pos = getattr(self, "_press_pos", None)
        self._press_pos = None
        if pos is None or (e.pos() - pos).manhattanLength() > 6:
            return
        # 右下角心形区域点收藏，其余点切图
        fs = max(10, min(14, int(self.width() * 0.16)))
        hx, hy = self.width() - fs - 5, self.height() - fs - 4
        if hx <= pos.x() <= hx + fs and hy <= pos.y() <= hy + fs:
            self.on_fav()
        else:
            self.on_click()


class ThumbScrollArea(QScrollArea):
    """缩略图横向滚动：滚轮横滚 + 左键拖动任意缩略图 + 触摸屏滑动。"""

    def __init__(self, parent=None):
        super().__init__(parent)
        self._press_pos = None
        self._dragging = False
        self._touch_x = None
        self.setVerticalScrollBarPolicy(Qt.ScrollBarAlwaysOff)
        self.setHorizontalScrollBarPolicy(Qt.ScrollBarAlwaysOff)
        self.viewport().setAttribute(Qt.WA_AcceptTouchEvents, True)
        self.viewport().installEventFilter(self)

    def _map_pos(self, obj, pos):
        if obj is self.viewport():
            return QPoint(int(pos.x()), int(pos.y()))
        return obj.mapTo(self.viewport(), QPoint(int(pos.x()), int(pos.y())))

    def _scroll_by(self, delta):
        bar = self.horizontalScrollBar()
        if bar.maximum() > 0:
            bar.setValue(bar.value() - int(delta))

    @staticmethod
    def _wheel_delta(e):
        """鼠标滚轮/触摸板统一取横向位移：优先像素位移，其次角度位移。"""
        pixel = e.pixelDelta()
        px = pixel.x() if pixel.x() else pixel.y()
        if px:
            return px
        return e.angleDelta().y()

    def wheelEvent(self, e):
        delta = self._wheel_delta(e)
        if delta:
            self._scroll_by(delta)
            e.accept()
        else:
            super().wheelEvent(e)

    def eventFilter(self, obj, e):
        # 关键：鼠标/触摸事件通常发给缩略图卡片，不再只监听 viewport，
        # 否则卡片吃掉了事件，拖动和滚轮看起来就像没实现。
        if obj is self.viewport() or isinstance(obj, ThumbCard):
            t = e.type()
            if t == QEvent.Wheel:
                delta = self._wheel_delta(e)
                if delta:
                    self._scroll_by(delta)
                    return True
            if t == QEvent.MouseButtonPress and e.button() == Qt.LeftButton:
                self._press_pos = self._map_pos(obj, e.pos())
                self._dragging = False
                return False  # 让 ThumbCard 仍能收到按下，用于后续点击判定
            if t == QEvent.MouseMove and self._press_pos is not None:
                pos = self._map_pos(obj, e.pos())
                dx = pos.x() - self._press_pos.x()
                if abs(dx) > 3:
                    self._dragging = True
                if self._dragging:
                    self._scroll_by(dx)
                    self._press_pos = pos
                    return True  # 拖动中不再传给卡片
            if t == QEvent.MouseButtonRelease and self._press_pos is not None:
                was_dragging = self._dragging
                self._press_pos = None
                self._dragging = False
                return was_dragging  # 拖动过则吞掉 release，避免误切图/误收藏
            if t in (QEvent.TouchBegin, QEvent.TouchUpdate, QEvent.TouchEnd):
                pts = e.touches()
                x = self._map_pos(obj, pts[0].pos()).x() if pts else None
                if t == QEvent.TouchBegin:
                    self._touch_x = x
                    return True
                if t == QEvent.TouchUpdate and x is not None and self._touch_x is not None:
                    self._scroll_by(x - self._touch_x)
                    self._touch_x = x
                    return True
                if t == QEvent.TouchEnd:
                    self._touch_x = None
                    return True
        return super().eventFilter(obj, e)


class WinButton(QPushButton):
    """自绘标题栏按钮：30px 命中区，图标大小与 40px 标题栏协调。"""

    def __init__(self, kind, parent=None):
        super().__init__(parent)
        self.kind = kind
        self.c = LIGHT
        self.setFixedSize(30, 30)
        self.setCursor(Qt.ArrowCursor)
        self.setToolTip("最小化" if kind == "min" else "关闭")

    def set_colors(self, c):
        self.c = c
        self.update()

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        r = QRectF(0.5, 0.5, self.width() - 1, self.height() - 1)
        if self.underMouse() and self.isEnabled():
            p.setBrush(QColor(self.c["btn_hover"]))
            p.setPen(Qt.NoPen)
            p.drawRoundedRect(r, 5, 5)
        col = QColor(self.c["text"])
        p.setPen(QPen(col, 1.4, Qt.SolidLine, Qt.RoundCap))
        cx, cy = self.width() / 2.0, self.height() / 2.0
        if self.kind == "min":
            p.drawLine(QPointF(cx - 5, cy), QPointF(cx + 5, cy))
        else:
            p.drawLine(QPointF(cx - 5, cy - 5), QPointF(cx + 5, cy + 5))
            p.drawLine(QPointF(cx + 5, cy - 5), QPointF(cx - 5, cy + 5))


class FovCheck(QWidget):
    """自绘「自动更换」勾选框：勾严格画在方框内，避免系统主题把 ✓ 画出边界。"""

    toggled = pyqtSignal(bool)

    def __init__(self, text, c, parent=None):
        super().__init__(parent)
        self.text = text
        self.c = c
        self.checked = False
        self.setFixedHeight(30)
        self.setCursor(Qt.PointingHandCursor)
        self.setAttribute(Qt.WA_TransparentForMouseEvents, False)

    def isChecked(self):
        return self.checked

    def setChecked(self, v):
        v = bool(v)
        if v == self.checked:
            return
        self.checked = v
        self.update()
        self.toggled.emit(v)

    def set_colors(self, c):
        self.c = c
        self.update()

    def mouseReleaseEvent(self, e):
        if e.button() == Qt.LeftButton and self.rect().contains(e.pos()):
            self.setChecked(not self.checked)
        super().mouseReleaseEvent(e)

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        h = self.height()
        box = QRectF(0.5, (h - 14) / 2.0 + 0.5, 13.0, 13.0)
        p.setBrush(QBrush(QColor(self.c["accent"]) if self.checked else QColor(self.c["surface"])))
        p.setPen(QPen(QColor(self.c["accent"]) if self.checked else QColor(self.c["border"]), 1.2))
        p.drawRoundedRect(box, 3, 3)
        if self.checked:
            # 对勾整体内缩在 13x13 方框内（x 3~10、y 4~9），不会越界。
            pen = QPen(QColor("#ffffff"), 1.8, Qt.SolidLine, Qt.RoundCap, Qt.RoundJoin)
            p.setPen(pen)
            path = QPainterPath()
            path.moveTo(box.left() + 3.2, box.top() + 6.8)
            path.lineTo(box.left() + 5.6, box.top() + 9.0)
            path.lineTo(box.left() + 10.0, box.top() + 4.0)
            p.drawPath(path)
        p.setPen(QColor(self.c["muted"]))
        font = self.font()
        font.setPixelSize(12)
        p.setFont(font)
        p.drawText(QRectF(19, 0, max(1, self.width() - 19), h),
                   Qt.AlignVCenter | Qt.AlignLeft, self.text)


class CheckIcon(QWidget):
    """收藏提示图标：绿色实心圆 + 白色粗对勾，按参考图自绘，比字体 ✓ 更饱满。"""

    def __init__(self, size=22, parent=None):
        super().__init__(parent)
        self.setFixedSize(size, size)
        self.setAttribute(Qt.WA_TransparentForMouseEvents, True)

    def paintEvent(self, e):
        p = QPainter(self)
        p.setRenderHint(QPainter.Antialiasing)
        w, h = self.width(), self.height()
        # 绿色实心圆，留 0.5px 给抗锯齿，边缘更干净
        p.setPen(Qt.NoPen)
        p.setBrush(QBrush(QColor("#2e9e3f")))
        p.drawEllipse(QRectF(0.5, 0.5, w - 1.0, h - 1.0))
        # 白色对勾：两笔圆头线条，粗度随图标尺寸缩放
        pen = QPen(QColor("#ffffff"), max(2.2, w * 0.15),
                   Qt.SolidLine, Qt.RoundCap, Qt.RoundJoin)
        p.setPen(pen)
        path = QPainterPath()
        path.moveTo(w * 0.27, h * 0.53)
        path.lineTo(w * 0.45, h * 0.70)
        path.lineTo(w * 0.74, h * 0.33)
        p.drawPath(path)


class Gallery(QWidget):
    def __init__(self, app):
        super().__init__()
        self.app = app
        self.items = []          # 全部壁纸，按最旧→今天
        self.view = "all"
        self.cur = 0
        self.pix_cache = {}      # index -> QPixmap（大图原图）
        self.thumb_cache = {}    # index -> QPixmap（缩略图原图）
        self.last_applied = -1
        self.applying = False
        self.dark = False
        self.fatal_ticks = 0
        self.win_id = 0
        self._data_ready = False
        self._loading = False
        self._dirty = False
        self._data_ts = 0.0
        self._focus_grace_until = 0.0
        self.c = LIGHT
        self.thumb_cards = []
        self.thumb_w = 92
        self._listen_tries = 0
        self._fov_loaded = False
        self._workarea_bottom = None   # (bottom, 采样时刻)，避免每次弹出都 fork xprop/wmctrl
        self._taskbar_set = False      # skip_taskbar 属性只需设置一次
        self.focus_timer = QTimer(self)
        self.focus_timer.timeout.connect(self._check_focus)
        self.server = QTcpServer(self)
        self._build_ui()
        self._load_data()
        self._setup_server()
        self._setup_keepalive()
        # 进程在软件启动时后台预热，这里提前采样一次工作区，首次弹出也免 fork。
        self._workarea_bottom_sample()

    # ---------- UI ----------
    def _build_ui(self):
        # 用户指定标题：与旧版同名窗口区分，避免窗口管理器追加 <2>。
        self.setWindowTitle("LitheScape 壁纸画廊")
        # 自绘标题栏：deepin 原生按钮尺寸无法由 Qt 控制，改用无框窗口自定义
        # 30px 按钮 + 40px 标题栏，比例更和谐；仍保持不进任务栏、固定尺寸、无最大化。
        self.setWindowFlags(Qt.FramelessWindowHint | Qt.Tool | Qt.WindowStaysOnTopHint)
        self.setFixedSize(WIN_W, WIN_H)
        self._set_window_icon()
        self._drag_pos = None
        self.titlebar = QWidget(self)
        self.titlebar.setObjectName("titlebar")
        self.titlebar.setFixedHeight(40)
        self.titlebar.installEventFilter(self)
        self.title_lbl = QLabel("LitheScape 壁纸画廊", self.titlebar)
        self.title_lbl.setObjectName("wintitle")
        self.title_lbl.setAlignment(Qt.AlignCenter)
        self.title_lbl.setAttribute(Qt.WA_TransparentForMouseEvents, True)
        self.title_lbl.setGeometry(0, 0, WIN_W, 40)
        self.win_min = WinButton("min", self.titlebar)
        self.win_close = WinButton("close", self.titlebar)
        self.win_min.setGeometry(WIN_W - 76, 5, 30, 30)
        self.win_close.setGeometry(WIN_W - 40, 5, 30, 30)
        self.win_min.clicked.connect(self.showMinimized)
        self.win_close.clicked.connect(self.hide)
        # 旧浏览器版 CSS 用 px，Qt 默认点大小会偏大；统一按像素字号。
        font = QFont("Noto Sans CJK SC")
        font.setPixelSize(12)
        self.setFont(font)
        self.app.setFont(font)

        # stage：大图 + 覆盖其上的箭头/收藏（阴影由 QGraphicsDropShadowEffect 提供）
        self.stage = QWidget()
        self.stage.setObjectName("stage")
        self.photo = QLabel(self.stage)
        self.photo.setAlignment(Qt.AlignCenter)
        self.photo.setMinimumSize(1, 1)
        shadow = QGraphicsDropShadowEffect(self.photo)
        shadow.setBlurRadius(14)
        shadow.setOffset(0, 5)
        shadow.setColor(QColor(*self.c["shadow"]))
        self.photo.setGraphicsEffect(shadow)
        self.prev = NavButton(True, self.stage)
        self.next = NavButton(False, self.stage)
        self.prev.setObjectName("nav")
        self.next.setObjectName("nav")
        self.prev.clicked.connect(lambda: self.go(-1))
        self.next.clicked.connect(lambda: self.go(1))
        self.photo_fav = HeartButton(24, self.stage)
        self.photo_fav.clicked.connect(lambda: self.toggle_fav(self.current_item()))
        # 日期放在预览图左下角，与右下角收藏图标同高（两者中心对齐）。
        self.photo_date = QLabel(self.stage)
        self.photo_date.setObjectName("photodate")
        self.photo_date.setAlignment(Qt.AlignVCenter | Qt.AlignLeft)
        self.photo_date.setAttribute(Qt.WA_TransparentForMouseEvents, True)
        self.photo_date.setFixedHeight(24)
        self.prev.hide()
        self.next.hide()
        self.photo_fav.hide()
        self.photo_date.hide()

        # bar：标签 / 信息 / 缩略图
        self.tab_all = QPushButton("全部壁纸")
        self.tab_fav = QPushButton("我的收藏")
        self.tab_all.setObjectName("tab")
        self.tab_fav.setObjectName("tab")
        self.tab_all.setFixedHeight(28)
        self.tab_fav.setFixedHeight(28)
        self.tab_all.setCheckable(True)
        self.tab_fav.setCheckable(True)
        self.tab_all.clicked.connect(lambda: self.switch_view("all"))
        self.tab_fav.clicked.connect(lambda: self.switch_view("fav"))
        tabs = QHBoxLayout()
        tabs.setContentsMargins(0, 0, 0, 0)
        tabs.setSpacing(5)
        tabs.addWidget(self.tab_all)
        tabs.addWidget(self.tab_fav)
        tabs.addStretch(1)

        self.title = QLabel("加载中…")
        self.title.setObjectName("title")
        self.title.setFixedHeight(30)
        self.date = QLabel("")
        self.date.setObjectName("date")
        self.date.setFixedHeight(30)
        # 日期已移到预览图左下角；信息行只在空收藏时用它显示引导文字。
        self.date.hide()
        self.favbtn = QPushButton("收藏")
        self.favbtn.setObjectName("btn")
        self.favbtn.setFixedHeight(30)
        self.favbtn.setCheckable(True)
        self.favbtn.clicked.connect(lambda: self.toggle_fav(self.current_item()))
        self.story = QPushButton("图片故事")
        self.story.setObjectName("btn")
        self.story.setFixedHeight(30)
        self.story.clicked.connect(self.open_story)

        # 收藏自动更换：恢复旧版勾选框开关 + 紧凑时间下拉，选择即保存。
        # 勾选框与时间框放在固定高度容器里，切换标签时信息行高度绝不变化。
        self.fov_wrap = QWidget()
        self.fov_wrap.setFixedHeight(30)
        hl = QHBoxLayout(self.fov_wrap)
        hl.setContentsMargins(0, 0, 0, 0)
        hl.setSpacing(5)
        self.fov_check = FovCheck("自动更换", self.c, self.fov_wrap)
        self.fov_check.setObjectName("fovcheck")
        self.fov_check.setFixedWidth(70)
        self.fovpreset = QComboBox()
        self.fovpreset.setObjectName("fovpreset")
        for text, interval, unit in FOV_PRESETS:
            self.fovpreset.addItem(text, (interval, unit))
        self.fovpreset.setFixedSize(70, 28)
        self.fov_check.toggled.connect(self._save_fov)
        self.fovpreset.currentIndexChanged.connect(self._save_fov)
        hl.addWidget(self.fov_check)
        hl.addWidget(self.fovpreset)

        info = QHBoxLayout()
        info.setContentsMargins(0, 0, 0, 0)
        info.setSpacing(9)
        info.addWidget(self.title, 1)
        info.addWidget(self.date)
        info.addWidget(self.fov_wrap)
        info.addWidget(self.favbtn)
        info.addWidget(self.story)

        self.thumb_area = ThumbScrollArea()
        self.thumb_area.setObjectName("thumbs")
        self.thumb_area.setWidgetResizable(True)
        self.thumb_host = QWidget()
        self.thumb_layout = QHBoxLayout(self.thumb_host)
        self.thumb_layout.setContentsMargins(0, 0, 0, 0)
        self.thumb_layout.setSpacing(THUMB_GAP)
        self.thumb_layout.setAlignment(Qt.AlignLeft)
        self.thumb_area.setWidget(self.thumb_host)
        self.thumb_area.setFixedHeight(76)

        self.bar = QWidget()
        self.bar.setObjectName("bar")
        bl = QVBoxLayout(self.bar)
        bl.setContentsMargins(GUTTER, 9, GUTTER, 11)
        bl.setSpacing(7)
        bl.addLayout(tabs)
        bl.addLayout(info)
        bl.addWidget(self.thumb_area)

        root = QVBoxLayout(self)
        root.setContentsMargins(0, 0, 0, 0)
        root.setSpacing(0)
        root.addWidget(self.titlebar)
        root.addWidget(self.stage, 1)
        root.addWidget(self.bar)

        # 保存提醒：浅灰圆角条 + 绿色圆形白勾 + 深色文字（对齐用户参考图）
        self.toast_lbl = QFrame(self)
        self.toast_lbl.setObjectName("toast")
        toast_layout = QHBoxLayout(self.toast_lbl)
        toast_layout.setContentsMargins(10, 8, 14, 8)
        toast_layout.setSpacing(9)
        self.toast_icon = CheckIcon(22, self.toast_lbl)
        self.toast_icon.setObjectName("toasticon")
        self.toast_text = QLabel("")
        self.toast_text.setObjectName("toasttext")
        toast_layout.addWidget(self.toast_icon)
        toast_layout.addWidget(self.toast_text)
        self.toast_lbl.hide()
        self.toast_timer = QTimer(self)
        self.toast_timer.setSingleShot(True)
        self.toast_timer.timeout.connect(lambda: self.toast_lbl.hide())

        self.apply_theme(False)

    def _set_window_icon(self):
        """用软件自身图标，避免 dock 显示变形的默认图标。"""
        try:
            data = http_get("/favicon.png", timeout=2)
            pm = QPixmap()
            if pm.loadFromData(data) and not pm.isNull():
                icon = QIcon(pm)
                self.setWindowIcon(icon)
                self.app.setWindowIcon(icon)
        except Exception:
            pass

    def apply_theme(self, dark):
        self.dark = dark
        c = DARK if dark else LIGHT
        self.c = c
        self.titlebar.setStyleSheet("""
            QWidget#titlebar {{
                background:{surface}; border-bottom:1px solid {border};
            }}
            QLabel#wintitle {{
                color:{text}; font-size:13px; font-weight:500;
                background:transparent;
            }}
        """.format(**c))
        self.win_min.set_colors(c)
        self.win_close.set_colors(c)
        if hasattr(self, "fov_check"):
            self.fov_check.set_colors(c)
        self.stage.setStyleSheet("QWidget#stage { background:%s; }" % c["bg"])
        # 图片上的日期：与右下角收藏心形同色（纯白），不加背景。
        self.photo_date.setStyleSheet("""
            QLabel#photodate {
                color:#ffffff; background:transparent; border:0;
                padding:0; font-size:11px;
            }
        """)
        self.bar.setStyleSheet("""
            QWidget#bar {{ background:{surface}; border-top:1px solid {border}; }}
            QLabel#title {{ color:{text}; font-size:13px; font-weight:500; }}
            QLabel#date {{ color:{muted}; font-size:11px; }}
            QPushButton#tab {{
                background:transparent; color:{muted}; border:1px solid transparent;
                border-radius:5px; padding:5px 12px; font-size:12px;
            }}
            QPushButton#tab:hover {{ color:{text}; }}
            QPushButton#tab:checked {{ background:{accent}; border-color:{accent}; color:#ffffff; }}
            QPushButton#btn {{
                background:{btn}; color:{text}; border:1px solid {border};
                border-radius:6px; padding:6px 10px; font-size:12px;
            }}
            QPushButton#btn:hover {{ background:{btn_hover}; }}
            QPushButton#btn:checked {{ background:{accent}; border-color:{accent}; color:#ffffff; }}
            QComboBox#fovpreset {{
                background:{btn}; color:{text}; border:1px solid {border};
                border-radius:6px; padding:3px 6px; font-size:12px;
                min-height:22px;
            }}
            QComboBox#fovpreset:disabled {{ color:{muted}; background:{surface}; }}
            QComboBox#fovpreset::drop-down {{ width:14px; border:0; }}
            QComboBox#fovpreset QAbstractItemView {{
                background:{surface}; border:1px solid {border};
                border-radius:8px; padding:4px; outline:0;
            }}
            QComboBox#fovpreset QAbstractItemView::item {{
                min-height:26px; padding:4px 10px; border-radius:6px;
                color:{text}; font-size:12px;
            }}
            QComboBox#fovpreset QAbstractItemView::item:hover {{ background:{btn_hover}; }}
            QComboBox#fovpreset QAbstractItemView::item:selected {{
                background:{accent}; color:#ffffff;
            }}
            QScrollArea#thumbs {{ background:{surface}; border:0; }}
        """.format(**c))
        self.toast_lbl.setStyleSheet("""
            QFrame#toast {
                background:#f7f7f7; border:1px solid #e2e2e2;
                border-radius:11px;
            }
            QLabel#toasttext {
                color:#333333; background:transparent; font-size:13px;
            }
        """)
        self.prev.set_colors(c)
        self.next.set_colors(c)
        self.photo_fav.update()
        if hasattr(self, "photo") and self.photo.pixmap():
            eff = self.photo.graphicsEffect()
            if eff:
                eff.setColor(QColor(*c["shadow"]))
        for card in getattr(self, "thumb_cards", []):
            it = card.property("it")
            card.apply(it.get("pm"), it.get("active"), it.get("fav"), c)
        self.tab_all.setChecked(self.view == "all")
        self.tab_fav.setChecked(self.view == "fav")
        self._update_fav_button()

    # ---------- 数据 ----------
    def _load_data(self):
        if self._loading:
            return
        self._loading = True
        try:
            data = http_json("/data?view=" + self.view)
        except Exception:
            self._loading = False
            QTimer.singleShot(1500, self._load_data)
            return
        self.items = data.get("items", [])
        theme = data.get("theme", "light")
        if (theme == "dark") != self.dark:
            self.apply_theme(theme == "dark")
        if not self.items:
            self.title.setText("壁纸列表加载中，几秒后自动刷新…")
            self._loading = False
            QTimer.singleShot(3000, self._load_data)
            return
        self.cur = len(self.items) - 1
        self.pix_cache.clear()
        self.thumb_cache.clear()
        self._loading = False
        self._data_ready = True
        self._data_ts = time.monotonic()
        # 配置只在数据就绪时读一次；反复弹窗/切标签不再回填，
        # 否则用户刚勾选的状态会被上一次的 HTTP 结果覆盖，看起来像勾选框消失。
        if not self._fov_loaded:
            self._fov_loaded = True
            self._load_fov()
        self._render()

    def visible(self):
        if self.view == "fav":
            return [x for x in self.items if x.get("f")]
        return self.items

    def current_item(self):
        v = self.visible()
        if 0 <= self.cur < len(v):
            return v[self.cur]
        return None

    # ---------- 渲染 ----------
    def _render(self):
        v = self.visible()
        self.fov_wrap.setVisible(self.view == "fav")
        self.tab_all.setChecked(self.view == "all")
        self.tab_fav.setChecked(self.view == "fav")
        if not v:
            self.title.setText("还没有收藏壁纸")
            self.date.setText("在「全部壁纸」中点「收藏」按钮")
            self.date.show()
            self.photo.clear()
            self.photo_fav.hide()
            self.photo_date.hide()
            self.prev.setEnabled(False)
            self.next.setEnabled(False)
            self.favbtn.setEnabled(False)
            self.story.setEnabled(False)
            self._clear_thumbs()
            return
        if self.cur >= len(v):
            self.cur = len(v) - 1
        it = v[self.cur]
        prefix = "收藏 · " if self.view == "fav" else ("今天 · " if it.get("i") == 0 else "%d 天前 · " % it.get("i", 0))
        self.title.setText(prefix + it.get("t", ""))
        d = it.get("d", "")
        self.date.hide()
        self.photo_date.setText("%s-%s-%s" % (d[:4], d[4:6], d[6:8]) if len(d) >= 8 else "")
        self.photo_date.adjustSize()
        self.photo_fav.set_on(bool(it.get("f")))
        self.photo_fav.show()
        # 旧版浏览器行为：到头后该方向按钮整体灰透，且点击无效。
        self.prev.setEnabled(self.cur > 0)
        self.next.setEnabled(self.cur < len(v) - 1)
        self.favbtn.setEnabled(True)
        self.story.setEnabled(bool(it.get("s")))
        self._update_fav_button()
        self._render_thumbs()
        self._load_main()
        QTimer.singleShot(0, self.apply_current)
        self._dirty = False

    def _update_fav_button(self):
        it = self.current_item()
        fav = bool(it and it.get("f"))
        self.favbtn.setChecked(fav)
        self.favbtn.setText("已收藏" if fav else "收藏")

    def _clear_thumbs(self):
        while self.thumb_layout.count():
            item = self.thumb_layout.takeAt(0)
            w = item.widget()
            if w:
                w.setParent(None)
                w.deleteLater()
        self.thumb_cards = []

    def _render_thumbs(self):
        self._clear_thumbs()
        v = self.visible()
        # 按可用宽度自适应：旧版口径（理想 100px、间距 10px），620 宽默认 6 张
        avail = self.thumb_area.viewport().width()
        show = max(1, int(round((avail + THUMB_GAP) / (THUMB_IDEAL + THUMB_GAP))))
        tw = max(52, min(140, int((avail - (show - 1) * THUMB_GAP) / show)))
        th = int(tw * 9 / 16)
        self.thumb_w = tw
        self.thumb_area.setFixedHeight(th + 22)
        for k, it in enumerate(v):
            pm = self._thumb_pix(it.get("i", 0))
            fav = bool(it.get("f"))
            card = ThumbCard(pm, k == self.cur, fav, self.c,
                             lambda kk=k: self._on_thumb_click(kk),
                             lambda ii=it.get("i"): self.toggle_fav_by_index(ii))
            card.setFixedSize(tw, th)
            card.setProperty("it", {"pm": pm, "active": k == self.cur, "fav": fav})
            card.installEventFilter(self.thumb_area)
            self.thumb_layout.addWidget(card)
            self.thumb_cards.append(card)
        QTimer.singleShot(0, self._scroll_cur_thumb)

    def _scroll_cur_thumb(self):
        if 0 <= self.cur < len(self.thumb_cards):
            self.thumb_area.ensureWidgetVisible(self.thumb_cards[self.cur])

    def _on_thumb_click(self, k):
        if 0 <= k < len(self.visible()):
            self.cur = k
            self._render()

    def _img_pix(self, idx, cache):
        if idx in cache:
            return cache[idx]
        try:
            data = http_get("/img/%d.jpg" % idx, timeout=10)
            pm = QPixmap()
            pm.loadFromData(data)
            if not pm.isNull():
                cache[idx] = pm
                return pm
        except Exception:
            pass
        return None

    def _thumb_pix(self, idx):
        return self._img_pix(idx, self.thumb_cache)

    def _load_main(self, retry=True):
        it = self.current_item()
        if not it:
            return
        pm = self._img_pix(it.get("i", 0), self.pix_cache)
        if pm:
            self._set_photo(pm)
        elif retry:
            # 和浏览器版一致：图片加载失败稍后重试，避免永久空白
            QTimer.singleShot(1200, lambda: self._load_main(False))

    def _set_photo(self, pm):
        st = self.stage.size()
        # 左右留 GUTTER，上下留 8；contain 缩放
        scaled = pm.scaled(max(1, st.width() - 2 * GUTTER), max(1, st.height() - 16),
                           Qt.KeepAspectRatio, Qt.SmoothTransformation)
        self.photo.setPixmap(rounded_pixmap(scaled, 8))
        self._img_rect = (scaled.width(), scaled.height())
        QTimer.singleShot(0, self._place_nav)

    def _place_nav(self):
        sw, sh = self.stage.width(), self.stage.height()
        iw, ih = getattr(self, "_img_rect", (0, 0))
        if not iw or not ih:
            return
        left = (sw - iw) // 2
        top = (sh - ih) // 2
        # QLabel 是 stage 的子控件但没有布局，必须显式摆到居中位置；
        # 之前它在 (0,0) 不动，导致大图贴左贴顶、右侧和下方留出大块空白。
        self.photo.setGeometry(left, top, iw, ih)
        cy = top + ih // 2
        half = self.prev.width() // 2
        # 按钮完整落在图片内部，左右各内缩 6px，不再压到图片外的灰色区域。
        inset = 6
        prev_x = left + inset
        next_x = left + iw - inset - self.next.width()
        self.prev.move(prev_x, cy - half)
        self.next.move(next_x, cy - half)
        self.photo_fav.move(left + iw - 24 - 8, top + ih - 24 - 8)
        # 日期与收藏图标同高：两者都是 24px 高，顶部 y 相同即中心对齐。
        self.photo_date.move(left + 8, top + ih - 24 - 8)
        self.prev.raise_()
        self.next.raise_()
        self.photo_fav.raise_()
        self.photo_date.raise_()
        self.prev.show()
        self.next.show()
        if self.current_item():
            self.photo_fav.show()
            if self.photo_date.text():
                self.photo_date.show()
        # 下方栏保持与旧版一致的固定 10px 左右留白：不再跟着大图边缘漂移，
        # 否则图片高度受限时文字和缩略图两侧会多出几十像素空白。
        # 需要先让布局生效，再按实际视口宽度校准缩略图张数。
        self._relayout_thumbs()

    def _relayout_thumbs(self):
        v = self.visible()
        if not v or not self.thumb_cards:
            return
        avail = self.thumb_area.viewport().width()
        show = max(1, int(round((avail + THUMB_GAP) / (THUMB_IDEAL + THUMB_GAP))))
        tw = max(52, min(140, int((avail - (show - 1) * THUMB_GAP) / show)))
        th = int(tw * 9 / 16)
        if tw == self.thumb_w:
            return
        self.thumb_w = tw
        self.thumb_area.setFixedHeight(th + 22)
        for k, card in enumerate(self.thumb_cards):
            it = v[k] if k < len(v) else {}
            pm = self._thumb_pix(it.get("i", 0))
            card.setFixedSize(tw, th)
            card.setProperty("it", {"pm": pm, "active": k == self.cur, "fav": bool(it.get("f"))})
            card.apply(pm, k == self.cur, bool(it.get("f")), self.c)

    def resizeEvent(self, e):
        super().resizeEvent(e)
        it = self.current_item()
        if it and it.get("i") in self.pix_cache:
            self._set_photo(self.pix_cache[it.get("i")])

    # ---------- 交互 ----------
    def go(self, step):
        v = self.visible()
        nxt = self.cur + step
        if 0 <= nxt < len(v):
            self.cur = nxt
            self._render()

    def switch_view(self, v):
        if v == self.view:
            return
        # 数据里已包含全部/收藏两套视图，切标签只用内存数据重绘，
        # 不重新 HTTP 拉取、不清空缓存，避免「加载中」闪一下导致页面跳动。
        self.view = v
        self._dirty = False
        self._render()

    def toggle_fav(self, it):
        if it:
            self.toggle_fav_by_index(it.get("i"))

    def toggle_fav_by_index(self, idx):
        if idx is None:
            return
        try:
            j = http_json("/fav/%d" % idx)
        except Exception:
            return
        if not j.get("ok"):
            return
        fav = bool(j.get("fav"))
        for x in self.items:
            if x.get("i") == idx:
                x["f"] = fav
        self._render()
        self.toast("已保存到收藏夹" if fav else "已取消收藏")

    def open_story(self):
        it = self.current_item()
        if it and it.get("s"):
            QDesktopServices.openUrl(QUrl(it["s"]))

    def _save_fov(self, *_):
        """勾选框控制开关、下拉只选时间；任一变化都立即落盘，无「保存」按钮。"""
        enabled = self.fov_check.isChecked()
        self.fovpreset.setEnabled(enabled)
        data = self.fovpreset.currentData()
        interval, unit = data if data else (30, "min")
        try:
            http_json("/favcfg/set?enabled=%d&interval=%d&unit=%s" %
                      (1 if enabled else 0, interval, unit))
        except Exception:
            pass

    def _load_fov(self):
        try:
            cfg = http_json("/favcfg")
        except Exception:
            return
        unit = cfg.get("unit", "min")
        interval = int(cfg.get("interval", 0) or 0)
        if unit == "day":
            # 兼容旧「天」配置：折算成小时后归入系统预设（1小时）
            unit, interval = "hour", 1
        key = (0, unit) if unit in ("login", "wake") else (interval, unit)
        idx = next((i for i, p in enumerate(FOV_PRESETS) if p[1:] == key), -1)
        if idx < 0:
            idx = 5  # 30分钟
        # 先断开两个信号，避免加载配置时误写回后端
        try:
            self.fov_check.toggled.disconnect(self._save_fov)
            self.fovpreset.currentIndexChanged.disconnect(self._save_fov)
        except TypeError:
            pass
        self.fovpreset.setCurrentIndex(idx)
        self.fov_check.setChecked(bool(cfg.get("enabled")))
        self.fovpreset.setEnabled(bool(cfg.get("enabled")))
        self.fov_check.toggled.connect(self._save_fov)
        self.fovpreset.currentIndexChanged.connect(self._save_fov)

    def apply_current(self):
        v = self.visible()
        if not v or self.applying:
            return
        it = v[self.cur] if 0 <= self.cur < len(v) else None
        if not it or it.get("i") == self.last_applied:
            return
        self.applying = True
        ok = False
        try:
            j = http_json("/set/%d?quiet=1" % it.get("i"))
            ok = bool(j.get("ok"))
            if ok:
                self.last_applied = it.get("i")
        except Exception:
            pass
        self.applying = False
        if not ok:
            QTimer.singleShot(900, self.apply_current)
        else:
            v2 = self.visible()
            now = v2[self.cur] if 0 <= self.cur < len(v2) else None
            if now and now.get("i") != self.last_applied:
                QTimer.singleShot(0, self.apply_current)

    def toast(self, msg):
        self.toast_text.setText(msg)
        self.toast_lbl.adjustSize()
        self.toast_lbl.move((self.width() - self.toast_lbl.width()) // 2,
                            self.height() - self.toast_lbl.height() - 18)
        self.toast_lbl.show()
        self.toast_lbl.raise_()
        self.toast_timer.start(1000)

    # ---------- 窗口显隐 ----------
    def _workarea_bottom_sample(self):
        """采样一次工作区底边（排除 dock/taskbar）并缓存。

        这是弹出链路里最大的延迟源：原实现每次 show 都 fork xprop/wmctrl 子进程。
        dock 位置在会话内几乎不变，进程启动时采样一次即可，后续直接复用。
        每 30 秒最多重采一次，兼顾 dock 自动隐藏/分辨率变化。
        """
        now = time.monotonic()
        if self._workarea_bottom and now - self._workarea_bottom[1] < 30:
            return self._workarea_bottom[0]
        scr = self.app.primaryScreen().geometry()
        bottom = None
        # 首选 EWMH 工作区：deepin 下 _NET_WORKAREA 精确排除 dock/taskbar。
        try:
            out = subprocess.check_output(["xprop", "-root", "_NET_WORKAREA"], timeout=1)
            nums = [int(v.strip()) for v in out.decode("utf-8", "ignore")
                    .split("=", 1)[1].split(",")]
            if len(nums) == 4:
                bottom = nums[1] + nums[3]
        except Exception:
            pass
        # 兜底：从 wmctrl 找 dock 窗口顶边
        if bottom is None:
            try:
                out = subprocess.check_output(["wmctrl", "-lG"], timeout=1)
                for line in out.decode("utf-8", "ignore").splitlines():
                    parts = line.split(None, 7)
                    if len(parts) < 7:
                        continue
                    low = line.lower()
                    if "dock" not in low and "taskbar" not in low:
                        continue
                    x0, y0, w0, h0 = map(int, parts[2:6])
                    if w0 >= scr.width() // 2 and h0 <= 120 and y0 >= scr.height() // 2:
                        bottom = y0
                        break
            except Exception:
                pass
        if bottom is None:
            bottom = self.app.primaryScreen().availableGeometry().bottom() + 1
        self._workarea_bottom = (bottom, now)
        return bottom

    def _place_window(self):
        """右下角定位：以真实 dock 顶边为基准，四边留白一致且不触发窗口吸附。"""
        scr = self.app.primaryScreen().geometry()
        # 按用户截图：右侧与底部到任务栏都留约 10px，既不贴死也不抬高。
        gap = 10
        bottom = self._workarea_bottom_sample()
        # show() 后 WM 会加标题栏，frameGeometry 比客户区高；
        # move() 定位的是窗口框原点，所以要把这部分高度扣掉，客户区底边才真正留 gap。
        frame_top = max(0, self.frameGeometry().height() - self.height())
        # QRect.right() 是最后一个像素坐标（含端点），用它算会多留 1px；
        # 这里用 x + width 取真正的右边界，保证右边距恰好是 gap。
        x = max(gap, scr.x() + scr.width() - self.width() - gap)
        y = max(gap, bottom - self.height() - gap - frame_top)
        self.move(x, y)

    def show_gallery(self, view):
        # 数据里已同时包含全部/收藏，切标签不需要重新 HTTP 拉取或清空图片缓存。
        # 只有数据过期（>60s）或尚未就绪才刷新，减少首次弹出的等待。
        stale = not self._data_ready or (time.monotonic() - self._data_ts > 60)
        if stale:
            self._load_data()
        if view != self.view:
            self.view = view
            self._render()
        elif self._dirty:
            self._render()
        else:
            # 内容没变：只把当前缩略图滚进视野，不再重建控件/重载图片。
            self._scroll_cur_thumb()
        # 直接定位 + 显示，不再用 60ms 延迟等 WM：无框窗口 frame 偏移固定，
        # 进程启动时已采样工作区，定位无需 fork；下一次事件循环再校准一次即可。
        self._place_window()
        self.show()
        self.raise_()
        self.activateWindow()
        QTimer.singleShot(0, self._place_window)
        # 工具窗口不进任务栏：窗口 ID 不变，skip_taskbar 属性只设一次，
        # 避免每次弹出都 fork wmctrl 拖慢速度。
        if not self._taskbar_set:
            self._taskbar_set = True
            try:
                subprocess.run(["wmctrl", "-i", "-r", hex(int(self.winId())),
                                "-b", "add,skip_taskbar,skip_pager"],
                               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=1)
            except Exception:
                pass
        # 失焦监控：Qt focusOut 立即触发 + 低频兜底轮询，关闭更快且不卡界面。
        self.win_id = int(self.winId())
        # 宽限期只覆盖 activateWindow 的焦点切换过程，缩短到 90ms 让点外部关闭更跟手。
        self._focus_grace_until = time.monotonic() + 0.09
        if not self.focus_timer.isActive():
            self.focus_timer.start(50)

    def _is_active(self):
        aw = QApplication.activeWindow()
        if aw is not None:
            return aw is self or self.isAncestorOf(aw)
        try:
            out = subprocess.check_output(["xdotool", "getactivewindow"], timeout=0.3)
            return int(out.decode().strip()) == self.win_id
        except Exception:
            return True

    def focusOutEvent(self, e):
        super().focusOutEvent(e)
        if self.isVisible():
            # 立即复查一次；20ms 后再兜一次，兼顾点外部秒关与下拉弹层不误关。
            self._check_focus()
            QTimer.singleShot(20, self._check_focus)

    def _check_focus(self):
        if not self.isVisible():
            self.focus_timer.stop()
            return
        if time.monotonic() < self._focus_grace_until:
            return
        if self._is_active():
            return
        self.hide()
        self.focus_timer.stop()

    def eventFilter(self, obj, e):
        if obj is self.titlebar:
            if e.type() == QEvent.MouseButtonPress and e.button() == Qt.LeftButton:
                self._drag_pos = e.globalPos() - self.frameGeometry().topLeft()
                return True
            if e.type() == QEvent.MouseMove and self._drag_pos is not None:
                self.move(e.globalPos() - self._drag_pos)
                return True
            if e.type() == QEvent.MouseButtonRelease:
                self._drag_pos = None
                return True
        return super().eventFilter(obj, e)

    def keyPressEvent(self, e):
        if e.key() == Qt.Key_Right:
            self.go(1)
        elif e.key() == Qt.Key_Left:
            self.go(-1)
        elif e.key() == Qt.Key_Escape:
            self.hide()
        else:
            super().keyPressEvent(e)

    # ---------- 控制通道 ----------
    def _setup_server(self):
        self._listen_tries = 0
        self._retry_listen()

    def _retry_listen(self):
        if self.server.isListening():
            return
        self._listen_tries += 1
        if self.server.listen(QHostAddress("127.0.0.1"), CTRL_PORT):
            return
        # 已有画廊进程占用控制端口时，新进程直接退出，避免出现第二个同名窗口。
        if self._listen_tries >= 3:
            QTimer.singleShot(0, self.app.quit)
            return
        QTimer.singleShot(400, self._retry_listen)

    def _setup_keepalive(self):
        self.ka = QTimer(self)
        self.ka.timeout.connect(self._check_parent)
        self.ka.start(5000)

    def _check_parent(self):
        try:
            http_get("/favicon.png", timeout=2)
            self.fatal_ticks = 0
        except Exception:
            self.fatal_ticks += 1
            if self.fatal_ticks >= 3:
                QTimer.singleShot(0, self.app.quit)

    def on_new_connection(self):
        while self.server.hasPendingConnections():
            sock = self.server.nextPendingConnection()
            sock.readyRead.connect(lambda s=sock: self._read_cmd(s))

    def _read_cmd(self, sock):
        try:
            raw = bytes(sock.readAll()).decode("utf-8", "ignore").strip()
        except Exception:
            return
        sock.disconnectFromHost()
        for line in raw.splitlines():
            parts = line.split("\t")
            if parts[0] == "show":
                self.show_gallery(parts[1] if len(parts) > 1 else "all")
            elif parts[0] == "quit":
                QTimer.singleShot(0, self.app.quit)


def main():
    app = QApplication(sys.argv)
    app.setQuitOnLastWindowClosed(False)
    g = Gallery(app)
    g.server.newConnection.connect(g.on_new_connection)
    sys.exit(app.exec_())


if __name__ == "__main__":
    main()
