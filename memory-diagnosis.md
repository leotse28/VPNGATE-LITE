# VPNGATE-LITE v3.14 内存诊断报告

- **日期**：2026-11-08
- **对象**：`vpngate-gui.exe` 0.3.13 → 0.3.14，`~/workspace/vpngate-rust-client/`
- **现象**：Leo 任务管理器截图：`vpngate-gui.exe` **204.5 MB**，CPU 0%，磁盘 0 MB/s，网络 0 Mbps；系统内存 87%。Leo 称内存“越来越大了”（随使用时间增长，不只是启动就大）。
- **方法**：只读诊断，未改任何代码文件，未编译，未推送。证据来自源码行号 + `Cargo.toml` / `Cargo.lock` + 本机（Linux VM）实测文件尺寸 + `~/.cargo/registry` 中 eframe/egui/epaint 0.36.2 的源码（字形缓存与渲染后端行为以该版本源码为准）。
- **先说结论**：
  1. **没有找到单一“爆炸式”泄漏能单独解释 204.5 MB。** 204.5 MB 更像“高基线 + 多个小增长项叠加”。基线大头是可数的：整份中文字体常驻内存约 15–22 MB + 字形图集 CPU 镜像最高 16 MB + wgpu/DX12 运行时 + 39 MB 的 exe 映像 + tokio 多线程运行时。
  2. **真正随时间单调增长（不回落）的已证实机制只有一个**：隧道 TCP 会话表 `SstpTunnel::sessions` ——正常关闭的会话永不从表里删除，只在断开连接丢弃整条隧道时才清掉（见 §4.2）。但它每会话只有几百字节，**量级太小，单独成不了 204 MB 的主因**。
  3. **最可能让 Leo 体感“越用越大、断开也不回落”的机制**：`serve_mixed` 里每个接入连接是脱离父任务的 `tokio::spawn`（`src/socks5.rs:216`），`disconnect()` 只 abort `serve` 列表（`src/bin/vpngate-gui.rs:1900-1906`），杀不掉这些子任务；只要手机/浏览器还有一条卡住的长连接（如 Leo 报告的 Stripchat 式卡住、YouTube Live 迟滞），它就一直持有 `Arc<SstpTunnel>`，隧道、TLS、读写任务、全部会话表都释放不了（见 §4.1）。
  4. **系统内存 87% 不是本程序造成的**：204.5 MB 只占 8 GB 内存的约 2.5%、16 GB 的约 1.3%。87% 是整机状态，别把两者因果倒置；但本程序 204 MB 的基线确实偏高，值得砍。
  5. **在拿到真机 RSS 曲线之前，任何“改完能省 XX MB”的数字都是估计。** 本报告凡写“估计”处均为估计，凡写“实测”处均在 VM 上量过；凡写“无法在 VM 上量，需真机日志”处就是缺真机数据，不要拿估计当承诺。

---

## 0. 基线分解（204.5 MB 大概由什么组成）

| 成分 | 量级 | 依据 | 性质 |
|---|---|---|---|
| exe 文件本身 | 磁盘 39 MB（实测 `target/x86_64-pc-windows-gnu/release/vpngate-gui.exe`） | `ls -lh` 实测 | 代码段按需换入，RSS 贡献小于 39 MB，但 39 MB 的体量说明 wgpu + tokio + rustls/native-tls + eframe 全套都在里面 |
| 中文字体全文件常驻 | **约 15–22 MB** | 见 §1 | 启动时一次性，不是增长，但基线最大单项之一 |
| egui 字形图集 CPU 镜像 | 初始 2048×32 很小，随不同汉字增多按 2 倍长高，**上限 2048×2048×4 B = 16 MB** | epaint 0.36.2 `texture_atlas.rs` / `fonts.rs:975-977` | 随使用增长，有顶；到 80% 满时整表重建（见 §1） |
| 字形 GPU 纹理 | 与 CPU 镜像同尺寸，最高 16 MB | 同上；但这是显存/共享显存，不一定计入任务管理器“内存”列 | 增长有顶 |
| wgpu / DX12 运行时 | **无法在 VM 上量，需真机日志**；估计为数十 MB 级（见 §2，估计） | eframe 0.36 默认 wgpu，见 §2 | 启动时一次性 + 驱动分配 |
| tokio 多线程运行时 | 每个工作线程栈虚拟保留 2 MB（未用不计 RSS）；任务/通道堆内存随连接数浮动 | `src/bin/vpngate-gui.rs:4316` `Runtime::new()` | 基线 + 随连接浮动 |
| 隧道 TCP 会话表残留 | 每会话约 0.2–0.5 KB（估计），1000 个会话约 0.2–0.5 MB | §4.2 | **真正单调增长，但量级小** |
| 每帧临时分配 | 每帧约 30–50 KB（估计），帧后释放 | §3 | 推高分配器高水位，不单调增长 |
| 界面日志 `self.log` | 100 行上限，约 5–10 KB | 已核实排除项，`vpngate-gui.rs:1579-1588` | 不增长（复核通过） |
| `probes` / `probe_ms` / `selected` / `blocked` | 以节点数（~125）为界，约几十 KB | 已核实排除项 | 不增长（复核通过） |

> 复核已核实排除项：`log_line_at`（`src/bin/vpngate-gui.rs:1579-1588`）`push` 后 `len() > 100` 即 `drain(..excess)`，确实有界；`probes` / `probe_ms` 以节点 host 为键，节点数 ~125，确实有界。排除成立，不再查。

---

## 1. 嫌疑一：egui 字形缓存 / 纹理图集 —— 基线最大头 + 随使用增长（有顶）

### 1.1 文件:行号证据

- **整文件读入内存，永不释放**：
  - `src/bin/vpngate-gui.rs:863-908` `find_cjk_font()`：按顺序试 `C:\Windows\Fonts\msyh.ttc` → `msyhbd.ttc` → `simsun.ttc` → `simhei.ttf` → macOS PingFang → Linux NotoSansCJK，第一个能 `std::fs::read` 成功的就整个 `Vec<u8>` 返回。
  - `src/bin/vpngate-gui.rs:1278-1290` `setup_visuals()`：`fonts.font_data.insert("cjk", Arc::new(egui::FontData::from_owned(bytes)))`，把整份字体放进 `FontDefinitions`，再 `cc.egui_ctx.set_fonts(fonts)`。`from_owned` 的语义是取得 `Vec<u8>` 所有权（epaint 0.36.2 `text/fonts.rs:112-136`：`FontData { font: Cow<'static, [u8]>, index: u32, … }`，`from_owned` 即 `Cow::Owned`）。此后没有任何代码释放它、替换它或重新 `set_fonts`。
  - **注意一个细节**：`FontData::from_owned` 默认 `index: 0`（`fonts.rs:128` 附近），而 `msyh.ttc` 是 TTC 集合（含 “Microsoft YaHei UI” 与 “Microsoft YaHei” 两个 face，见搜索结果 [5]），`index: 0` 只用其中一个 face，但**整个 TTC 文件仍全量在内存里**，另一个 face 的字节是纯 overhead。
  - 另外 `FontDefinitions::default()` 自带的默认字体（Hack 303 KB + Ubuntu-Light 354 KB + NotoEmoji 409 KB + emoji-icon 317 KB，本机 `epaint_default_fonts-0.36.2/fonts/` 实测，共约 1.4 MB）也被一起保留，因为 `setup_visuals` 是在 `default()` 上 `insert`，不是替换。量小，顺带注明。
- **字形缓存无逐出，只有整表重建**（epaint 0.36.2 源码，`~/.cargo/registry/.../epaint-0.36.2/src/text/`）：
  - `font.rs:367,375`：`FontFace` 有 `glyph_id_cache: HashMap<char, …>`、`advance_width_cache`、`glyph_alloc_cache: HashMap<GlyphCacheKey, GlyphAllocation>`，三个缓存**没有任何 `remove` / `retain` / 容量上限**；`font.rs:633` 是 `entry(...).or_insert_with(allocate_glyph_uncached)`，只增不减。渲染过的每一个不同字符（每种字号/位置组合）都在这里留一条。
  - `texture_atlas.rs:allocate()`：新字形在图集里占一行位置；`resize_to_min_height` 把图集高度每次翻倍；`texture_atlas.rs:fill_ratio()` 满了就 `overflowed = true`，光标回绕到 1/3 高度处覆盖旧字形。
  - `fonts.rs:730-745` `Fonts::begin_pass()`（每帧调用）：`font_atlas_almost_full = atlas.fill_ratio() > 0.8` 时**整个 `Fonts` 重建**：`definitions.clone()`（`FontData` 在 `Arc` 里，克隆只是 `Arc` 计数，不复制字体字节）+ 新的空 `FontsImpl` + 空 `GalleyCache`。所以字形缓存的增长是**锯齿形、有顶的**：涨到 80% 满就一次性清零重光栅化，不是无限单调。
  - `fonts.rs:1286-1290` `GalleyCache::flush_cache()`：每帧只保留本帧用过的 galley，其余丢弃。**文本排版缓存（galley）不是泄漏**，每帧都清——这点要给 egui 记一功，别误伤。
  - 可观测接口已存在：`Fonts::font_atlas_fill_ratio()`（`fonts.rs:796,908`），`egui::Context::fonts(|f| …)`（egui 0.36.2 `context.rs:1097`）——测量方案（§7）直接用它读图集填充率，不用猜。

### 1.2 增长机制

启动时：`msyh.ttc` 全量（约 15–22 MB）+ 默认字体约 1.4 MB 一次性进内存。
使用中：界面每出现一个以前没渲染过的汉字（节点国家/ISP 名、日志行里的新词、用户输入、错误信息），就发生：① `glyph_id_cache` + `glyph_alloc_cache` 各增一条；② 图集里多占一块约 20×20 px 的位置；③ 图集 CPU 镜像翻倍长高（32 → 64 → … → 2048 行）。`TextOptions::default()`（`epaint-0.36.2/src/text/mod.rs:59`）`max_texture_side: 2048`，所以 CPU 镜像顶在 2048×2048×4 B = **16 MB**，GPU 纹理同尺寸另算。涨到 80% 满后整表重建、清零，再重新涨——锯齿，不是直线。
本程序的汉字增量来源是真实存在的：125 个节点的国家/ISP（日/韩/美/新/德……几十个不同汉字）、探测/连接/刷新日志的中文句子、手机端浏览产生的 `ConnLog` 行都会把新汉字带进渲染。

### 1.3 量级估计

| 项 | 数值 | 性质 |
|---|---|---|
| `msyh.ttc` 文件本身 | **无法在 VM 上量（VM 没有 Windows 字体），需真机核对**；公开资料区间为 **约 15–22 MB**：有来源称原文件 22.5 MB（iopenv 引述）、有来源称 `msyh.ttf` 约 14.3 MB / 约 16 MB / 约 19 MB（不同 Windows 版本/拆分方式）。取区间，不取单点。 | 启动一次性 |
| VM 类比实测 | 本机 `/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc` **19 MB**（`ls -lh` 实测，含 JP/HK/KR/SC/TC 五个 face 的 TTC）。`find_cjk_font` 在 Linux 走的就是它，量级与 `msyh.ttc` 同档。 | 实测 |
| 图集 CPU 镜像上限 | 2048×2048×4 B = **16.0 MB**（算术）；2048×1024 = 8 MB。当前实际值未知，需真机读 `font_atlas_fill_ratio()`。 | 随使用增长，有顶 |
| 每个不同字形的缓存条目 | `GlyphCacheKey` + `GlyphAllocation` + `glyph_id_cache` 条目，约几十字节（估计）；500 个不同汉字约几十 KB，可忽略；图集像素才是大头（500 字 × 约 20×20 px × 4 B ≈ 0.8 MB，算术）。 | 小 |
| **合计（字体 + 图集满载）** | **约 31–38 MB**，占 204.5 MB 的约 15–18% | 基线 + 有顶增长 |

### 1.4 修复建议（具体到函数/配置名）

按性价比排序（详见 §6，这里只列本节的）：

1. **先测量，别先动字体**（§7）：在 RSS 日志里同时记 `ctx.fonts(|f| f.font_atlas_fill_ratio())` 和 `font_image_size()`。如果填充率长期 < 0.3，图集就不是增长主因，别动。
2. **子集化字体（省得最多，但风险最高，需 Leo 单独拍板）**：用 `fonttools` 把 `msyh.ttc` 的 index 0 face 子集化为常用汉字（Latin + GB2312 约 6700 字，公开做法称可压到每份约 3 MB，见搜索结果 [8] 的 subset-fonts.md），随程序发一个子集字体文件，改 `find_cjk_font()` 优先读它、读不到再回落系统字体。**不要直接改 `find_cjk_font` 的候选顺序去绕开系统字体而不给回落**。
3. **不推荐的捷径**：`FontData::from_owned` 改 `from_static` / `mmap` 零拷贝——egui 的 `Cow<'static, [u8]>` 要求 `'static`，mmap 的生命周期管理要 `leak`，收益只是“同一份字节不复制”，而现在本来就只复制一次（`fs::read` 的那一份），省不掉 15–22 MB 的常驻，性价比低，别做。
4. **可调的配置**：`egui::TextOptions { max_texture_side }`——目前代码没传自定义 `TextOptions`，用的是默认 2048。若真机证实图集常满，可在 `setup_visuals` 后用 `ctx.options_mut(|o| …)` 之类途径调小（需核对 0.36 的 `Options` 字段名再动手；本报告不编 API）。调小会更早触发重建（重光栅化卡顿），是拿卡顿换内存，**不到万不得已不动**。
5. **不要做的事**：定期 `set_fonts` 强行清图集——会造成全界面文字一帧闪烁/重光栅化，且 `begin_pass` 在 80% 时已经自动重建，手动清没有额外收益。

### 1.5 风险

- 子集化：缺字变“□”（tofu）。本程序界面汉字集不固定（节点国家/ISP 名来自 vpngate.net，是开放集），子集一旦漏了某个国家名就直接显示方框。**必须保留系统字体回落**，或把子集做得足够大（GB2312 全量），否则中文显示必受损。
- 调小图集：重建更频繁，滚动节点列表时可能出现文字短暂重光栅化卡顿。
- 测量本身：零风险（只读 `font_atlas_fill_ratio()`）。

---

## 2. 嫌疑二：eframe 默认渲染后端 wgpu —— 基线大头之一，但不是“增长”

### 2.1 文件:行号证据

- `Cargo.toml:37`：`eframe = "0.36"`，**未指定任何 `features`**，即默认特性全开。
- eframe 0.36.2 `Cargo.toml.orig`（本机 registry）：`default = ["accesskit", "default_fonts", "links", "wayland", "web_screen_reader", "wgpu", "winit/default", "x11"]`；`glow` 是可选特性，**默认关闭**。`wgpu = ["wgpu_no_default_features", "egui-wgpu/default"]`。
- `eframe-0.36.2/src/epi.rs:598-619` `Renderer::default()`：只有 glow 时返回 `Glow`，只有 wgpu 时返回 `Wgpu`，两者都有时**优先 `Wgpu`**。本程序 `src/bin/vpngate-gui.rs:4318-4330` `main()` 里 `NativeOptions { viewport: …, ..Default::default() }`，**没有设置 `renderer`**，所以实际跑的是 **wgpu**。
- `Cargo.lock` 核对：`eframe 0.36.2`、`egui-wgpu 0.36.2`、`wgpu 30.0.1` 均在列；`egui_glow 0.36.2` / `glow 0.17.0` / `glutin` 也在 lock 里——那是 `egui_kittest` 的 `wgpu` 特性与 eframe 的可选依赖解析带来的（lock 包含可选依赖不代表编译进 release 二进制），**不能从 lock 里有 `egui_glow` 就推断程序用了 glow**。判定以 `Cargo.toml` 特性 + `Renderer::default()` 为准：**当前是 wgpu**。
- eframe 官方在 `Cargo.toml.orig:88` 明确写了：`Switching from wgpu (the default) to glow can significantly reduce your binary size`。注意它说的是 **binary size（体积）**，不是 RSS。

### 2.2 增长机制

wgpu 本身不是泄漏：DX12 设备、着色器、交换链、命令缓冲、纹理上传队列在启动时建立，之后基本稳定。它贡献的是**高基线**，不是随时间增长。唯一随时间浮动的部分是每帧的纹理上传（图集 dirty 区域 `take_delta()`：`texture_atlas.rs` 的 `take_delta()`，整表重建时 `ImageDelta::full(self.image.clone(), …)` 会克隆整个图集镜像——16 MB 的一次性克隆，每帧一次，只在重建帧出现）和命令缓冲。`egui-wgpu` 的 `Renderer` 在 `renderer.rs:755` 有 `free_texture`，纹理释放路径存在，未发现泄漏。

### 2.3 量级估计（写清是实测还是估计）

- **估计，不是实测**：glow-only 能省多少 RSS，**无法在 VM 上量**（任务禁编译，且 VM 没有 Windows DX12/OpenGL 真机环境）。不要信任何没有出处的具体数字。
- 推理上限（估计）：省的部分 = ① wgpu 运行时常驻（设备、驱动分配、着色器缓存，量级估计为数十 MB，但无公开可引用的同版本对照数据）；② 二进制体积（eframe 官方只承诺 binary size 显著减小，未给 MB 数；本程序 exe 实测 39 MB，glow 版能小多少需编译后实测）。**两者都不等于 RSS 节省**，显存/驱动分配在任务管理器“内存”列里未必全现身。
- 反向约束：即使 glow 把 wgpu 的全部常驻都省掉，204.5 MB 里也还剩字体/图集/运行时等约 100+ MB，**glow 不是把 204 MB 变 50 MB 的魔法**。把它排在修复清单中段（§6），不是第一位。

### 2.4 修复建议

- 切换方法（供 v3.14 实施者核对，非本报告执行）：`Cargo.toml` 把 `eframe = "0.36"` 改为 `eframe = { version = "0.36", default-features = false, features = ["default_fonts", "accesskit", "glow", "winit/default"] }`（注意：去掉 `wayland`/`x11` 是 Linux 项，Windows 交叉编译本来就用不上；但要逐项核对，别照抄），并在 `main()` 的 `NativeOptions` 里显式写 `renderer: eframe::Renderer::Glow`。改完必须在 Leo 的 Windows 10 真机上跑通：窗口、中文、托盘、滚动、探测/连接全流程。
- **先做 A/B 测量再决定**：用 §7 的 RSS 方案分别量 wgpu 版与 glow 版的 T0 / T+5min / 连接 30min，三点对齐再比。没有 A/B 数据就不要合入。

### 2.5 风险

- **kittest 依赖 wgpu**：`Cargo.toml:70` `[dev-dependencies] egui_kittest = { version = "0.36", features = ["eframe", "snapshot", "wgpu"] }`。`egui_kittest 0.36.2` 的 `wgpu` 特性会拉 `egui-wgpu` + `wgpu` + `eframe?/wgpu`（本机 `egui_kittest-0.36.2/Cargo.toml` 实测）。所以 **glow-only 只能动 `[dependencies]` 的 eframe，不能动 `[dev-dependencies]`**；测试照常走 wgpu 渲染（`Harness` 的 `WgpuTestRenderer`，`egui_kittest-0.36.2/src/renderer.rs:26,39`），生产走 glow。后果：测试覆盖的渲染路径与生产路径分叉——截图/渲染测试仍能跑，但测的不是生产用的后端；发版前必须在真机上人工过一遍渲染回归（v3.13 的图标/滚动条回归就是前车之鉴）。
- **glow 在 Leo 机器上的 OpenGL 驱动风险**：Windows 上 glow 走 glutin/WGL，老 Intel 核显的 OpenGL 驱动质量不如 DX12 稳定，可能出现黑窗/闪烁/启动失败。必须保留 wgpu 回退方案（特性开关或双后端编译），不能把 wgpu 代码路径删掉。
- **隧道、中文显示本身不受影响**：后端只管画画，不管 SSTP/SOCKS5；中文显示只取决于 §1 的字体，与后端无关。

---

## 3. 嫌疑三：每帧分配 —— 推高水位，不单调增长

### 3.1 文件:行号证据（逐一列出）

每帧都跑的 `ui()`（`src/bin/vpngate-gui.rs:3066` 起，`impl eframe::App for GuiApp`）：

| # | 位置 | 代码 | 每帧分配 |
|---|---|---|---|
| 1 | `vpngate-gui.rs:4252` | `let log_text = self.log.join("\n");`（100 行日志拼成一个 `String`，随后 `TextEdit::multiline(&mut log_text.as_str())` 只读显示） | 1 次大分配，约 5–10 KB（100 行 × 每行约 50–100 字节） |
| 2 | `vpngate-gui.rs:3343-3405` | `let rows: Vec<(usize, String×9)> = self.display_indices().into_iter().map(…).collect();` | 约 125 行 × 9 个 `String` ≈ **1100+ 次小分配/帧**（`status` 的 `.cloned()`、`full` 的 `format!("{}:{}", …)`、`status_icon` 的 `"∞".to_string()` / `String::new()`、国家/ISP 的 `.clone()`、`sessions_cell` / `quality_cell` / `per_capita_cell` 各 1 次 `format!`、显示 host 的 `format!`，见 `vpngate-gui.rs:3348-3405`） |
| 3 | `vpngate-gui.rs:2347-2381` | `display_indices()`：`(0..self.nodes.len()).collect()` 一个 `Vec<usize>`（125 × 8 B = 1 KB），有排序时再 `sort_by` | 1 次分配/调用；本函数在行循环、`move_selection`（`vpngate-gui.rs:2480`）、`probe_all_targets`（`vpngate-gui.rs:1698`）等多处被调用，**每帧至少 1 次，行循环里是 1 次**（`rows` 构建时调 1 次，`vpngate-gui.rs:3343`） |
| 4 | `vpngate-gui.rs:3339` | `let connected_host = self.connected_node_host();`，内含 `vpngate-gui.rs:2458` `self.conn.as_ref()?.node_label.clone()` + `rsplit_once` 后的 `to_string()` | 每帧 1–2 次小分配（只在已连接时有值，未连接时 `None`，约 0 次） |
| 5 | `vpngate-gui.rs:2322-2331` | `drain_events()`：`let mut events = Vec::new(); while let Ok(ev) = rx.try_recv() { events.push(ev); }` | 平时 0–1 个事件，分配可忽略；事件 burst 时随事件数增长，但帧后释放 |
| 6 | `vpngate-gui.rs:3144` | 仅托盘隐藏时：`ctx.request_repaint_after(250ms)`，即隐藏后仍以 **4 fps** 重绘，上面 1–5 项每 250 ms 跑一遍 | 隐藏≠停帧，别指望藏托盘就省分配 |
| 7 | `vpngate-gui.rs:1508-1516` | `auto_save_if_dirty()` 每帧末调用：`current_config()` 里 `self.config.clone()` + 若干 `String::clone()`（`vpngate-gui.rs:1428-1458`），**只在 dirty 时才写文件**，不 dirty 时只有几次克隆 | 每帧少量克隆，无文件 I/O（除非真改了设置） |
| 8 | `vpngate-gui.rs:1547` | `init_file_log` 里 `for line in self.log.clone()`——只在启动时跑一次，不在每帧 | 非每帧，排除 |

另外注意：`rows` 的 10 元组里 `status` 是 `probes.get().cloned().unwrap_or_else(|| "未探测".to_string())`（`vpngate-gui.rs:3348-3352`）——125 行里未探测的行每行都新建一个 `"未探测"` 字符串；`country` / `isp` 的 `.clone()` 同理。这些都是**纯显示用、帧后即弃**的分配。

帧率约束（重要）：eframe 默认不是 60 fps 常驻重绘——没有动画/持续 `request_repaint` 时只在有事件（输入、通道事件、`request_repaint_after`）时才跑 `ui()`。所以上面每帧约 30–50 KB 的分配，**只在有交互/有后台事件/托盘隐藏（4 fps）时才发生**，静置不动的窗口不烧分配。Leo 截图时 CPU 0% 与这一点一致。

### 3.2 增长机制

Rust 默认分配器在 Windows 上是系统堆（`GlobalAlloc` 行为）：小块反复分配/释放会产生**堆碎片 + 自由块不归还 OS**，RSS 表现为“涨上去就下不来”的高水位，而不是严格单调泄漏。`rows` 每帧 1100+ 次小分配、持续数小时（尤其托盘隐藏 4 fps + 手机浏览不断触发 `ConnLog` → `request_repaint`），足以把堆的高水位顶上去几个 MB，但**解释不了几十 MB 的增长**。它是放大器，不是主因。

### 3.3 量级估计

- 每帧：`rows` 约 125 行 ×（9 字符串 × 平均约 20–40 B + `String` 头 24 B）≈ **25–40 KB** + `log_text` 约 5–10 KB + `display_indices` 1 KB ≈ **30–50 KB/帧**（估计）。
- 累计分配量（估计）：托盘隐藏 4 fps × 3600 s = 14400 帧/小时 × 40 KB ≈ **576 MB/小时的累计分配**，但 99% 以上帧后释放，RSS 只留高水位。**不要把“累计分配”当成“泄漏量”**。
- RSS 高水位影响（估计）：**几个 MB 级**，无法在 VM 上量，需真机 RSS 曲线 + 堆统计佐证。

### 3.4 修复建议

1. **缓存 `rows`，只在节点/探测/排序变化时重建**（改 `GuiApp`：加 `rows_cache: Option<…>` + 脏标记，在 `apply_event` / `toggle_sort` / `delete_selected` / 刷新时置脏）。收益小、改动面大（行元组 10 个字段全是 `String`，缓存本身也要持有它们），**排最后**。
2. **低成本先做**：`log_text` 别每帧 `join`——把 `TextEdit::multiline` 改为直接读 `self.log` 的只读展示，或缓存 `log_text` 只在 `log_line_at` 时重拼（`vpngate-gui.rs:1579-1588` 是唯一写日志处，重拼点唯一）。改动小，省每帧一次 5–10 KB 分配。
3. **`display_indices` 缓存**：同 1，排序结果在 `nodes`/`sort`/`probe_ms` 不变时不变，可缓存 `Vec<usize>`。但 `probe_ms` 每探测一个节点就变，缓存失效频繁，收益有限。
4. **别做的事**：为省这点分配去改 `sessions_cell` / `quality_cell`（`vpngate-gui.rs:124-168`）的 `format!` 为静态字符串——它们本来就是每行都不同的数字，省不掉。

### 3.5 风险

- 低。只动显示层缓存，不碰隧道、中文显示（字符串内容不变）、kittest（行循环的输出不变，测试读的是同一批 label）。唯一风险是缓存失效漏了某个置脏点导致列表显示 stale——必须把所有改 `nodes`/`probes`/`probe_ms`/`sort` 的入口都列出来对一遍。

---

## 4. 嫌疑四：后台任务与通道

### 4.1 `serve` 的 `JoinHandle`：abort 不干净 —— 已证实的部分泄漏路径

**证据**：

- `src/bin/vpngate-gui.rs:201-216` `ActiveConnection { serve: Vec<tokio::task::JoinHandle<()>>, … }`；`connect_flow`（`vpngate-gui.rs:466-478`）每个监听地址 `tokio::spawn(serve_mixed(…))` 推入 `serve`。
- `src/bin/vpngate-gui.rs:1900-1906` `disconnect()`：`for s in c.serve { s.abort(); }`，然后 `proxy_guard.unset()`，`log_line(已断开…)`。`ActiveConnection` 随 `self.conn.take()` 被 drop。
- **但 `serve_mixed` 内部的每个接入连接是脱离的 `tokio::spawn`**：`src/socks5.rs:213-221`，`tokio::spawn(async move { handle_mixed_conn(socket, t, …).await })`，返回值（`JoinHandle`）被直接丢弃，**没有任何 `JoinSet` 持有它们**。`abort` 掉 `serve_mixed` 任务本身**不会** abort 这些子任务（tokio 的 spawn 是分离的，父任务取消不级联）。
- 子任务持有 `tunnel.clone()`（`src/socks5.rs:213` `let t = tunnel.clone();`），所以只要还有一个子任务活着，`Arc<SstpTunnel>` 的强引用就不归零 → 隧道不 drop → TLS 流不关闭 → 读写任务（`src/sstp.rs:815-851` 的 writer/reader，靠 `rx` 关闭 / `read_packet` 出错才退出）不退出 → `sessions` / `udp_flows` 两张表（`src/sstp.rs:757-758`）也不释放。
- 唯一被 `serve_mixed` 持有、abort 时能带走的只有 UDP 回复转发器：`src/socks5.rs:197-199` `forwarders: JoinSet<()>` 是 `serve_mixed` 栈上的局部量，abort 时 drop 掉，转发器被取消——这点是干净的（注释里也写了）。**TCP 接入任务没有同样的待遇**，是不对称的。
- `ConnLog` 转发器（`src/bin/vpngate-gui.rs:1847-1852`）倒是被推进了 `c.serve`，disconnect 时会被 abort——这条干净。问题只在 `socks5.rs:216` 的接入任务。

**增长机制**：Leo 的使用场景正好踩中这条：手机经 10818 共享代理上网，浏览器/ App 会开大量长连接、keep-alive、流媒体（他报告的 YouTube Live 能播但卡、Stripchat 类站点卡住——卡住的连接正是“不结束”的连接）。点“断开”后，这些子任务只要对端不关闭、本地 `timeout` 不触发，就一直挂着；`handle_mixed_conn` 里 `copy_bidirectional`（`src/socks5.rs:445,1335`）在两端都静默时就是 `Pending`，不消耗 CPU（与截图 CPU 0% 一致），但一直占着任务 + 缓冲区 + 会话表项。**断开≠释放**，内存高水位被这些残留任务钉住；反复“连接→断开→换节点连”会让残留任务叠加（每次断开都可能留几个）。

**量级估计**：每个残留接入任务约：tokio 任务 future（几 KB，估计）+ `copy_bidirectional` 双向各 8 KB 缓冲（tokio 1.54.1 `DEFAULT_BUF_SIZE = 8 * 1024`，`src/io/util/mod.rs:88`，实测源码）+ `TunnelStream { pending: VecDeque<u8> }`（`src/sstp.rs:601-604`，已收未读的字节，卡住的流可达数 KB）+ 会话表项。**单个约 20–40 KB（估计）；10 个残留约 0.2–0.4 MB；100 个约 2–4 MB。** 单独看不大，但它钉住的是**整条隧道**（TLS 会话、读写任务、两张表、UDP relay），隧道本身的常驻无法释放。**无法在 VM 上量具体残留数，需真机日志**（§7 的日志里要记断开后残留任务数——这需要先加一个计数器，见 §4.1 修复）。

**修复建议**：

1. 在 `serve_mixed` 里把接入任务收进一个 `JoinSet`（与 `forwarders` 同款，`src/socks5.rs:197` 已有先例），`serve_mixed` 退出时 `abort_all()`；或把 `JoinHandle` 集合回传给 `ActiveConnection::serve`，让 `disconnect()` 一并 abort。**这是本报告建议的第二优先修复**（仅次于 RSS 测量）。
2. 给每个接入任务加可观测性：一个 `AtomicUsize` 计数器（进入 +1、结束 -1），断开时记进日志（§7 的 `disconnect` 记录点要带这个数）。
3. 更彻底但改动大：给 `handle_mixed_conn` 的 `copy_bidirectional` 加总超时/空闲超时（现在只有建立阶段的 `timeout`，传输中没有空闲超时），让卡住的流媒体连接能被回收。先做 1、2，再评估 3。

**风险**：中低。改任务所有权不碰隧道协议、不碰中文、不碰 kittest（kittest 不跑真实 `serve_mixed` 长连接）。风险点：abort 时机——正在传输的连接会被立刻掐断，这是断开本来的语义（用户点了断开），与现有行为一致；但要确保 abort 不发生在 `connect_flow` 的系统代理回滚路径（`vpngate-gui.rs:483-490` 已有 abort 循环，那里也要同步改）。

### 4.2 `ConnLog` 的 unbounded channel：平时不积压，卡住时能积压

**证据**：

- `src/socks5.rs:138`：`pub type ConnLog = tokio::sync::mpsc::UnboundedSender<String>;`，`emit_conn_log`（`src/socks5.rs:140-143`）`let _ = tx.send(line);`——无界，发送永不阻塞、永不失败反馈。
- 生产链路：`src/bin/vpngate-gui.rs:1831-1833` 每连接建 `tokio::sync::mpsc::unbounded_channel::<String>()`（`conn_tx` / `conn_rx`）；转发器（`vpngate-gui.rs:1847-1852`）`while let Some(line) = conn_rx.recv().await { tx2.send(GuiEvent::ConnLog(line)) }`，`tx2` 是 `std::sync::mpsc::Sender<GuiEvent>`（`vpngate-gui.rs:1102-1103` 的 `mpsc::channel()`，**标准库 channel 也是无界的**）。
- 消费侧：`drain_events()`（`vpngate-gui.rs:2322-2331`）每帧开头 `while let Ok(ev) = rx.try_recv() { events.push(ev); }` 再逐个 `apply_event`；`GuiEvent::ConnLog(line)`（`vpngate-gui.rs:2269-2271`）直接 `self.log_line(line)`，进 100 行 cap 的界面日志 + 文件日志（`filelog.rs:178` 每行 `write_all` + `flush`，磁盘行为，不占内存）。
- 消费是真实在跑的：`ui()` 每帧第一行就是 `drain_events()`（`vpngate-gui.rs:3071`），托盘隐藏时还有 250 ms 的 `request_repaint_after`（`vpngate-gui.rs:3144`）保底。所以**正常浏览时不积压**——生产者（每 TCP 连接 1–2 行）远慢于消费者（每帧全 drain）。
- 积压窗口只在一种情况下打开：**UI 帧停了，但生产者没停**。例如窗口隐藏后 eframe 的重绘被系统挂起、或某一帧被图集重建卡住时，`conn_rx` → `tx2` → `event_rx` 三级队列里攒行。每行约 50–150 字节，攒 1000 行才约 0.1 MB。**量级小**，不是主因，但它是无界设计，要记一笔。

**同款无界通道（顺带核查）**：

- 隧道内每 TCP 会话一个 `mpsc::unbounded_channel::<Vec<u8>>`（`src/sstp.rs:1163`，`payload_tx` / `payload_rx`）；`handle_tcp_packet`（`src/sstp.rs:1114-1118`）收到包就 `tx.send(chunk)`，每块 1300 B（`TCP_MSS = 1300`，`src/sstp.rs:41`）。如果消费端（`TunnelStream::poll_read`，`src/sstp.rs:630-660`）停读——比如下游 App 卡住不读——包会在这个无界队列里一直攒，**这是全程序里最有“无界积压 → MB 级”潜力的通道**。一个卡住的视频流攒 1000 个包就是 1.3 MB。配合 §4.1 的残留任务，风险是真实的。
- 隧道出站 `tx: mpsc::UnboundedSender<Vec<u8>>`（`src/sstp.rs:756`）：writer 任务（`src/sstp.rs:815-830`）一直在 drain，TLS 写阻塞时才会攒，窗口小。
- `PacketReader.buf`（`src/sstp.rs:167-168,178-192`）：`read_exact_buf` 是“读够 n 字节就 `drain(..n)`”，SSTP 包长上限 0x0FFF（`src/sstp.rs:201` 的 `& 0x0FFF`），buf 不会无界长。排除。

**修复建议**：

1. `ConnLog`：把 `tokio::sync::mpsc::unbounded_channel` 改为有界（例如 512）+ `try_send` 失败即丢弃并计数（日志行丢了不影响隧道，界面日志本来就只留 100 行）。改动点：`vpngate-gui.rs:1831-1833`（建通道）+ `src/socks5.rs:138`（类型别名，涉及 `serve_mixed` 签名，要同步）。
2. `payload_tx`：不要轻易改有界——有界会反压 TCP，改不好会把下载卡死。先加观测：§7 日志里记 `sessions.len()` 与断开后残留；如果证实是它在涨，再做“会话关闭时丢弃未读包”的定向修复。
3. `GuiEvent` 的 `std::mpsc::channel`：`drain_events` 每帧全 drain，暂不动。

**风险**：`ConnLog` 改有界丢日志的风险极低（日志行可丢）；`payload` 通道不动，零风险。

### 4.3 probe-all 任务是否残留

**证据**：`start_probe_all`（`vpngate-gui.rs:1715-1750`）→ `spawn_task(probe_all_flow(…))`；`probe_all_flow`（`vpngate-gui.rs:348-398`）是单任务顺序循环，每步查 `stop`（`probe_all_stop: Arc<AtomicBool>`，`vpngate-gui.rs:955`），结束必发 `ProbeAllDone`。`spawn_task`（`vpngate-gui.rs:1630-1639`）用 `Handle::try_current()`，无 runtime 时 no-op。**没有残留**：同一时间只能有一个 `busy`（`vpngate-gui.rs:1600-1602` 的 `if self.busy.is_some() { return; }` 守卫），停止后当前节点测完即出循环；`targets` 是快照 `Vec<(String, String)>`（`vpngate-gui.rs:1692-1712`），约 125 × 2 字符串，不到 10 KB，随任务结束释放。**排除**。唯一的小尾巴：`probe_all_flow` 里每个 probe 都 `probe_flow` → `SstpTunnel::probe_any`，失败候选的 TLS 流靠 `JoinSet::abort_all` 清理（`src/sstp.rs:894-899`），注释称已处理，未发现反例。

### 4.4 tokio 运行时与 TLS 会话、DoH 缓存

- **tokio 运行时**：`main()`（`vpngate-gui.rs:4316`）`tokio::runtime::Runtime::new()` = 多线程 runtime，工作线程数默认 = CPU 核数。线程栈是虚拟保留，RSS 只计实际用的栈页；每个 `tokio::spawn` 的任务 future 是堆分配，任务结束即释放。**无常驻增长点**，排除。唯一注意：`start_test_front_proxy`（`vpngate-gui.rs:1881`）用的是裸 `tokio::spawn` 而不是 `spawn_task`，kittest 下无 runtime 会 panic——这是测试问题，不是内存问题，顺带记一笔，不修。
- **TLS 会话**：Windows 走 SChannel（`src/net.rs:143-166` `verified_tls_connect`，每次 `native_tls::TlsConnector::new()` 新建；SSTP 走 `accept_any_tls_connect`，`src/net.rs:205` 起同款每次新建）。**每次握手都新建 connector，没有复用、也没有缓存**——代价是每次握手多一点分配/CPU，不是常驻增长；SChannel 会话票据由系统层管理，不在进程堆里。`rustls` 的 `verified_tls_config()`（`src/net.rs:94-100`）只在非 Windows 编译，每次调用都重建 `RootCertStore`（`verified_roots()`，`src/net.rs:105-117`：Mozilla 根 + 系统根，几百个证书，约几 MB 的临时分配），但同样只在握手时存在。**结论：无缓存、无泄漏；若要省，可把 connector 做成 `OnceLock` 复用，但收益是 CPU/临时分配，不是 RSS，排最后。**
- **DoH 缓存**：没有。`src/doh.rs` 全程无 `HashMap` / `static` 缓存，每次 `resolve_ipv4` 都重新 DoH。无增长点，排除（顺带：这意味着每次建流都多一次 HTTPS，慢但不占内存）。
- **UDP 中继表**：`UdpRelay`（`src/socks5.rs:655-667`）`flows: Mutex<UdpFlowTable>`，`UDP_RELAY_MAX_FLOWS = 1024`（`src/socks5.rs:460`），满了踢最老的（`src/socks5.rs:622-630`），空闲 60 s 清扫（`UDP_FLOW_IDLE`，`src/socks5.rs:462`；清扫间隔 15 s，`UDP_SWEEP_INTERVAL`，`src/socks5.rs:464`）。**有界 + 有过期，排除**。`UdpFlow` 有 `Drop` 调 `close_udp_flow`（`src/sstp.rs:591-595`）——与 TCP 会话的无 `Drop` 形成对照，见 §4.5。

### 4.5 隧道 TCP 会话表：已证实的单调泄漏（量级小）

**证据**：

- `src/sstp.rs:757-758`：`sessions: Arc<Mutex<HashMap<u16, Arc<Session>>>>`，`udp_flows` 同款。
- `open_stream`（`src/sstp.rs:1155-1185`）：每次建流 `map.insert(src_port, session.clone())`。
- `remove_session`（`src/sstp.rs:1237-1243`）是唯一删表项的函数，调用点只有三处，全是握手失败：`src/sstp.rs:1200`（SYN 发送失败）、`:1221`（对端关闭/死亡）、`:1225`（握手 4 次超时）。
- **正常关闭路径不删**：`handle_tcp_packet` 里 RST（`src/sstp.rs:1105-1109`）与 FIN（`src/sstp.rs:1136-1144`）都只调 `session.close()`——而 `Session::close()`（`src/sstp.rs:529-538`）只置 `closed = true` + 丢 `payload_tx` + `notify_waiters()`，**不从 `sessions` 里 `remove`**。
- **`TunnelStream` 没有 `Drop` 实现**：全文唯一 `impl Drop` 是 `UdpFlow`（`src/sstp.rs:591`）；`TunnelStream`（`src/sstp.rs:599` 起）没有任何 `Drop`，所以连接处理完丢掉 `TunnelStream` 时，会话 Arc 仍被 `sessions` 表攥着，**直到整条隧道丢弃**。
- 对照：UDP 流有关闭路径（`UdpFlow::drop` → `close_udp_flow`，`src/sstp.rs:1324` 起），TCP 没有——这是不对称的 bug，不是设计。

**增长机制**：一条隧道连接期间，手机/浏览器每完成一次 TCP 连接（一次网页、一次 API、一次短视频分片），`sessions` 里就永久多一个死 `Session`（`SessionState` + `Mutex` + `Notify` + `payload_tx: Mutex<Option<…>>` 的空壳 + `HashMap` 表项）。隧道不断开，表只增不减。端口空间是 20000–59999（`src/sstp.rs:1158-1161` 的随机端口），理论上限 40000 项才会撞端口，但 Leo 的使用量远到不了上限——先到的是内存缓慢爬升。

**量级估计**：每 `Session` 约 0.2–0.5 KB（估计：结构体 + 两把 `Mutex` + `Notify` + `Arc` 控制块 + 表项开销）；**1000 个已关闭连接约 0.2–0.5 MB，10000 个约 2–5 MB**。要靠它单独涨到几十 MB，需要约 10 万次连接——不现实。**结论：机制已证实、方向正确、量级太小；修它便宜，但别指望它解释 204 MB。**

**修复建议**：给 `TunnelStream` 加 `impl Drop { fn drop(&mut self) { self.tunnel.remove_session(self.session.src_port); } }`（`remove_session` 已存在，`src/sstp.rs:1237`，直接复用）。注意：`remove_session` 里 `try_lock` 拿不到锁时静默跳过（`if let Ok(mut map)`），`Drop` 里不能阻塞，这点正好合适；还要确认 `open_stream` 握手失败路径已经 `remove_session`，`Drop` 再调一次是幂等的（`map.remove` 不存在即 `None`），无 double-free。

**风险**：低。只影响会话表生命周期，不碰协议、不碰中文、不碰 kittest（`TunnelStream` 的 drop 语义变了，但测试里 `open_stream` 的 mock 路径走同一条 `remove_session`，已存在且被测试覆盖）。唯一要小心的：`TunnelStream` 被 `poll_shutdown` 后、`poll_read` 返回 EOF 前，`Drop` 先删表项——此时对端重传的包在 `handle_tcp_packet` 里 `map.get(&dst_port)` 查不到会直接 `return`（`src/sstp.rs:1070-1076`），这是期望行为（连接已死）。

---

## 5. 其他常驻增长点

| 项 | 证据 | 结论 |
|---|---|---|
| 主线程 / eframe 事件循环 | `main()`（`vpngate-gui.rs:4313-4330`）标准 `run_native`，无自定义循环、无后台线程 | 无增长点 |
| 托盘 | `src/tray.rs:142-189` `TrayHandle::new()`：解码 `assets/icon.ico` 里最大的 PNG（256 px，`decode_tray_icon_rgba`，`tray.rs:126-140`，RGBA = 256×256×4 B = 256 KB，一次性），菜单 3 项；`drain_requests()`（`tray.rs:189`）每帧 `try_recv` 全 drain，无堆积 | 一次性约 256 KB + 句柄，无增长。v3.14 图标重绘不影响内存（图标文件 6.3 KB，`assets/icon.ico` 实测） |
| 定时器 | 全程序唯一的周期 tick 是 UDP 清扫 `tokio::time::interval(15s)`（`src/socks5.rs:200`）+ 托盘隐藏时的 `request_repaint_after(250ms)`（`vpngate-gui.rs:3144`） | 无累积状态；清扫本身是删表项的 |
| config 自动保存 | `auto_save_if_dirty()`（`vpngate-gui.rs:1508-1516`）每帧末调，dirty 才写文件；`current_config()`（`vpngate-gui.rs:1428-1458`）每帧克隆几个 `String`，无文件 I/O（不 dirty 时） | 无增长；注意 `save_settings_quiet` 失败时 `last_save_error` 只存一条（`vpngate-gui.rs:1496-1497`），有界 |
| 文件日志 | `FileLogger`（`src/filelog.rs:113-128`）：`MAX_LOG_BYTES = 2 MiB`（`filelog.rs:29`），超了 rename 为 `.old` 再新建；内存里只有 `bytes_written: u64` + `File` 句柄 | 磁盘 2 MiB 上限，内存无增长。`init_file_log` 的 `for line in self.log.clone()`（`vpngate-gui.rs:1547`）只在启动跑一次 |
| 节点缓存 | `nodes_cache.json` 读写（`src/vpngate.rs:696-717`）是整文件 `serde_json::to_string`，只在刷新成功时写一次 | 无常驻 |
| 剪贴板 / `arboard` / `rfd` | 只在导出/导入点击时建 `Clipboard` / 文件对话框，用完即 drop | 无常驻 |
| `egui::Context` 持久状态 | 滚动位置、窗口状态等由 eframe `persistence` 之外默认保存？本程序未开 `persistence` 特性（`Cargo.toml:37` 无此项），`egui::Memory` 只存当前帧交互状态，随帧更替 | 无增长 |
| 系统内存 87% | 204.5 MB 占 8 GB 约 2.5% | **不是本程序造成的**；若 Leo 机器是 8 GB，87% 的大头在别处（浏览器等）。修本程序不会把 87% 打下来，别给错误预期 |

---

## 6. 修复清单（按性价比排序，先砍最大头）

> “性价比” = （可省量级 × 确定性）/（改动量 × 风险）。第 0 项不是修复，是测量——没有它，后面每一项都无法验收，所以排第一。

| 顺序 | 修复 | 改动点 | 预期收益 | 风险 |
|---|---|---|---|---|
| **0** | **RSS 测量落地**（§7）：`GetProcessMemoryInfo` 直调 + 三档目标 | `src/bin/vpngate-gui.rs` 加约 40 行（`extern` 声明 + 记录函数 + 3 个调用点），零依赖 | 不是省内存，是让所有修复可验收；没有它就是盲修 | 零（只读进程自身内存信息，写日志） |
| **1** | **`TunnelStream` 加 `Drop` → `remove_session`** | `src/sstp.rs`：`TunnelStream` 补 `impl Drop`，复用已有的 `remove_session`（`src/sstp.rs:1237`） | 堵住唯一已证实的单调泄漏；量级小（每千连接约 0.2–0.5 MB，估计），但改动最小、最确定 | 低（见 §4.5） |
| **2** | **接入任务收进 `JoinSet`，断开时 `abort_all`** | `src/socks5.rs:197-221`：`serve_mixed` 里给接入任务一个 `JoinSet`（照抄 `forwarders` 的写法），退出时 abort；或回传 `JoinHandle` 给 `ActiveConnection::serve` | 让“断开”真正释放隧道；量级取决于残留连接数（估计每残留连接 20–40 KB，钉住的隧道整体另算），是“越用越大、断开不回落”最可能的解释 | 中低（见 §4.1）；不断开语义 |
| **3** | **`ConnLog` 改有界通道 + 丢弃计数** | `src/bin/vpngate-gui.rs:1831-1833` + `src/socks5.rs:138` | 堵住日志行的无界积压窗口；量级小（平时约 0） | 极低（日志行可丢） |
| **4** | **`log_text` 缓存**：只在 `log_line_at` 时重拼 | `src/bin/vpngate-gui.rs:4252` + `:1579-1588` | 每帧省一次 5–10 KB 分配，降堆高水位；量级小 | 低 |
| **5** | **字体子集化**（需 Leo 单独拍板） | `find_cjk_font()`（`vpngate-gui.rs:863`）优先读随程序发的子集字体 + 保留系统字体回落；构建时用 `fonttools` 子集化 | 省得最多：字体 15–22 MB → 约 3 MB（公开做法数据），**约省 12–19 MB**，是全表最大单项 | **高**（缺字变方框，见 §1.5）；必须先有回落 + 真机缺字测试 |
| **6** | **glow-only A/B 测试**（先量后合） | `Cargo.toml:37` eframe 特性 + `main()` 的 `NativeOptions.renderer` | RSS 节省**未实测、无法承诺**（估计为数十 MB 上限，见 §2.3）；二进制体积官方承诺减小 | 中高（驱动兼容 + kittest 与生产后端分叉，见 §2.5） |
| **7** | **`rows` / `display_indices` 缓存** | `src/bin/vpngate-gui.rs:2347,3343` | 每帧省约 25–40 KB 分配，降堆高水位；RSS 收益估计几个 MB | 低，但改动面大、收益小，放最后 |
| **8** | **TLS connector `OnceLock` 复用** | `src/net.rs:94,185` | 省每次握手的临时分配，不是 RSS，别优先做 | 低 |

**明确不做的**：OpenVPN 相关（另有封禁在案，与内存无关）；定期 `set_fonts` 清图集（§1.4）；`payload_tx` 改有界（§4.2，反压风险大于收益，先观测）。

---

## 7. RSS 测量方案（最小改动）

### 7.1 二选一：`GetProcessMemoryInfo` 直调（选这个），不用 `sysinfo`

| 方案 | 改动 | 结论 |
|---|---|---|
| **A. `extern` 直调 psapi（选）** | 零新依赖。在 `src/bin/vpngate-gui.rs`（或 `src/util.rs`，看实施者习惯）加约 30 行：`extern "system" { fn GetCurrentProcess() -> HANDLE; fn GetProcessMemoryInfo(hProcess, ppsmemCounters, cb) -> BOOL; }` + `#[repr(C)] PROCESS_MEMORY_COUNTERS`（只取 `WorkingSetSize` 与 `PagefileUsage` 两个字段，其余字段按顺序占位，`cb` 传 `size_of::<…>()`），链接 `psapi`（`#[link(name = "psapi")]`）。只在 `#[cfg(windows)]` 编译，Linux 走空实现返回 `None`，不影响 VM 构建与 kittest。 | **改动最小**：不改 `Cargo.toml`、不加依赖、不改构建、不影响任何现有测试。 |
| B. `sysinfo` crate | 加一行依赖 + 每次采样要 `System::new()` / `refresh_processes`，采样本身有分配与系统调用开销，且 `sysinfo` 的进程内存口径（`memory()`）与任务管理器不完全一致。 | 不选。 |

> 注意：任务管理器“内存”列是**专用工作集（Private Working Set）**，`WorkingSetSize` 是**工作集（含共享页）**，两者有几 MB 的口径差。报告里以 `WorkingSetSize` 为主、`PagefileUsage`（提交量）为辅，**同一口径纵向比增长曲线**，不与任务管理器截图做绝对值对齐。采样失败（返回 `None`）时记一行 `RSS unreadable`，不 panic、不影响界面。

### 7.2 记录点（写进日志，供 Leo 发回日志后画增长曲线）

采样函数（示意，实施者按 7.1 落地）：

```rust
// 伪代码（只放进本报告，不改源码）：
// fn sample_rss(tag: &str) -> String // 返回 "[RSS] tag=… working_set=…MB pagefile=…MB"
// 记录时同时带：atlas_fill（ctx.fonts(|f| f.font_atlas_fill_ratio())）、
// sessions（需给 SstpTunnel 加一个 pub fn session_count(&self) -> usize，约 3 行）、
// conn（是否已连接）、nodes（节点数）。
```

记录点表（`tag` 即日志里的事件名）：

| # | 时机 | 触发位置 | 备注 |
|---|---|---|---|
| T0 | **启动时一次** | `GuiApp::new` 完成后的第一帧（`ui()` 首次执行时记一次，用 `bool` 守卫） | 基线。含字体 + wgpu 初始化后的值，不是进程刚起的值 |
| T+5min | **启动后 5 分钟**（Tier A，见 §7.3） | `ui()` 里 `Instant` 计时跨过 5 分钟时记一次（只记一次） | 要求：这 5 分钟**不连接、不探测**，纯静置，量基线漂移 |
| C0 | **连接成功后一次** | `apply_event` 的 `GuiEvent::Connected` 分支（`vpngate-gui.rs:2256` 附近） | 连接后的跳变 |
| C+30min | **连接 30 分钟**（Tier B） | 连接后计时跨过 30 分钟记一次；若中途断开则不记（记 D0 代替） | 要求：这 30 分钟保持连接、手机正常浏览，量使用中增长 |
| D0 | **断开后一次** | `disconnect()` 末尾（`vpngate-gui.rs:1900-1906` 后）+ 断开 10 秒后再补一次（看残留任务是否释放，用 `request_repaint_after` 或下一帧计时） | 断开是否回落是 §4.1 的关键证据；两次采样（立即 + 10 s 后） |
| P0 | **探测全部后一次** | `apply_event` 的 `GuiEvent::ProbeAllDone` 分支（`vpngate-gui.rs:2244` 附近） | Tier C；探测是短时高分配，看峰值与回落 |
| 每 30 分钟一次 | **常规定时采样** | `ui()` 里 `Instant` 计时，每跨过 30 分钟边界记一次（与 T+5min / C+30min 共用同一个计时器，取最早到期的那个记） | 长期曲线的主力；托盘隐藏时 250 ms 的 `request_repaint_after`（`vpngate-gui.rs:3144`）保证隐藏时也能采样 |

日志行格式（进 `vpngate-gui.log`，走现成的 `log_line`，所以 Leo 发回 `vpngate-gui.log` 就够了，不用另建文件）：

```text
[RSS] t=+0005:00 tag=startup_plus_5min working_set=187.3MB pagefile=201.1MB atlas_fill=0.42 sessions=0 conn=false nodes=125
[RSS] t=+0032:10 tag=connected_plus_30min working_set=204.5MB pagefile=219.8MB atlas_fill=0.71 sessions=342 conn=true nodes=125
```

字段说明：`t` 是自启动的累计时间；`atlas_fill` 是 `font_atlas_fill_ratio()`（0–1，> 0.8 下一帧会重建）；`sessions` 是隧道会话表当前项数（断开时为 0，未连接时为 0；**若连接 30 分钟后 `sessions` 远大于活跃连接数，就是 §4.5 泄漏的直接证据**）；`conn` 是否已连接；`nodes` 节点数。

### 7.3 三档可核对目标的定义

> 以下目标是**可核对的定义**，不是已测得的承诺。绝对值以第一次真机 RSS 日志的 T0 为准校准；相对增量（Δ）才是判定标准。假设 T0 约 180–205 MB（与 Leo 截图的 204.5 MB 同档），若 T0 实测远偏离此区间，先以实测 T0 为准重算，不要硬套。

| 档 | 定义（满足即通过） | 判定方法（Leo 发回日志后） | 若不通过，指向 |
|---|---|---|---|
| **Tier A — 启动后 5 分钟（静置，不连接、不探测）** | `working_set(T+5min) − working_set(T0) ≤ 10 MB`，且 `atlas_fill` 有界（< 0.8 或已重建回落） | 日志里找 `tag=startup_plus_5min` 的行，与 T0 行相减 | 超了 → 基线本身在涨：优先查 §1 图集（看 `atlas_fill` 是否一路涨到 0.8）与 §2 wgpu 驱动分配；与隧道无关（未连接） |
| **Tier B — 连接 30 分钟（保持连接、手机正常浏览）** | `working_set(C+30min) − working_set(C0) ≤ 15 MB`；且**断开后** `working_set(D0+10s) − working_set(C0) ≤ 10 MB`（即断开能回落到接近连接前） | 日志里找 `tag=connected_plus_30min` 与 `tag=disconnected_plus_10s` 的行 | 连接中涨超 15 MB → 看同期 `sessions` 是否同步涨（§4.5）或 `atlas_fill` 是否涨（§1）；**断开不回落 → §4.1 残留任务是主因**，优先修清单第 2 项 |
| **Tier C — 探测全部后** | `working_set(P0) − working_set(P0 之前最近一次定时采样) ≤ 10 MB`，且下一次定时采样（30 分钟后）回落到 P0 之前 ±5 MB 内 | 日志里找 `tag=probe_all_done` 的行，与前后定时采样行比较 | 探测后不回落 → 探测任务残留或 `probes`/`probe_ms` 之外有增长（先排除 §4.3 已证无残留，再查 §3 堆高水位）；峰值本身高但回落了 → 正常，不修 |

**画曲线的方法**：把 `vpngate-gui.log` 里所有 `[RSS]` 行按 `t` 排序，横轴时间、纵轴 `working_set`，另画一条 `sessions`（只在连接期间有值）与一条 `atlas_fill`。三条线对齐看，增长的归因就出来了：跟 `sessions` 走 → §4.5；跟 `atlas_fill` 走 → §1；断开后不下来 → §4.1；谁都不跟、均匀爬升 → 回来查 §3 堆高水位 / §2 驱动。

### 7.4 实施清单（给 v3.14 实施者，不在本报告执行）

1. 加 `#[cfg(windows)]` 的 psapi 直调 + `sample_rss(tag)`（约 30–40 行），Linux 空实现。
2. 给 `SstpTunnel` 加 `pub fn session_count(&self) -> usize`（`self.sessions.try_lock().map(|g| g.len()).unwrap_or(0)`，约 3 行）——§7.2 的 `sessions` 字段要用。
3. 在 §7.2 表里的 6 个调用点各加一行 `log_line(sample_rss(…))`，计时用 `GuiApp` 里一个 `Instant` + `next_sample_at` 字段。
4. 不改 `Cargo.toml`，不加依赖，不改任何现有行为。发版前在 VM 上 `cargo test`（kittest 走 wgpu，不受影响）+ Leo 真机跑一轮 §7.2 的记录点，发回 `vpngate-gui.log`。

---

## 附录：核查过但排除的项（免得重复查）

- `self.log` 100 行上限（`vpngate-gui.rs:1579-1588`）——有界，排除。
- `probes` / `probe_ms` / `selected` / `blocked_hosts`——以节点数/选择数为界，排除。
- probe-all 任务残留（`vpngate-gui.rs:348-398` + `busy` 守卫）——单任务、必发 `ProbeAllDone`，排除。
- tokio 运行时、DoH 缓存（无缓存）、TLS 会话缓存（无缓存，每次新建）——排除。
- UDP 中继表（1024 上限 + 60 s 空闲清扫，`src/socks5.rs:460-464`）——有界，排除。
- `GalleyCache`（每帧 `flush_cache`，`fonts.rs:1286-1290`）——每帧清，排除。
- `PacketReader.buf`（SSTP 包长 ≤ 0x0FFF，`src/sstp.rs:201`）——有界，排除。
- 文件日志（2 MiB 轮转，`src/filelog.rs:29`）——磁盘有界、内存无增长，排除。

## 附录：出处（版本号以本机 registry 为准）

- eframe 0.36.2 / egui 0.36.2 / epaint 0.36.2 / egui-wgpu 0.36.2 / egui_glow 0.36.2 / wgpu 30.0.1 / tokio 1.54.1 / egui_kittest 0.36.2（`Cargo.lock`）。
- eframe 默认特性与 glow 说明：`~/.cargo/registry/.../eframe-0.36.2/Cargo.toml.orig`（`default` 含 `wgpu`、不含 `glow`；`Switching from wgpu (the default) to glow can significantly reduce your binary size`）。
- `Renderer::default()` 优先 wgpu：`eframe-0.36.2/src/epi.rs:598-619`。
- 字形缓存无逐出、图集 80% 重建、galley 每帧清：`epaint-0.36.2/src/text/font.rs:367,375,633`、`texture_atlas.rs:allocate()`、`text/fonts.rs:730-745,1286-1290`。
- `msyh.ttc` 体量区间：公开资料（`msyh.ttc` 原文件 22.5 MB；`msyh.ttf` 约 14.3–19 MB，不同版本/拆分方式），本机类比实测 `NotoSansCJK-Regular.ttc` 19 MB。
- 字体子集化做法：`fonttools` 子集化至 Latin + GB2312 约 6700 字后约 3 MB（公开 subset-fonts.md 做法）。
