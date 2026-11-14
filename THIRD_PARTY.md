# 第三方组件与许可

ClipForge 本身以 **GPL-3.0-or-later** 发布。下面列出运行期与构建期涉及的第三方内容。

## FFmpeg（P1 起使用）

**决策（ADR-002）**：以 sidecar CLI 方式调用 `ffmpeg` / `ffprobe`，不链接开发库。

**许可策略：GPL 构建，随包分发**

- 分发包内自带一份包含 libx264 / libx265 的 **GPL 版 FFmpeg**，位于 `bin/`。
- 因为 ClipForge 与该 GPL 二进制构成整体分发，ClipForge 自身按 GPL-3.0-or-later 开源，源码随二进制提供。
- 精确导出默认编码器：H.264 (`libx264`, CRF 18 / preset medium) + AAC 192k；WebM 使用 VP9 + Opus，启动时探测编码器可用性。
- 随包附带的文件：

  ```text
  bin/ffmpeg.exe, bin/ffprobe.exe
  bin/avcodec-*.dll, avfilter-*.dll, avformat-*.dll,
      avutil-*.dll, swresample-*.dll, swscale-*.dll
  ```

  `ffplay.exe` 与 `avdevice-*.dll` 不参与 ClipForge 的任何功能，未随包提供。

- 二进制定位顺序（P1 实现）：用户设置路径 → 可执行文件同目录 `bin/` → 应用资源目录 → `PATH`。
  发行包因此开箱即用；用户仍可在偏好文件里指向自己的 FFmpeg 覆盖内置版本。
- 启动执行 `ffmpeg -version` 与 `ffmpeg -encoders` 做能力探测，缺失时给出可操作引导而不是崩溃。

**当前打包的构建**

```text
ffmpeg version N-125752-g2f209337fc-20260724
--enable-gpl --enable-version3 --enable-shared
含 libx264 / libx265 / libvpx-vp9 / aac / libopus / libdav1d / libsvtav1 等
```

- 来源：[BtbN/FFmpeg-Builds](https://github.com/BtbN/FFmpeg-Builds)（win64-gpl-shared）
- 对应源码：https://github.com/FFmpeg/FFmpeg/tree/n7.1 （构建对应 commit 见版本串 `g2f209337fc`）
- 构建脚本：https://github.com/BtbN/FFmpeg-Builds 的 `win64-gpl-shared` 配置

> 分发前必须复核：所用 FFmpeg 构建的 `--enable-gpl` 与具体库列表，并同步更新本节。
> 若替换为其他构建，请同时更新上面的版本串与源码链接，并确认 `bin/` 内的文件清单仍然完整。

## 字体

### HarmonyOS Sans SC（`assets/fonts/`）

```text
Copyright 2021 Huawei Device Co., Ltd.
HarmonyOS Sans Fonts Software is licensed under HarmonyOS Sans Fonts License Agreement.
完整条款：assets/fonts/HarmonyOS_Sans_LICENSE.txt
```

关键约束（摘要，以原文为准）：

- 允许随软件复制、嵌入、打包、再分发**未经修改**的字体副本；
- 必须在软件中**显著声明**使用了 HarmonyOS Sans 字体（本项目在「关于」页的「界面字体」一栏展示）；
- 不得修改字体本身，不得单独分发或出售字体文件。

### Microsoft YaHei

不随包分发，仅在用户系统已安装时作为**运行时回退**读取。Windows 用户需自行持有合法授权。

## Rust 依赖

通过 crates.io 引入，各自许可见 `cargo license` 或 `Cargo.lock` 对应 crate。

主要依赖：

| crate | 许可 | 用途 |
|---|---|---|
| eframe / egui / epaint / ecolor | MIT OR Apache-2.0 | GUI 框架与渲染 |
| wgpu / winit | MIT OR Apache-2.0 | GPU 后端与窗口 |
| rust-i18n | MIT | 多语言 |
| directories | MIT OR Apache-2.0 | 配置目录 |
| dark-light | MIT | 系统主题探测 |
| rfd | MIT | 原生文件对话框 |
| image | MIT OR Apache-2.0 | 图标解码 |

构建期专用（`xtask`）：

| crate | 许可 | 用途 |
|---|---|---|
| resvg / usvg / tiny-skia | MPL-2.0 | SVG 栅格化 |
| ico | MIT | 生成 `.ico` |

> MPL-2.0 是文件级 copyleft，只影响 `xtask` 构建工具，不传染到 ClipForge 主体代码。

## 图标与 Logo

`assets/logo/` 下的全部 SVG 与 `assets/icons/` 下的 PNG/ICO 均为本项目原创，随项目以 GPL-3.0-or-later 发布。

修改图标后重新生成：

```bash
cargo run -p xtask -- icons
```
