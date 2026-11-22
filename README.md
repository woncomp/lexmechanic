# AgentChatReader

基于 Rust / GPUI 的 Windows x64 桌面会话阅读器。只读扫描本机 Agent 缓存，在一个工作台中浏览、搜索和导出历史对话。支持 **Copilot CLI、Copilot App 和 VS Code 中的 GitHub Copilot Chat**；无需 GitHub 登录、Python 或模型服务。

## 使用

运行便携包中的 `AgentChatReader.exe`。首次启动扫描 `COPILOT_HOME`（未设置时为 `%USERPROFILE%\.copilot`）中的 CLI / App 会话，并自动发现 `%APPDATA%\Code\User` 中的 VS Code Copilot 会话。可在 **Settings → Add Copilot home…** 选择包含 `session-state` 或 App `data.db` 的目录，在 **Add VS Code User…** 选择包含 `globalStorage` 和 `workspaceStorage` 的 VS Code `User` 目录。

- 左侧四列表格：无标题的 Agent 图标列、日期、workspace、会话名。悬停图标显示对应 Agent 名称。Date 只显示 `MM-DD`，悬停显示完整本地日期、时间及 UTC 偏移；排序仍使用完整时间，不按月日文本排序。旧版列宽自动迁移，日期默认缩窄。
- **扫描顺序与展示排序独立**：每个 Agent 在独立工作线程中按日志文件修改时间从新到旧扫描，覆盖全部历史；结果按消息时间插入统一总表。
- 点击 **Latest message / First assistant** 切换时间模式。首条 Assistant 的时间缺失或无效时排末，不回退到创建时间。时间在内部统一为 UTC，界面显示本地时间。
- 单击行临时预览，双击、Enter、**Keep tab** 或双击标签常驻打开。最多一个预览标签；已有标签不会重复打开。可拖动标签排序，右键关闭其他或右侧标签。
- 右侧以 Notebook 单元呈现顶层 User / Assistant 消息，支持 Markdown、代码高亮、折叠、选择复制、原始消息文本和 Markdown 导出。
- 最右侧为逐消息导航，点击横条或输入消息编号定位；长会话使用独立滚动的导航列表。
- 顶部搜索全部已索引主对话；`Ctrl+F` 查找当前会话。搜索为 Unicode 规范化后的大小写不敏感字面子串，不将输入解释成正则、SQL 或搜索语法。中文一、二字查询也可使用。
- 搜索结果渐进返回，常见查询分页显示；**Next page / More matches** 继续读取下一批。扫描或索引未完成时结果仍可能补全，不应将当前空结果理解为整个来源都没有命中。
- 原始日志追加内容会合并刷新；阅读历史时不会抢滚动位置。**New content · Reload** 读取变化，主动开启 **Follow** 才跟随最新；手动滚动暂停跟随。
- 常驻标签、阅读位置、折叠、主题、字号与列宽保存在应用自身目录；临时预览不会在重启后恢复。

主对话不包含工具调用、子 Agent 和纯系统注入。已知系统包装后的真实用户文本会保留，**Raw** 可查看来源仍可用的原始消息文本。附件只显示元数据，不自动打开附件、远程图片或执行任何会话内容。外部 HTTP(S) 链接仅在主动点击后交给浏览器。

### 各来源的读取方式

| 来源 | 会话元数据 | 对话内容 |
| --- | --- | --- |
| Copilot CLI | `session-state\<id>\workspace.yaml` 中的 `client_name` / `last_client_name` 标记与名称 | `events.jsonl` 中的顶层用户和助手消息 |
| Copilot App | 只读查询 `data.db` 的 `sessions` / `workspaces`；workspace 路径从 `workspace.yaml` 或 `session-store.db` 补充 | 同目录下的 `session-state\<id>\events.jsonl`，与 CLI 共用消息过滤规则 |
| VS Code | `globalStorage\github.copilot-chat\session-store.db` 中的 Copilot Chat 会话；各 workspace 的 `state.vscdb` 中的标题索引 | `workspaceStorage\<workspace>\chatSessions\<id>.jsonl` 的会话快照与增量重放 |

`workspace.yaml` 中的 `last_client_name: github/cli` 优先识别为 Copilot CLI，包括从 Copilot App 分支后在 CLI 继续的会话，不受原始 `client_name` 或 App 数据库登记影响。没有此标记时，App 登记的会话优先归属 App，其余 `client_name: github/cli` 的会话归属 CLI。归属变化时会迁移旧缓存、标签和阅读位置，避免重复显示。单一 workspace 的 App 名称优先于通用标题；没有本地日志但仍有本地目录或共享数据库记录的 App 会话可显示元数据，并明确提示正文不可用，不会伪装成空对话。

VS Code 重放 `kind=0/1/2` 的快照、赋值和数组追加，仅显示可见 request 的用户文本与助手文字片段，过滤隐藏 request 和工具片段。标题索引不可用时使用快照标题或首条用户内容。同一会话存在多个 transcript 副本时使用最后修改时间最新的一份并显示诊断。VS Code 时间从 request 的 `timestamp` / `responseTimestamp` 提取（毫秒时间戳），不会用标题索引的活动时间冒充首条助手消息时间。

VS Code 追加 delta 可能改写已有回复，因此变更时重放并重建该会话索引，不按 CLI 的纯追加消息处理。损坏的中间 delta 会明确报错，避免把不完整状态当作完整对话；未写完的最后一行则等待续写。数据库均以只读方式打开，文件监听覆盖日志、App 数据库和 VS Code 标题数据库。

### 快捷键

| 快捷键 | 操作 |
| --- | --- |
| Ctrl+Shift+F | 搜索全部会话 |
| Ctrl+F | 查找当前会话 |
| Up / Down | 在会话列表中选择行 |
| Space / Enter | 预览 / 常驻打开选中行 |
| Ctrl+Tab / Ctrl+Shift+Tab | 切换标签 |
| Ctrl+W / Ctrl+Shift+T | 关闭 / 恢复最近关闭标签 |
| Alt+Up / Alt+Down | 阅读区中切换消息 |
| F3 / Shift+F3 | 下一个 / 上一个查找命中 |
| F5 | 刷新来源 |
| Esc | 退出查找、搜索或设置等局部状态 |

### 数据与隐私

索引、配置和诊断日志位于 `%LOCALAPPDATA%\AgentChatReader\data`（由 Windows 标准应用目录 API 解析）。可设置 `AGENT_CHAT_READER_DATA_DIR` 为独立应用数据目录，例如用于合成数据测试。

SQLite 索引包含会话正文，**不是加密数据库**。请按原始会话数据的敏感程度保护该目录。程序不上传会话，普通诊断不记录正文。清除本地索引不修改 Agent 缓存；原始来源丢失、损坏或不可读会显示通知。

切换扫描来源不会自动抹除已有历史缓存。若只希望保留当前配置来源的结果，使用 **Clear local index**；程序会从当前来源重新建立索引。

导出只包含完整主对话，不只导出当前搜索命中；默认不附带本机完整路径或附件实体。会话尚未完整索引时会明确拒绝导出。索引中的文件身份使用检查点验证，异常时重建；来源被外部程序原地修改而保留大小与修改时间的极端情况，需要清除索引后重新扫描。

## 构建

需要 Windows x64、Visual Studio C++ Build Tools / MSVC 与 Windows SDK。仓库的 `rust-toolchain.toml` 固定已验证的 Rust 工具链；不需要修改全局默认工具链。

```powershell
cargo run -p agent-chat-reader
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
.\scripts\package-windows.ps1
```

发行输出为 `dist\AgentChatReader\AgentChatReader.exe` 与 `dist\AgentChatReader-windows-x64.zip`，包含依赖许可说明。便携包不提供安装器、自动更新或代码签名。需要支持所选 GPUI Windows GPU 后端的显卡与驱动；启动失败时查看应用数据目录中的 `AgentChatReader.log`。

GPUI Kit 与其匹配的 GPUI snapshot 由版本及 `Cargo.lock` 锁定；不要单独替换其中一份 GPUI 依赖。Windows 目标通过 `.cargo\config.toml` 静态链接 C 运行库，发行包不要求另装 Visual C++ Redistributable。

## 结构与性能

- `crates\core`：Agent 无关的身份和时间模型、同步适配接口、CLI / App / VS Code 独立适配器、共享 Copilot 事件解析、每 Agent 扫描工作线程、SQLite 单写入通道、增量检查点与字面搜索。
- `crates\app`：原生 GPUI 工作台、表格/标签状态、变高 Notebook、消息分段与导航、独立文档/搜索线程、文件通知、偏好持久化和导出。
- 摘要扫描优先于历史正文索引；打开文档有独立的读取通道，不能排在全部历史索引之后。
- UI 只渲染可见行和消息段。超长消息分段显示，避免一个巨型单元阻塞布局；长消息的分段使用纯文本代码视图以保持完整内容，**Copy** 和导出仍覆盖整条消息。
- 正文/渲染状态有字节预算和实体数量限制，切换标签释放旧渲染状态。SQLite FTS5 trigram 缩小较长查询候选；短词采用可取消的精确扫描。旧请求不能覆盖新选择。
- 追加解析只消费新完整记录；末尾半条 JSON 保留待续，不会提前越过索引游标。文件截断、替换或检查点不一致会重建相应会话；中间坏行保留明确诊断，其他会话继续可用。
- VS Code 的快照重放在后台按行读取并支持取消，最多保留一个已重放的会话，不将全部历史状态常驻内存；单个 VS Code 会话仍需还原完整状态，内存开销与该会话大小相关。
- `assets\agents` 中的图标嵌入可执行文件；Copilot CLI 使用由提供的 Icons8 PNG 矢量重绘的 `copilot-cli.svg`，保留紫蓝渐变与青色细节，不内嵌位图。Copilot App 保持 PNG，VS Code 使用 SVG；发行包不依赖原始图标路径。
- 可重建索引使用 SQLite WAL / NORMAL：事务中的正文、FTS 与游标保持原子一致，断电时尚未持久化的索引进度允许从原始日志重新生成；不以此降低原始数据或设置文件的持久化要求。

### 合成数据验证

不要把真实私有会话加入测试夹具。以下命令要求使用不存在的新输出目录，生成器不会覆盖既有来源：

```powershell
cargo run -p agent-chat-reader-core --release --example fixtures -- generate target\synthetic 1000 100 4096
cargo run -p agent-chat-reader-core --release --example fixtures -- scan target\synthetic target\synthetic.sqlite
.\scripts\smoke-windows.ps1 `
    -Executable target\debug\AgentChatReader.exe `
    -CopilotHome target\synthetic `
    -DataDirectory target\synthetic-app
```

生成器刻意使文件修改时间与消息时间相反，验证扫描优先级不会被展示排序替代。还可将数量改为 `10000 100 4096`，验证百万消息场景。

百万消息实测（10,000 会话、1,000,000 条主对话、约 8 GiB 原始事件）：最终实现无应用索引时最近 50 个摘要约 0.56 秒可用，系统文件缓存可能已预热。缓存核对一轮约 4.55 秒，首批摘要约 1.12 秒可用。索引进程在构建中一次采样的私有内存约 21 MiB，不包括 GPUI 窗口或 GPU 资源。

对同一百万消息索引连续测量 30 次，静态运行库构建中，稀有长查询首批 p95 约 1.7 毫秒，中文二字与三字查询首批 p95 分别约 8.6 与 20 毫秒。这些是本机合成数据观测，**不是跨硬件保证**。可用 `fixtures recent SOURCE NEW_DATABASE` 和 `fixtures search DATABASE` 重现相应测量。

补充的单个约 269 MiB 日志包含 128 条各约 1 MiB 的主对话消息，完整索引一轮约 13 秒。大正文搜索按消息数及 2 MiB 字节预算共同分批，实测首批约 29–40 毫秒返回两条命中，后续命中继续返回，而不是先等待收集满 128 条。

性能调整包括：较新会话优先使用有界索引突发，巨型历史日志仍会让出执行机会；稀有查询走 trigram 候选，常见查询按日期索引推进；短词和后续页使用游标，不从头扫描并跳过全部旧页。帧耗时、不同 DPI 与超长内容的原生交互仍应单独验收，不能用搜索时间替代 UI 流畅度。

## 当前边界

当前支持 Copilot CLI、Copilot App、VS Code Copilot Chat，平台验收目标仍为 Windows x64。不提供其他 Agent 实现、工具时间线、子 Agent 树、自动摘要、语义搜索、编辑/继续对话或云同步。附件保持元数据视图；远程图片保持阻止状态。跨分段的鼠标拖选不合并为一次全消息选择，需要使用消息级 Copy；任意超长 Markdown 的分段优先保证原文完整，而非跨段格式连续。