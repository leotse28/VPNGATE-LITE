# 图标统一 · 准备简报 (2026-11-08 11:35 SGT)

Leo 11:30 发了 3 张截图:3 个图标不统一,第 1 张是托盘图标,他认为托盘图标不好看,
要求学习网上优秀 icon 设计、重新设计后统计更改这 3 个图标。

## 三个图标盘点 (已核实)

| # | 位置 | 截图里是什么 | 实际来源 |
|---|------|------------|---------|
| 1 | 系统托盘 | 新版白 V + 实心靛蓝底 (截图里出现两次,原因未明:可能是两个实例在跑,或托盘+溢出区各一个) | `assets/icon.ico` 的 256px 条目,`src/tray.rs` 的 `largest_png_in_ico` 专门挑最大条目,256x256 交给 tray-icon 后由 Windows 缩到 16px —— 不是用 .ico 里原生 16px 条目 |
| 2 | 桌面 exe | **旧版**地球 + swoosh 渐变图标 | 已证实不是 v3.13 构建问题:用 pefile 验过 `VPNGATE-LITE-windows-v3.13.zip` 里的两个 exe,4 个 RT_ICON 条目 md5 与当前 `assets/icon.ico` 的 16/32/48/256 条目逐字节一致。所以 Leo 桌面那个文件要么还是 v3.12 的旧 exe 没换掉,要么是 Windows 图标缓存没刷新。核对办法:看 GUI 里版本号是不是 0.3.13,或右键 exe → 属性 → 详细信息 → 文件版本;0.3.12 就是旧文件,0.3.13 就是缓存问题(改名/清 IconCache/重启资源管理器可解) |
| 3 | 第三张 | 灰底小尺寸渲染,上半蓝弧 + 下半白色,疑似旧地球图标的小尺寸形态 | **未确认是哪个位置** —— 可能是任务栏运行图标/窗口标题栏/其他小尺寸处。不要猜,先问 Leo 第三张截的是哪里 |

单一事实来源:全部三个位置最终都来自 `assets/make_icon.py` → `assets/icon.ico`
(16/32/48/256)。`.ico` 是 gitignored 的生成物,不进仓库;`build.rs` 找不到 windres
时会静默无图标构建(这次不是这种情况,已验过)。

## 为什么托盘图标不好看 (已查资料)

- Microsoft 官方《Construct your Windows App's Icon》:托盘/标题栏/右键菜单用 16px
  (高 DPI 下 20/24/32/48/64);Windows 先找精确尺寸,没有才从更大尺寸往下缩;
  最低应提供 16/24/32/48/256;透明底最好,带底色的图标要准备深浅两套。
  来源: https://learn.microsoft.com/en-us/windows/apps/design/iconography/app-icon-construction
- 托盘图标行业惯例 (WireGuard / Tailscale 等):16px 单色线形 glyph、无底色 tile,
  靠系统/主题着色;macOS 叫 template image。实心大色块 tile 在托盘里和一排细线形
  系统图标放一起显得特别重、特别糊 —— 这正是 v3.13 托盘图标的问题:实心靛蓝方块 +
  白 V,16px 下 V 的笔画几乎贴到 tile 边缘。
- 另外一个技术点:托盘现在拿 256px 条目让 Windows 缩放,画质不如直接用 .ico 里
  原生 16px 条目;`tray.rs` 改一行即可 (`largest_png_in_ico` → 取 16px 条目)。

## 本文件夹的草稿 (只是方向,不是定稿)

- `tray-A-indigo-V-16/32.png`: 无 tile 靛蓝 V —— 浅色任务栏清晰,深色任务栏看不见
- `tray-B-white-V-16/32.png`: 无 tile 白 V —— 深色任务栏清晰,浅色任务栏看不见
- `tray-C-tile-V-16/32.png`: 保留 tile 但 V 边距加大 —— 深浅都可见,但仍是色块
- `tray-compare-sheet.png`: 四者在浅/深底上的 4 倍放大对照
- `current-tray-16/32.png`: 当前托盘渲染(作对照)

结论:单色无 tile 的 glyph 无法同时在深浅任务栏上都可见 —— 要么做深浅两套 +
主题检测切换,要么保留 tile(C 方案),要么给 glyph 加描边/衬线。这是留给 Leo 定的
关键选择,不要替他拍板。

## 改动清单 (给主 agent / v3.13.x)

1. 问清两件事:第三张截图是哪个位置;桌面 exe 版本是 0.3.12 还是 0.3.13。
2. 定托盘方案:A/B 双主题切换,还是 C 保留 tile,还是带描边的 glyph。
3. `make_icon.py` 按定稿重画 → 重新生成 `icon.ico` (16/24/32/48/256,建议补 24px)。
4. `src/tray.rs`:托盘改用 16px 原生条目(另加 20/24px 备选,按 DPI 选)。
5. `build.rs` 不用动;重新交叉编译 → 验 zip 里 exe 的 RT_ICON md5 → 发 v3.13.x Release。
6. Leo 那边:换新 exe 后若桌面仍显示旧地球图标,清 Windows 图标缓存。

## 参考

- https://learn.microsoft.com/en-us/windows/apps/design/iconography/app-icon-construction
- https://www.deskifier.com/blog/electron-tray-icon-guide (各平台托盘尺寸与 template image)
