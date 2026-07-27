# 移除内建 PTY 与终端复用职责：设计与实施计划

> 状态：设计决策完成，等待 fork 内 issue 与项目指导后再实施。

## 目标

从 `nono-cli` 中彻底移除自建 PTY、终端 I/O relay、attach/detach、屏幕重放以及相关终端复用逻辑，同时保留 `nono run` 与 `nono shell` 的 Supervised 执行模型。

改造完成后，普通前台命令的终端关系应当是：

```text
调用方终端 <────────────> sandboxed child
                              │
                        nono supervisor
                 （等待、审计、代理、rollback、
                   resource limits、结构化诊断）
```

supervisor 不再位于 child 的终端数据路径上，不再创建虚拟终端，也不再观察、解析、缓存、重放或临时接管 child 的终端输入输出。

## 核心原则

1. **nono 专注于沙箱与监督。** 终端复用不是沙箱能力，不再由 nono 自行实现。
2. **保留 Supervised，删除 terminal supervision。** 不能通过改用 `wrap`、Direct 或取消 supervisor 来规避问题。
3. **前台终端是显式共享资源。** 文件、网络、凭据、进程检查和执行能力继续由沙箱限制；调用方终端本身不再被视为隔离边界。
4. **不以 pipe、tee 或另一套 relay 替代 PTY。** child 的 stdin/stdout/stderr 直接继承调用方 fd。
5. **破坏性删除优于兼容空壳。** 不保留不可工作的 attach/detach 命令、隐藏开关或终端复用兼容层。
6. **改动按可验证、可 bisect 的提交拆分。** 该 fork 会持续跟随上游，每一步都应尽量保持编译、测试和清晰的冲突边界。

## 已确认的产品决策

### 1. 完整删除终端会话功能

删除以下用户可见能力：

- `nono attach`
- `nono detach`
- `attach` / `detach` 的 `resume` / `pause` alias
- `nono run --detached`
- `--detach-timeout`
- in-band detach 快捷键
- `[ui].detach_sequence`
- attach Unix socket、握手、认证、resize channel
- detached start helper、startup log 与自动 attach
- PTY scrollback、VT screen state 和画面重放

这是明确的 CLI 破坏性变更，不保留废弃命令或“功能已删除”的运行时占位实现。

### 2. 保留非终端 session 管理

继续保留：

- `nono ps`
- `nono stop`
- `nono logs`
- `nono inspect`
- `nono prune`
- session registry 与 session/audit 元数据

删除 session 模型中的终端 attachment 概念：

- 删除 `SessionAttachment`
- 删除 `SessionRecord.attachment`
- 删除 `nono ps` 的 `ATTACH` 列
- 删除 `nono inspect` 的 `Attached` 字段
- 删除 session attach socket path 与 attachment 状态更新 API

这些命令管理的是 sandbox/supervisor session，不是终端会话，因此不应随 PTY 一起删除。

### 3. 保留 Supervised 执行模型

`nono run` 与 `nono shell` 继续保留 supervisor，并继续支持：

- Landlock / Seatbelt 沙箱应用
- seccomp mediation
- network / credential proxy
- tool-sandbox command mediation
- audit event 与 audit integrity
- rollback
- resource limits
- session registry
- child 等待和退出码传播
- 来自 supervisor、内核沙箱和代理的结构化诊断

缺失的组合应被正式支持：

```text
Supervised runtime + child 直接继承真实 stdin/stdout/stderr
```

### 4. child 使用调用方的真实 stdio

顶层 supervised child 不再调用：

- `openpty()`
- `setsid()`（PTY setup 路径）
- `TIOCSCTTY`
- 为 PTY 执行的 `dup2()`
- PTY master/slave relay
- PTY resize 同步
- PTY controlling-terminal 管理

child 直接继承 nono 收到的 stdin/stdout/stderr，包括：

- TTY
- pipe
- 文件重定向
- 关闭的或混合类型的 stdio

nono 不应为了统一这些情况而另建中间传输层。

### 5. 使用原生前台进程组与作业控制

nono 与 child 留在外层 shell 分配的同一个前台进程组中：

```text
外层 shell
  └─ foreground process group
       ├─ nono supervisor
       └─ sandboxed child
```

不为 child 创建独立 session 或 process group，也不调用 `tcsetpgrp()` 接管真实终端。

预期行为：

- Ctrl-C、Ctrl-\、Ctrl-Z 由终端内核直接投递给前台作业。
- `fg` / `bg` 继续由外层 shell 管理。
- 窗口尺寸变化由真实终端直接对前台进程生效。
- nono 不再手工暂停、恢复或重绘 TUI。

### 6. 信号职责划分

已确认的目标信号语义：

- `SIGINT`、`SIGQUIT`：运行 child 时 supervisor 不二次转发；终端已直接投递给共享前台进程组。
- `SIGTSTP`、`SIGCONT`：不安装自定义处理器，交给原生 job control。
- `SIGWINCH`：不再转发到 PTY，child 直接使用真实终端。
- `SIGTERM`、`SIGHUP`：继续作为 supervisor 管理信号转发给直接 child。
- `SIGUSR1`：删除 detach 控制用途。
- child 退出后：恢复 supervisor 原有信号状态，确保退出后提示正常工作。

从外部管理 session 时，`SIGTERM` 是标准终止接口；不再把只发送给 supervisor PID 的 `SIGINT` 当成管理协议。

### 7. 不捕获或恢复 child 的 termios

nono 不新增 session-wide termios guard，不在启动 child 前保存 fd 0/1/2 的终端模式，也不在 child 结束后恢复。sandboxed child 直接继承真实标准 fd，并对其终端副作用负责；外层 shell 或调用方负责在作业结束后恢复自己的终端状态。

明确接受：

- child 可以留下 raw、no-echo 或关闭 `OPOST` 的终端状态。
- 诊断 footer、rollback 信息在异常终端模式下可能显示错乱。
- 没有 job-control shell 的调用环境可能需要用户执行 `reset` 或 `stty sane`。
- `nono run -- stty ...` 对终端作出的修改不会被 supervisor 撤销。
- nono 不恢复 alternate screen、光标、颜色、屏幕内容或其他控制序列造成的状态。

这些是共享真实终端通道的兼容性与拒绝服务风险，不是 sandbox escape。nono 不为此读取、代理、解析或规范化 child 的终端 I/O。

现有 profile-save prompt 可以保留自己的局部 `PromptTerminalGuard`：nono 在自己即将读取用户输入时临时配置 `/dev/tty`，属于 prompt 的内部 UI 实现，不是对 child session 的终端监督。本计划不扩张或重构该 guard。

### 8. 删除运行期间的终端授权提示

运行中的 child 与 supervisor 不得竞争读取同一个真实终端。删除 runtime `TerminalApproval`：

- 不再在 child 运行期间从 `/dev/tty` 读取 `y/N`。
- 不通过暂停 child、`SIGSTOP`、termios 抢占或其他方式插入 prompt。
- 配置中的 terminal approval backend 应从受支持模型中移除。
- 没有可用的非终端 approval backend 时，动态权限请求默认拒绝。

保留非终端 approval backend：

- webhook
- 仅由非终端 backend 组成的 chain

现有 command-policy approval backend 配置应复用于主 supervisor 的动态 capability approval；不为本次改造设计第二套 schema。

启动 child 之前或 child 退出之后的提示继续保留，因为此时不存在输入竞争：

- 启动前的安全、工作目录或信任确认
- 退出后的 rollback 提示
- 退出后的 profile-save 提示

### 9. 删除终端输出观察与推断

nono 不再捕获或扫描 child 的终端输出，因此删除：

- PTY screen plaintext
- scrollback
- VT parser / screen reconstruction
- `has_visible_output()`
- alternate-screen 检测
- `is_interactive()`
- 从终端文本识别 `permission denied`、缺失路径或网络错误
- 从终端文本生成 diagnostic path hint
- 从终端文本生成 profile-save grant 候选
- 依赖“是否进入交互状态”的 startup timeout
- `--startup-timeout` 与 `NONO_STARTUP_TIMEOUT`

child 的原始 stdout/stderr 仍会直接显示给用户。nono 继续报告自己实际观察到的结构化事实：

- Landlock / Seatbelt 拒绝
- seccomp 拒绝
- supervisor IPC 拒绝
- network proxy / tool-sandbox audit event
- resource limit、OOM 和进程数限制
- child 退出码或终止信号
- macOS sandbox log（不依赖 PTY 的部分）
- audit log 与 diagnostic footer

### 10. tool-sandbox 同样删除 PTY

tool-sandbox 当前在 shim 的三个 stdio 都是 TTY 时创建第二层 PTY，并手写 raw mode relay。该路径也必须删除，否则 nono 仍然承担终端代理职责。

目标行为：

- shim 继续通过 Unix socket 传递 stdin/stdout/stderr fd。
- 所有交互式工具直接使用这些 fd。
- TTY 工具不再选择 `"pty"` stdio mode。
- TTY 工具不再调用 `setpgid(0, 0)` 脱离前台进程组。
- 非 TTY 的 pipe、重定向、brokered stdio 和 stdio limit 继续保留。
- 删除 tool-sandbox 的 raw guard、PTY relay、resize 同步与 drain。

### 11. 真实终端不再是隔离边界

删除 PTY 不会自动放宽 Landlock、Seatbelt、网络、凭据或 execute allowlist，但 child 会直接获得真实终端 fd，可以：

- 修改真实终端 termios
- 调用终端 ioctl
- 读取用户发给前台程序的按键
- 输出 ANSI/OSC 控制序列
- 参与同一前台进程组的 job control

安全文档必须明确：nono 的隔离边界覆盖文件、网络、凭据、进程和执行能力；交互式前台 session 的终端是显式共享通道。

### 12. Linux 强制拒绝 `TIOCSTI`

为所有 Linux sandboxed child 增加 fail-closed seccomp 规则：

```text
syscall != ioctl                 -> allow
ioctl request == TIOCSTI         -> EPERM
other ioctl requests             -> allow
```

要求：

- 覆盖顶层 supervised child 及其后代。
- 覆盖 tool-sandbox 受控工具及其后代。
- 允许正常 termios 与窗口尺寸 ioctl。
- 不依赖 `/proc/sys/dev/tty/legacy_tiocsti` 的主机默认值。
- filter 安装失败时拒绝启动 child，不静默降级。
- 添加 BPF shape 单元测试和真实 syscall 集成测试。

macOS 不临时实现一套未经验证的 ioctl 过滤；文档应明确平台差异。

### 13. 不保证后台后代与 footer 的严格输出顺序

主 child 退出后，nono 可以立即进入结构化诊断和退出后提示流程。nono 不等待仍存活的后台后代“输出完毕”，也不通过重新代理 stdout/stderr 来获得顺序控制。

测试只保证：

- 主 child 退出前完成的输出可正常到达调用方终端。
- 主 child 的退出码正确传播。
- supervisor 结构化诊断仍正常生成。

不保证后台后代输出与 footer/profile prompt 的字节级顺序。

### 14. Linux 与 macOS 统一删除 PTY

两个平台都采用直接 stdio 语义，并保持可编译、可用：

- Linux：Landlock + seccomp，并增加 `TIOCSTI` 拒绝。
- macOS：Seatbelt，终端明确不属于隔离边界。

不能让 macOS 保留 PTY 而 Linux 删除，否则同一 CLI 在不同平台会继续具有两套终端模型，`pty_proxy` 也无法彻底删除。

## 非目标

本计划不包含：

- 集成 rmux、tmux 或其他终端复用器。
- 提供可选 PTY mode。
- 保留 attach/detach 的兼容实现。
- 使用 pipe/tee 捕获 child 输出。
- 实现终端模拟器或 ANSI sanitizer。
- 修复 child 留下的 alternate-screen 或光标画面。
- 将 Supervised 替换为 Direct/Monitor。
- 重构与 PTY 删除无关的沙箱、审计或代理模块。
- 改变后台后代的生命周期语义。

## 预期删除范围

### 完整删除的模块

- `crates/nono-cli/src/pty_proxy.rs`
- `crates/nono-cli/src/startup_runtime.rs` 中仅服务 detached start 的实现；若文件无其他职责则删除整个模块
- `crates/nono-cli/src/startup_prompt.rs` 中仅服务 PTY startup timeout 的实现；保留仍被其他诊断调用的通用 terminal-safe 输出时，应移动到职责匹配的现有模块，而不是留下空壳
- `crates/nono-cli/src/terminal_approval.rs`（前提是所有非运行期调用均已迁移或删除）

### 重点修改的模块

- `crates/nono-cli/src/main.rs`
- `crates/nono-cli/src/cli.rs`
- `crates/nono-cli/src/app_runtime.rs`
- `crates/nono-cli/src/cli_bootstrap.rs`
- `crates/nono-cli/src/launch_runtime.rs`
- `crates/nono-cli/src/execution_runtime.rs`
- `crates/nono-cli/src/supervised_runtime.rs`
- `crates/nono-cli/src/exec_strategy.rs`
- `crates/nono-cli/src/exec_strategy/supervisor_linux.rs`
- `crates/nono-cli/src/session.rs`
- `crates/nono-cli/src/session_commands.rs`
- `crates/nono-cli/src/config/user.rs`
- `crates/nono-cli/src/profile_save_runtime.rs`
- `crates/nono-cli/src/approval_runtime.rs`
- `crates/nono-cli/src/command_policy.rs`
- `crates/nono-cli/src/tool-sandbox/platform/linux.rs`
- `crates/nono-cli/src/tool-sandbox/platform/macos.rs`
- `crates/nono/src/sandbox/linux.rs`
- `crates/nono-cli/src/timeouts.rs`
- `crates/nono-cli/Cargo.toml`

### 依赖清理

- 删除仅供终端屏幕模拟使用的 `vt100` 直接依赖。
- 重新检查 `nix` 的 `term` feature：现有退出后 prompt 的局部终端处理仍可能需要它，不能因为删除生产 PTY 而机械移除。
- 通过 Cargo 原生命令更新 `Cargo.lock`，不得手工编辑。

### 文档清理

至少检查并更新：

- `README.md`
- `docs/cli/features/session-lifecycle.mdx`
- `docs/cli/features/execution-modes.mdx`
- `docs/cli/usage/flags.mdx`
- `docs/cli/internals/security-model.mdx`
- 所有提及 attach、detach、PTY、multiplexing、startup timeout 或 terminal approval 的页面

`session-lifecycle.mdx` 不应整体删除；应重写为 sandbox/supervisor session lifecycle，只保留 `ps`、`stop`、`logs`、`inspect`、`prune`、audit 与 rollback 相关内容。

## 实施阶段与建议提交边界

每个阶段完成后都必须至少执行针对性测试和 `cargo check -p nono-cli --all-targets`。在当前阶段不稳定时，不进入下一阶段。

### 阶段 0：固定 fork 保留的既有非终端契约，不改变行为

**目的：** 为删除 PTY 将直接改动的执行边界建立最小回归覆盖；不补充 fork 即将删除的 PTY、attach/detach 或交互式终端行为测试。

1. 添加现有非 TTY supervised inherited-stdio 行为的回归测试。
2. 仅为 PTY 删除将直接触及的 supervisor 路径添加最小回归测试：child 退出码传播、由 session 管理路径发出的 `SIGTERM`、session registry 更新和结构化诊断。
3. 若 tool-sandbox 当前已经直接继承非 TTY fd，添加该既有行为的回归测试；若这是改造后的目标行为，则将测试与实现放入对应阶段。
4. 不添加 Ctrl-C、Ctrl-Z/`fg`、resize、TTY identity、termios、attach/detach、screen replay 或任何需要 PTY harness 的测试。
5. 不在本阶段添加“生产代码没有 PTY”的检查；删除尚未发生，该检查会制造刻意红灯。
6. 不在本阶段添加 `TIOCSTI` filter 测试；测试与实现放在同一安全提交中。

建议提交：

```text
test(runtime): pin supervised direct-stdio behavior
```

### 阶段 1：先删除 attach/detach 与 detached start

**目的：** 先移除终端复用的用户表面，把现有 PTY 收缩为只服务普通前台运行的临时 relay，避免后续切换 inherited stdio 时留下仍可调用但已经失效的 attach/detach 命令。

1. 删除 CLI attach/detach command、args、aliases 和 dispatch。
2. 删除 `RunArgs.detached`、`detach_timeout_secs`、detached launch 环境变量和 re-exec 流程。
3. 删除 startup attach socket polling、startup log、attach handshake、resize socket 和 screen replay。
4. 删除 in-band detach sequence、`[ui].detach_sequence` 运行时类型和按键匹配逻辑；保留 removed-field 迁移诊断。
5. 删除 `SessionAttachment` 以及 `ps`/`inspect` 的 attachment 展示与更新 API。
6. 将 `PtyProxy` 暂时收缩为普通前台 child 的单客户端 I/O relay，不再提供 attach、detach、pause 或 replay 能力。
7. 更新 CLI help、completions 和与已删除用户表面直接相关的测试。

此阶段结束时普通前台 `run`/`shell` 仍可能经过 PTY，但已经没有任何终端复用或 detached session 入口。现有测试必须同步删除或改写，不能保留会把用户送入无法重新 attach 的状态。

建议提交：

```text
feat(cli)!: remove terminal session attach and detach
```

### 阶段 2：删除运行期 terminal approval

**目的：** 在 child 改为共享真实终端前，先确保 supervisor 永不与运行中的 child 竞争读取终端。

1. 删除 `TerminalApproval` 与 named terminal backend 实现。
2. 保留 `ApprovalBackendType::Terminal` 作为纯解析/迁移哨兵，validation 在 child 启动前给出 removed-type 错误。
3. 校验 chain 不能引用 terminal backend，不能静默删除 chain 子项。
4. 将主 supervisor 的 filesystem capability approval 接到现有 `command_policies.approval_defaults.backend`。
5. 未配置 default、backend 不可达、timeout、错误或 chain 无 granting backend 时 fail closed，并记录 backend 与原因。
6. 删除 `request_approval_with_relay_paused()`，直接调用配置的 webhook/chain backend。
7. 保留 child 启动前和退出后的 prompt。
8. 更新配置校验、schema、示例和测试。

建议提交：

```text
feat(approval)!: remove in-session terminal approvals
```

### 阶段 3：删除终端输出观察与 startup timeout

**目的：** 在切换 inherited stdio 前，让 supervisor 不再依赖 PTY screen 数据或终端内容推断。

1. `ErrorObservation` 不再接收 PTY screen 数据。
2. 删除仅由终端文本产生的诊断输入路径。
3. profile-save 只使用结构化 denial/policy evidence。
4. 删除 `StartupTimeoutConfig` 和相关 loop deadline。
5. 删除 `--startup-timeout`、环境变量、profile plumbing 和文档。
6. 保留不依赖 PTY 的 diagnostic formatter 与 sandbox log。

若 `analyze_error_output()` 在其他明确的、非 PTY 输入来源仍被使用，应保留通用解析函数；不得为了“顺便清理”扩大删除范围。

建议提交：

```text
feat(diagnostics)!: stop observing child terminal output
```

### 阶段 4：提前增加 Linux `TIOCSTI` 防护

**目的：** 在顶层和 tool-sandbox child 获得调用方真实终端前先完成 Linux 输入队列注入防护，避免任何中间提交暴露共享终端但尚未安装 filter。

1. 在现有 classic BPF seccomp builder 中添加精确的 `ioctl` request rule。
2. 为 syscall number、architecture check、argument offset、ALLOW/ERRNO 分支添加 shape 测试。
3. 在顶层 Linux sandboxed child exec 前安装。
4. 在 tool-sandbox child exec 前安装。
5. filter 安装错误必须中止执行。
6. 添加 fork 隔离的 live test：目标 `TIOCSTI` request 返回 `EPERM`，未过滤 ioctl 继续进入内核并返回正常的 `EBADF`/`ENOTTY`。
7. 文档明确 host sysctl 只是额外防线，不是 nono 的依赖。

建议提交：

```text
feat(linux): deny TIOCSTI in sandboxed children
```

### 阶段 5：让顶层 Supervised 使用 inherited stdio

**目的：** 在终端复用表面、运行期 terminal approval、screen observation 和 Linux 注入风险都已经处理后，切断普通 `run`/`shell` 主路径对 PTY 的依赖。

1. 从 `SessionRuntimeState` 删除 `pty_pair` 和 `should_open_supervised_pty()`。
2. 从 `execute_supervised()` 参数中删除 `PtyPair`。
3. child fork 后不再执行 `setup_child_pty()`，stdin/stdout/stderr 保持继承。
4. parent 不再创建 `PtyProxy`，supervisor poll loop 删除顶层 PTY fd 和 relay 分支。
5. `SIGINT`、`SIGQUIT`、`SIGTSTP`、`SIGCONT`、`SIGWINCH` 不再由 supervisor 模拟或二次转发。
6. `SIGTERM`、`SIGHUP` 保留为 supervisor 管理信号，`SIGUSR1` detach 用途删除。
7. 不捕获、保存或恢复 child 的 termios。
8. 保留 supervisor socket、seccomp、proxy、audit、rollback、resource limit 和 wait path。

建议提交：

```text
refactor(runtime): run supervised children on inherited stdio
```

### 阶段 6：删除 tool-sandbox PTY 路径

**目的：** 确保受控工具也不经过 nono 的终端 relay。

1. 移除 `selected_stdio_mode()` 的 TTY → PTY 选择。
2. 协议中不再发送或接受 `"pty"` mode。
3. 删除 `launch_child_with_pty()`。
4. 删除 `relay_pty_and_wait()`、`TerminalRawGuard`、winsize relay、drain/read/write helper（仅在无其他调用时）。
5. TTY fd 走 direct fd 启动，并保持调用方前台进程组。
6. 非 TTY 且配置 stdio limit 时继续走 brokered pipe relay。
7. 审计中的 `stdio_mode` 改为真实、稳定的值，例如 `direct_fds` 或 `brokered`；不要保留永远不可能出现的 `pty`。

建议提交：

```text
refactor(tool-sandbox): use caller stdio without PTY relay
```

### 阶段 7：删除 PTY 模块与依赖

**目的：** 删除所有生产 PTY 实现，防止死代码或未来误用。

1. 确认 `rg` 不再找到生产代码中的 `PtyPair`、`PtyProxy`、`open_pty`、`setup_child_pty`。
2. 删除 `pty_proxy.rs` 与 `mod pty_proxy`。
3. 删除仅服务 PTY 的 timeout 常量、错误类型和 imports。
4. 删除 `vt100` 依赖。
5. 用 Cargo 命令重新生成 lockfile。
6. 删除或重写 PTY 单元测试；不要保留生产模块只为了测试。
7. 此时再添加 fork-specific 生产代码边界检查，拒绝 `openpty`、`PtyProxy`、`PtyPair`、`setup_child_pty`、attach/detach relay、`TIOCSCTTY` 与 `vt100` 回流。

建议提交：

```text
refactor(cli): remove the PTY proxy implementation
```

### 阶段 8：文档、安全声明与最终回归

**目的：** 让用户可从 README/CLI help 明确预期终端行为。

1. 文档说明 `run`/`shell` 的 child 直接使用调用方 stdio。
2. 文档说明终端是共享通道，不属于 nono 隔离边界。
3. 删除 attach/detach、detached start、terminal approval 和 startup timeout 示例。
4. 保留并重写 session management 文档。
5. 记录后台后代输出与 footer 不保证严格顺序。
6. 记录 Linux `TIOCSTI` 防护与 macOS 差异。
7. 运行完整 Linux 与 macOS CI 矩阵。

建议提交：

```text
docs: define direct terminal semantics for supervised runs
```

## 测试与验收矩阵

### CLI 表面

- `nono --help` 不出现 attach/detach。
- `nono run --help` 不出现 detached、detach timeout 或 startup timeout。
- completions 不包含已删除命令和参数。
- `[ui].detach_sequence` 被配置校验以 removed-field 专用错误明确拒绝，而不是静默忽略或作为普通未知字段处理。

### stdio 继承

- 非 TTY stdin/stdout/stderr 原样继承。
- stdout 或 stderr 重定向时保持各自独立，不被合并。
- pipe 输入输出保持 EOF 和退出码语义。
- tool-sandbox 的非 TTY direct/brokered stdio 行为保持现有语义。

真实 TTY device identity、Ctrl-C、Ctrl-Z/`fg`、resize 与全屏 TUI 行为不纳入本计划的自动化或人工验收矩阵。它们仍遵循前文定义的共享真实终端语义，但本 fork 不为此建设终端测试设施。

### 管理信号

- `SIGTERM` supervisor 会传给 child。
- `SIGUSR1` 不再触发 detach。

### 终端状态所有权

- nono 不保存或恢复 child 修改的 termios。
- child 的 `stty` 修改可以在 `nono run` 返回前继续影响共享终端。
- 外层 shell 或调用方负责重新取得前台终端并恢复自己的状态。
- 不测试或承诺 raw/no-echo、`OPOST`、alternate screen、光标或画面恢复。
- profile-save prompt 现有的局部 `PromptTerminalGuard` 只服务 nono 自己的输入，不构成 session-wide 恢复保证。

### session 管理

- `ps`、`stop`、`logs`、`inspect`、`prune` 继续工作。
- session JSON 不再写 attachment 字段。
- audit events 与 rollback metadata 不依赖 attach socket。
- session name、PID、exit status、profile、workdir、network 与 rollback 字段保持正确。

### approval

- 未配置非终端 backend 的动态请求被拒绝。
- webhook grant/deny/timeout/error 路径保持可审计。
- chain 不允许 terminal backend。
- 运行期间不会打开 `/dev/tty` 或打印 `Grant access? [y/N]`。
- 启动前和退出后 prompt 仍可用。

### 诊断

- child 原始 stderr 直接到达调用方。
- nono 不从 child 文本生成 path grant hint。
- seccomp、proxy、resource limit 和 audit 诊断继续出现。
- profile-save 不使用终端文本推断 grant。
- 后台后代与 footer 的相对顺序不作为断言。

### Linux 安全

- `ioctl(TIOCSTI)` 返回 `EPERM`。
- `TCGETS`、`TCSETS`、`TIOCGWINSZ` 等正常终端操作不受影响。
- filter 继承到 fork/exec 后代。
- filter 安装失败时 child 不执行。
- Landlock、seccomp mediation、network proxy 与 execute allowlist 回归测试全部通过。

### macOS

- `cargo check/test` 通过 macOS runner。
- Seatbelt filesystem/network 行为不变。
- session/audit/rollback 仍工作。
- 文档不宣称 macOS 提供 Linux `TIOCSTI` 等价防护。

### 完整验证命令

每个提交先运行对应的 targeted tests，阶段完成后运行：

```bash
make build
make test
make ci
```

还应在 GitHub Actions 上确认 `ubuntu-latest` 与 `macos-latest` 矩阵通过。普通 CI 不创建测试 PTY，也不引入 Expect 类依赖。`TIOCSTI` seccomp 测试可使用无效或非终端 fd 区分 filter 返回的 `EPERM` 与内核正常返回的 `EBADF`/`ENOTTY`，不必依赖测试终端。

## 长期跟随上游的维护策略

1. 保持上述提交边界，不 squash 成一个巨型删除提交。
2. 上游修改 `exec_strategy.rs` 时，先判断变更属于 sandbox supervision 还是 terminal supervision；只移植前者。
3. 上游新增 attach/detach、PTY、screen parsing 或 terminal approval 修复时，记录为 fork 中不适用，而不是反复解决后再删除。
4. 上游新增 supervisor socket、audit、proxy、resource limit 或 tool-sandbox 安全修复时，正常评估并移植。
5. 保留一个 fork-specific 回归测试，断言生产代码没有 PTY allocation 或 `vt100` 依赖，防止 merge 时功能悄然回流。
6. 定期执行：

```bash
rg -n "openpty|PtyProxy|PtyPair|TIOCSCTTY|attach_to_session|request_session_detach|vt100" crates/nono-cli/src crates/nono-cli/Cargo.toml
```

预期生产代码无匹配。本计划不新增 PTY 测试夹具或交互式终端测试框架。

## 已解决的阻塞项

以下事项均已完成共同决策，实施阶段必须遵守对应结论；后续实现发现新的源码约束时，应回到计划中明确记录，而不是静默改变边界。

### 阻塞 1：`nono stop --force` 与 supervisor 异常死亡后的 child 生命周期

当前 `nono stop --force` 直接向 supervisor PID 发送 `SIGKILL`。删除 PTY 后，不能依赖 PTY master 关闭产生的 hangup 间接终止 child；supervisor 被不可捕获的 `SIGKILL` 杀死后，child 可能继续持有真实终端并运行。

已确认不在本计划处理：

- 这是上游现有 session lifecycle 行为，不为删除 PTY 额外设计 control socket、管理信号、pidfd、PDEATHSIG、cgroup containment 或跨平台进程树终止机制。
- 不尝试保证 supervisor 异常死亡后直接 child 或 descendants 自动退出。
- 不使用共享前台进程组执行 group kill；该进程组可能包含外层 pipeline 中不属于 nono 的进程。
- descendant 即使继续存活，也仍继承已经施加的 Landlock/Seatbelt 限制；这是生命周期问题，不是 sandbox 脱离。
- `nono stop`、`--force` 与 timeout escalation 保持上游语义；本分支只做删除 PTY 后必要的编译适配，不顺带修复该行为。
- 如需推动修复，应作为独立上游 Issue 讨论，不阻塞本 fork 删除 PTY。

### 阻塞 2：非终端 capability approval 的精确配置语义

已确认：

- 主 filesystem capability elevation 使用现有的 `command_policies.approval_defaults.backend`。
- filesystem capability、command policy 与 proxy endpoint 共享这个 default route。
- command policy 和 proxy endpoint 的具体规则仍可显式选择 backend；filesystem capability request 没有单独 route，始终使用 default。
- 不新增 `filesystem.approval_backend` 或第二套 approval schema。
- 未配置 default backend 时，filesystem capability request fail closed 并返回明确 deny reason。
- backend 不可达、timeout、返回错误或 chain 没有任何 granting backend 时均 deny，并在响应与 audit 中保留 backend 名称及原因。
- 删除 `NamedTerminalApproval` 和运行时 `TerminalApproval`；只保留 `Webhook` 与不含 terminal 子项的 `Chain` 运行能力。
- `ApprovalBackendType::Terminal` 仅作为旧 profile 的解析/迁移哨兵保留，不能构造 backend；具体拒绝策略见“旧 session/config 数据迁移”阻塞项。

### 阻塞 3：是否保留 session-wide termios guard

已确认删除：

- 不新增 session-wide termios guard，不捕获 fd 0/1/2 的启动前状态。
- child 最终退出、被信号终止或 supervisor 进入诊断流程时，nono 都不恢复 child 修改的 termios。
- 删除 `discard_late_terminal_input()` 及 cursor-position reply 解析、等待和测试；不扫描、解释或消费任何 ESC 序列。
- 诊断 footer 在 raw、no-echo 或关闭 `OPOST` 的终端上可能显示异常，属于已接受的共享终端风险。
- 外层 shell、终端所有者或用户负责最终恢复；nono 不提供 `reset`/`stty sane` 等补偿逻辑。
- 现有 profile-save prompt 的局部 `PromptTerminalGuard` 保留原状，只在 nono 自己读取输入时临时配置 `/dev/tty`，不形成 child session 的恢复承诺。
- 因此不新增 termios 快照类型、恢复失败 warning、prompt 跳过策略或相关测试 PTY。

### 阻塞 4：macOS 真实终端通道的安全审计

已确认：

- 继承的真实终端 fd 与 `/dev/tty` 是有意交给 child 的外部通道，不属于 Landlock/Seatbelt 文件、网络或凭据隔离边界。
- macOS Seatbelt profile 保持现有 TTY `file-ioctl` 与 `pseudo-tty` 规则；禁止这些操作会破坏 raw mode、窗口尺寸查询以及 child 自建 PTY 等正常用途。
- sandboxed child 可以修改 termios、输出任意终端控制序列，并可能通过平台允许的终端 ioctl 影响共享前台作业。
- Seatbelt 的 signal isolation 不约束由终端驱动投递给前台进程组的信号；因此 macOS 不保证 terminal/supervisor 完整性隔离。
- 该风险可造成终端状态干扰或拒绝服务，但不能扩大 Seatbelt filesystem/network 权限，不构成 sandbox escape。
- Linux 额外拒绝 `ioctl(TIOCSTI)`；macOS 不临时发明等价机制，明确接受平台差异。
- 不为此保留顶层 PTY，不引入独立进程组、`tcsetpgrp()` job-control wrapper 或新的终端代理。

### 阻塞 5：旧 session/config 数据的迁移策略

已确认旧 session registry 采用 best-effort 读取，不做迁移：

- 删除 Rust `SessionRecord::attachment` 后，serde 自然忽略旧 JSON 中的 `attachment` 字段。
- 仍可解析的旧记录继续供 `ps`、`stop`、`logs`、`inspect` 与 `prune` 使用。
- 不重写旧 JSON，不新增 schema version 或一次性 session migration。
- 不让 `prune` 继续识别 attach socket、resize socket 或 detached startup log，也不新增 PTY 遗留物清理逻辑。
- 遗留 socket 交给运行时目录生命周期或系统清理；遗留普通文件保持原样，不主动删除升级前的未知状态。
- 旧版本仍在运行的 detached supervisor 不受新二进制保证；跨版本运行进程不在本计划处理。

已确认旧 `[ui].detach_sequence` 的处理方式：

- 不静默忽略，也不自动修改用户配置文件。
- 保留一个仅用于反序列化检测的 removed-field 入口；不保留 `DetachSequence` 类型、按键解析器或任何运行时用途。
- 配置加载发现该字段时 fail fast，提示：`[ui].detach_sequence is no longer supported because nono no longer provides terminal detach/attach; remove this setting`。
- 不为整个 `UiSettings` 添加 `deny_unknown_fields`，避免顺带改变其他 UI 字段的兼容策略。

已确认旧 terminal approval backend 的处理方式：

- 保留 `ApprovalBackendType::Terminal` 作为纯解析/迁移哨兵，不保留任何 backend 实现或运行能力。
- profile syntax/resolved validation 发现 terminal backend 时，在启动 child 前 fail fast，提示改用 webhook/chain 或删除依赖 approval 的规则。
- `build_approval_backend()` 不得构造 `TerminalApproval`；删除 `NamedTerminalApproval`、`TerminalApproval` 模块和相关测试。
- chain 引用 terminal backend 时同样整体拒绝，不能自动删除子项；否则会静默改变 `all`/`any` 的安全语义。
- 不把 terminal 映射为 deny backend，也不静默忽略。
- 该枚举值仅提供准确迁移诊断，后续 fork 不再需要兼容旧 profile 时可单独删除。

### 阻塞 6：交互式集成测试的 CI 方式

已确认不在 fork 内建设交互式终端测试：

- 不为 Ctrl-C、Ctrl-Z/`fg`、resize、TTY device identity、全屏 TUI 或异常 termios 行为增加自动化测试。
- 不要求这些行为成为本计划的人工 smoke-test 验收项。
- 不自建 session/job-control/PTY harness，不新增 `rexpect`、Pexpect、Expect、`portable-pty` 或 `script` runner。
- 这些测试若对保留 PTY/attach/detach 的上游有价值，应由上游自行决定；本 fork 不先替上游建设再删除。
- fork 只测试自身继续拥有的非 TTY stdio、退出码、管理信号、session、diagnostics 与 sandbox 行为。
- 使用直接的生产代码边界检查防止 PTY allocation、relay、attach/detach 和屏幕模拟依赖回流；交互行为测试不能替代该检查。
- session-wide termios 恢复已从计划删除，因此没有对应测试。
- Linux `TIOCSTI` live filter 测试不需要真实 TTY：对无效或非终端 fd 发出目标 ioctl，要求 seccomp 返回 `EPERM`；未过滤 ioctl 应继续进入内核并返回 `EBADF` 或 `ENOTTY`。

### 阻塞 7：执行顺序是否需要进一步细分

已确认按依赖顺序细分，并要求每个提交保持可编译、相关测试通过且用户可见行为自洽：

- 阶段 0 只固定需要保留的现有行为，不提前提交必然失败的目标测试。
- 先删除 attach/detach 与 detached start，把 PTY 收缩为纯前台 relay；不能先切断底层再留下失效的 CLI 表面。
- terminal approval 与 PTY screen observation 在 inherited stdio 之前删除，避免真实终端输入竞争和隐藏的数据依赖。
- Linux `TIOCSTI` filter 在顶层与 tool-sandbox child 获得真实终端之前安装。
- 顶层 inherited stdio 与 tool-sandbox direct fd 分为两个可独立审查的提交。
- 只有所有生产调用者都移除后才删除 `pty_proxy.rs`、`vt100` 和相关依赖。
- “生产代码没有 PTY”的边界检查与最终模块删除放在同一提交，不能在阶段 0 制造红灯。
- 被删除功能的测试、schema、help 和 completions 必须在对应行为提交中同步更新，不允许提交刻意失败的中间状态。
- 每个提交至少运行 targeted tests 与 `cargo check -p nono-cli --all-targets`；每个阶段完成后运行现有完整测试，最终运行完整 CI。

## 实施前置条件

1. 按仓库贡献政策确认 fork 内存在覆盖本改造的 issue，并记录意图、实现方法、风险与取舍。
2. 在 issue 获得所需项目指导后再开始代码修改。
3. 实施期间不得代表用户向上游提交 Issue、PR 或评论，除非用户另行明确授权。

## 完成定义

只有同时满足以下条件，改造才算完成：

- `nono-cli` 生产代码不创建 PTY。
- `nono-cli` 生产代码不代理终端 I/O。
- attach/detach/detached start 已从 CLI、配置、session model 和文档删除。
- tool-sandbox 不创建嵌套 PTY。
- Supervised 的沙箱、审计、代理、rollback、resource limits 和 session 管理仍工作。
- 运行期间不存在 terminal approval。
- child 终端输出不被 nono 捕获或解析。
- Linux sandboxed child 无法调用 `TIOCSTI`，且安装失败时 fail closed。
- nono 不捕获或恢复 child 的 termios，终端状态所有权边界与文档一致。
- Linux 与 macOS CI 通过。
- 安全文档准确声明终端是共享通道，而非隔离边界。
- 所有阻塞项均已有明确结论并落实到代码或文档。
