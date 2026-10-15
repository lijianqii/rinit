# rinit —— 类 systemd 初始化系统开发计划

> **定位**:使用 Rust 实现的轻量级 PID 1 初始化与服务管理系统,面向资源受限的嵌入式 Linux(aarch64)环境。  
> **目标**:在 128MB 内存约束下,以低内存占用、快启动速度、长期稳定运行为核心指标,提供可替代 systemd 的精简能力。

---

## 0. 项目概述与设计原则

### 0.1 核心目标

| 维度    | 目标              | 量化指标(建议)                        |
| ----- | --------------- | ------------------------------- |
| 内存占用  | 常驻 < 4MB        | 无服务时 RSS ≤ 2MB,20 个服务时 ≤ 6MB    |
| 启动速度  | 内核至首个服务拉起 < 1s  | 事件循环冷启动 ≤ 100ms                 |
| 稳定性   | 7×24 长稳无泄漏      | valgrind/miri 零泄漏,soak 测试 ≥ 72h |
| 二进制体积 | 可执行 ≤ 2MB         | `strip` 后 ≤ 1MB(动态 glibc);静态版 ≤ 4MB(musl/glibc static,见 §2 风险缓解) |

> **体积/RSS 指标为**建议值**:tokio + libc + std 在 release 下通常 1.5~2.5MB,加上 rinit 逻辑可能超 2MB 目标。**M0 首跑后立即实测体积与 RSS,回填校准本表**;必要时对 `rinit-core` 做 no_std + alloc 下沉、裁剪 tokio 特性(见 §2.1②)。这是文档自己预留的校准点,也是当前最不确定的数。

### 0.2 设计原则(贯穿全程)

1. **`rinit-core` 设计为 no_std + alloc,二进制不强制 no_main**:核心引擎与配置解析逻辑按 `no_std + alloc` 编写(不依赖 `std`,仅依赖 `alloc`,避免 panic handler/格式化/堆分配全家桶,保持将来下沉的可能性)。但**整个二进制用 `fn main()` + `std` 起步**(M0 直接落地),不强制 `#[no_main]`;等 profiling 证明 `std` 是瓶颈后,再考虑局部下沉。
2. **无全局可变状态 + 单一所有权**:所有模块状态注入到 `Runtime` 结构体,通过依赖注入传递,禁止 `static mut` / `lazy_static`,这是内存安全和长期稳定性的根基。
3. **事件驱动,而非轮询**:基于 tokio 事件循环(`epoll` 底层)+ `signal`/`time` 统一调度,避免忙等。
4. **显式错误,拒绝 `unwrap`/`expect` 滥用**:生产代码路径禁止 `unwrap`/`panic!`,错误必须向上传播或记录后降级。
5. **最小可运行子集优先(MVP 先行)**:每个阶段都交付一个可 boot 的最小系统,而非等到最后才集成。
6. **fail-closed**:任何配置/状态不确定时,宁可拒绝启动服务并报警,也不带病运行。

---

## 1. 整体架构设计与模块划分

### 1.1 分层架构

```
┌─────────────────────────────────────────────────────┐
│  控制平面  │  rctl CLI  │  Unix Socket 控制协议        │
├─────────────────────────────────────────────────────┤
│  核心引擎  │  事件循环 · 单元管理器 · 状态机 · 调度器  │
│            │  配置解析 · 日志收集 · 关机编排          │
├─────────────────────────────────────────────────────┤
│  运行时原语 │  spawn · cgroup · 健康检测 · 依赖 DAG   │
│            │  日志环形缓冲 · 状态持久化               │
├─────────────────────────────────────────────────────┤
│  平台抽象   │  libc 封装 · 系统调用 · /proc /sys      │
│  (HAL)     │  信号 · 子进程 · devtmpfs 挂载           │
└─────────────────────────────────────────────────────┘
```

### 1.2 Crate / 模块划分

采用 **workspace + 多 crate** 结构,将平台无关与平台相关逻辑隔离,便于测试。

```
rinit/
├── Cargo.toml                 # workspace 根
├── crates/
│   ├── rinit-core/            # ★ 核心引擎(平台无关,可单测)
│   │   ├── src/
│   │   │   ├── unit/          # 单元模型 + 配置解析
│   │   │   │   ├── mod.rs
│   │   │   │   ├── parser.rs      # 简化 unit 文件解析
│   │   │   │   ├── dependency.rs  # 依赖 DAG 构建/拓扑排序/环检测
│   │   │   │   └── config.rs      # 单元配置数据模型
│   │   │   ├── state/         # 状态机
│   │   │   │   ├── mod.rs
│   │   │   │   ├── unit_state.rs  # Inactive→Starting→Active→Stopping...
│   │   │   │   └── transition.rs  # 状态迁移表
│   │   │   ├── manager.rs     # 单元管理器(调度核心)
│   │   │   ├── restart.rs     # 重启/退避策略
│   │   │   └── health.rs      # 健康检测策略定义
│   │   └── ...
│   ├── rinit-runtime/         # 运行时原语(依赖 libc/tokio,平台相关)
│   │   ├── src/
│   │   │   ├── event/         # tokio 信号/定时/IO 事件封装
│   │   │   ├── process.rs     # fork/exec/子进程回收
│   │   │   ├── cgroup.rs      # cgroup v2 接口
│   │   │   ├── logger.rs      # 日志环形缓冲 + 持久化
│   │   │   └── socket.rs      # 控制 socket + socket 激活
│   ├── rinit/                 # ★ 主程序(PID 1 入口)
│   │   └── src/main.rs        # fn main() 入口 + 主循环编排
│   └── rctl/                  # 命令行管理工具(独立二进制)
│       └── src/main.rs
├── units/                     # 示例服务单元文件
├── tests/                     # 集成测试
└── scripts/                   # qemu 启动/测试脚本
```

### 1.3 核心模块职责详解

#### (1) `unit` 模块 —— 单元模型与解析

**简化 unit 文件格式**(不兼容 systemd,但保留其直觉)。文件由 `[Unit]`、`[Service]` 两个段组成(`[Unit]` 可选,`[Service]` 对 target 单元除外),`Key=Value` 形式,`#` 为注释,支持跨行续行(行尾 `\`)。

**完整示例**:

```ini
# 注释行
[Unit]
Description=Example daemon
Documentation=https://example.com
Requires=network.target dbus.service
Wants=metrics.socket
After=network.target
Before=app.service
Conflicts=debug.service

[Service]
Type=simple              # simple | oneshot | notify (forking 为 P2)
ExecStart=/usr/bin/example-daemon --foreground
ExecStop=/usr/bin/example-daemon --stop
ExecStartPre=/usr/bin/mkdir -p /var/lib/example
ExecStartPost=/usr/bin/echo started
Restart=on-failure        # no | always | on-success | on-failure | on-abnormal
RestartSec=2
StartLimitBurst=5
StartLimitIntervalSec=10
TimeoutStartSec=30
TimeoutStopSec=10
User=nobody
Group=nogroup
NoNewPrivileges=true
Environment=FOO=bar PATH=/usr/bin
EnvironmentFile=/etc/default/example
WorkingDirectory=/var/lib/example
HealthCheck=/usr/bin/example-health-check
HealthIntervalSec=5
HealthTimeoutSec=10
HealthRestartOnFailure=true
CgroupMemoryMax=32M
CgroupCPUMax=50%
CgroupTasksMax=64
```

**关键设计点**:

- 解析器实现为**纯函数**(`&str` → `Result<UnitConfig, ParseError>`),零全局状态,便于 fuzz 与单测。
- 依赖解析:在 `rinit-core` 内构建有向图,启动时做**拓扑排序 + 环检测**(Kahn 算法或 DFS),环报错并指名环路径。
- **引用完整性检查(M1,DAG 构建期)**:`Requires=`/`After=` 等引用的单元若在 `/etc/rinit/units/` 下**不存在对应单元文件**,在**解析后、启动前**即报错(`MissingDependency`),而非等到运行期才暴露;`Wants=` 指向缺失单元则**降级为告警**(弱依赖,尽力而为)。
- `Requires`(强依赖,失败则本单元失败)与 `Wants`(弱依赖,尽力而为)、`After/Before`(仅排序不构成依赖)语义区分清楚。

#### (1.0) 文件格式 EBNF(解析器实现依据)

```ebnf
unit-file      = { section } ;
section        = section-header , { entry } ;
section-header = "[" , section-name , "]" , eol ;
section-name   = "Unit" | "Service" ;
entry          = key , "=" , value , eol ;
key            = ALPHA , { ALPHA | DIGIT } ;          (* 配置项名,见 §1.1 契约表 *)
value          = { value-token } ;                    (* 可能为空 → 见空值语义 *)
value-token    = quoted-string | unquoted-char ;
quoted-string  = DQUOTE , { quoted-char } , DQUOTE ;
quoted-char    = escape-seq | (ANY - DQUOTE - "\") ;
escape-seq     = "\" , ( "\" | DQUOTE | "n" | "t" | "s" ) ;
unquoted-char  = ANY - ( " " | "\t" | "#" | "\" ) ;   (* 空格/注释/#/引号需特殊处理 *)
eol            = "\n" | ( "\r" , "\n" ) ;             (* 兼容 CRLF *)
comment        = "#" , { ANY - "\n" } , eol ;            (* 裸 # 一律视为注释起点 *)
continuation   = "\" , eol ;                           (* 行尾反斜杠续行 *)
```

**词法与语义规则**(必须明确,否则 fuzz 会暴露歧义):

- **分词**:`key = value` 以第一个 `=` 为界,`key` 两侧空白忽略;`value` 开头与结尾的空白**剥离**,内部连续空白折叠为单个空格(列表项如 `Requires` 用空格分隔)。
- **引号**:`"..."` 内的空格、`#`、`=` 均视为字面值;引号内的 `\"` `\\` `\n` `\t` `\ `(空格)为转义;引号未闭合报 `InvalidValue`。
- **转义**:未加引号的 `value` 中,`\` 也是转义符(`\ ` 表示空格、`\#` 表示字面 `#`),与引号内一致。
- **空值语义**:`key=`(等号后无内容)表示**空字符串**;对列表项(`Requires`/`Wants` 等)空字符串 = 空列表;对 `bool` 空 = 非法(报 `InvalidValue`);对 `String`(如 `Description`)空 = 合法空串。`ExecStart` 空 = 非法(`MissingExecStart`)。
- **注释**:裸 `#` 一律视为注释起点(EBNF 的 `comment` 规则);未引号值中若需字面 `#`,必须转义为 `\#`(如 `Environment=PATH=/usr/bin\#app`)。引号内 `#` 为字面值,无需转义。此规则简单无歧义。
- **续行**:行尾 `\` 表示下一行续接(拼接时以一个空格连接),续行不能出现在注释行。

**命令分词规则(ExecStart/ExecStop/ExecStartPre/ExecStartPost/HealthCheck 等命令类配置项)**:

`value` 解析完成后,命令类配置项还需进一步分词为 `argv`。规则为**最小 POSIX 风格分词,不执行 shell 元字符**:

| 规则         | 说明                                                                 |
| ----------- | ------------------------------------------------------------------ |
| 空白分隔       | 连续空白(空格/`\t`)折叠为单个分隔符,切分为多个参数                                     |
| 双引号         | `"..."` 内保留空格(含 `\t`),作为**单个参数**;引号本身不进入参数                        |
| 转义          | `\"`→`"`、`\\`→`\`、`\ `→字面空格、`\#`→字面 `#`;`\` 后跟其他字符则该字符作字面值          |
| **不支持 shell**  | 不执行 `|`、`>`、`<`、`&&`、`;`、`$()`、反引号等元字符,它们一律按普通字面字符进入参数(不解析重定向/管道) |
| **不支持变量展开** | `$FOO`/`${FOO}` 不展开,原样作为字面参数(变量注入走 `Environment=`/`EnvironmentFile=`)  |
| **不支持重定向**  | `>`、`>>`、`<` 不产生重定向,原样作为参数;需要重定向由服务自身或 `ExecStartPre` 脚本处理          |
| 空命令        | 分词后 argv 为空 → 报 `MissingExecStart`(与 §1.1 一致)                       |

**示例**:

```
ExecStart=/bin/echo "hello world"      # → ["/bin/echo", "hello world"]  (1 个含空格参数)
ExecStart=/bin/sh -c "echo hi"         # → ["/bin/sh", "-c", "echo hi"]  (显式 shell,合法)
ExecStart=echo a\ b                    # → ["echo", "a b"]               (转义空格)
ExecStart=echo hi > /tmp/x             # → ["echo", "hi", ">", "/tmp/x"] (重定向不解析,字面)
```

> **`ExecStartPre`/`ExecStartPost` 是 `Vec<String>`**:每个元素一行一个命令,逐行独立分词;`ExecStart`/`ExecStop` 是单行命令。

#### (1.1) unit 文件配置项完整定义

> 这是解析器与所有下游模块的**契约**。类型标注为 Rust 侧落地类型,取值、默认值、语义必须与解析器实现严格一致。

##### `[Unit]` 段(元信息与依赖)

| 配置项             | 类型              | 取值/格式              | 默认值       | 含义                                       |
| --------------- | --------------- | ------------------ | --------- | ---------------------------------------- |
| `Description`   | `String`        | 任意文本               | 空         | 单元的人类可读描述,仅用于 `rctl status` 展示           |
| `Documentation` | `String`        | URL                | 空         | 文档链接,仅元信息                                |
| `Type`          | enum            | `service`/`target` | `service` | 单元类型;`target` 为无进程汇聚单元(见 §7.2)。**注意与 `[Service]` 段的 `Type` 同名但含义不同**:此处是单元类型(service/target),后者是启动类型(simple/oneshot/notify) |

> **同名 `Type` 提示**:`[Unit] Type`(单元类型:service/target)与 `[Service] Type`(启动类型:simple/oneshot/notify)是两个独立配置项,`key` 同名但所属段不同、语义不同。解析器按"段 + key"定位,不会混淆。
| `Requires`      | `Vec<UnitName>` | 空格分隔单元名            | 空         | **强依赖**:这些单元必须成功激活,否则本单元失败;隐含 `After` 排序 |
| `Wants`         | `Vec<UnitName>` | 空格分隔单元名            | 空         | **弱依赖**:尽力激活,失败不影响本单元;隐含 `After` 排序      |
| `After`         | `Vec<UnitName>` | 空格分隔单元名            | 空         | **仅排序**:在这些单元之后启动,不构成激活依赖                |
| `Before`        | `Vec<UnitName>` | 空格分隔单元名            | 空         | 仅排序:本单元先于这些单元启动                          |
| `Conflicts`     | `Vec<UnitName>` | 空格分隔单元名            | 空         | 互斥:启动本单元时若冲突单元在运行,则先停止它                  |

**依赖语义要点**:

- `Requires`/`Wants` 同时产生**激活依赖**和**排序约束**(等价于也加了一条 `After` 边)。
- `After`/`Before` 纯排序,若所指单元未激活也不阻塞本单元(与 systemd 一致)。
- `Conflicts` **不传递**:`A Conflicts B`、`B Conflicts C` 时,`A` 与 `C` 可共存(互斥只在直接声明对之间成立)。
- 依赖名解析:带 `.service`/`.target` 后缀按后缀识别类型;无后缀时默认按 `.service` 处理。单元类型也可由文件自身的 `[Unit] Type=` 显式指定;**后缀与 `Type=` 冲突时 fail-closed 拒绝加载并报错**(如 `foo.target` 文件内写 `Type=service` → 报 `InvalidValue`,而非静默覆盖)。

##### `[Service]` 段(进程与运行)

| 配置项                      | 类型               | 取值/格式                                                 | 默认值      | 含义                   |
| ------------------------ | ---------------- | ----------------------------------------------------- | -------- | -------------------- |
| `Type`                   | enum             | `simple`/`oneshot`/`notify`(`forking` 为 P2,见 §7.10) | `simple` | 启动完成判定方式(见下方类型说明)    |
| `ExecStart`              | `String`         | 命令 + 参数                                               | 必填       | 启动命令(除非 target 单元)   |
| `ExecStartPre`           | `Vec<String>`    | 命令                                                    | 空        | 主命令前依次执行,任一失败则启动失败   |
| `ExecStartPost`          | `Vec<String>`    | 命令                                                    | 空        | 主命令启动后执行,失败不影响主服务    |
| `ExecStop`               | `String`         | 命令                                                    | 空        | 停止命令;为空则发 `SIGTERM`  |
| `Restart`                | enum             | `no`/`always`/`on-success`/`on-failure`/`on-abnormal` | `no`     | 退出后是否重启及条件           |
| `RestartSec`             | `u64`(秒)         | 正整数                                                   | `2`      | 重启退避**基准值**(指数退避序列首项,见 §1.3(4))  |
| `StartLimitBurst`        | `u64`            | 正整数                                                   | `5`      | 时间窗内允许的失败次数上限        |
| `StartLimitIntervalSec`  | `u64`(秒)         | 正整数                                                   | `10`     | 失败计数时间窗              |
| `TimeoutStartSec`        | `u64`(秒)         | 正整数                                                   | `30`     | 启动超时,超时判 Failed      |
| `TimeoutStopSec`         | `u64`(秒)         | 正整数                                                   | `10`     | 停止超时,超时发 `SIGKILL`   |
| `User`                   | `String`         | 用户名/uid                                               | 空(root)  | 降权运行用户               |
| `Group`                  | `String`         | 组名/gid                                                | 空        | 运行组                  |
| `NoNewPrivileges`        | `bool`           | `true`/`false`                                        | `false`  | 禁止子进程提权(setuid 失效)   |
| `CapabilityBoundingSet`  | `Option<Vec<Cap>>` | 能力名空格分隔;`None`=未设置,`Some(vec![])`=显式清空            | `None`(继承) | 可继承的能力集上限(P1,见 §7.4) |
| `AmbientCapabilities`    | `Option<Vec<Cap>>` | 能力名空格分隔;`None`=不授予,`Some(vec![])`=显式空(无意义但语义清晰) | `None`(不授予) | 授予的 ambient 能力(P1)   |
| `Environment`            | `Vec<(K,V)>`     | 空格分隔 `K=V`(含空格的值用引号或 `\` 转义,与命令分词规则一致)           | 空        | 注入环境变量               |
| `EnvironmentFile`        | `String`         | 路径                                                    | 空        | 读取文件注入环境变量(格式见下)   |
| `WorkingDirectory`       | `String`         | 路径                                                    | `/`      | 工作目录                 |
| `HealthCheck`            | `String`         | 命令                                                    | 空        | 健康检测命令,退出码 0 为健康     |
| `HealthIntervalSec`      | `u64`(秒)         | 正整数                                                   | `30`     | 健康检测周期               |
| `HealthTimeoutSec`       | `u64`(秒)         | 正整数                                                   | `10`     | 单次检测超时               |
| `HealthRestartOnFailure` | `bool`           | `true`/`false`                                        | `false`  | 健康失败是否触发重启           |
| `CgroupMemoryMax`        | `u64`(字节)        | `32M`/`1G`/纯数字                                        | 空(不限)    | 内存硬上限(P1,见 §7.3)     |
| `CgroupMemoryHigh`       | `u64`(字节)        | `24M`/纯数字                                             | 空(不限)    | 内存软上限(先回收 cache,P1)  |
| `CgroupCPUMax`           | `CpuQuota`(enum) | `50%` 或 `quota period`(如 `200000 100000`)             | 空(不限)    | CPU 上限               |
| `CgroupTasksMax`         | `u64`            | 正整数                                                   | 空(不限)    | 进程/线程数上限             |

> **数值 0 值语义(关键)**:
>
> - `RestartSec=0` → **立即重启,无退避**(跳过退避序列)。
> - `StartLimitIntervalSec=0` → **禁用熔断**(不计数,永不进入 `StartLimitHit`)。
> - `TimeoutStartSec=0` → **无超时**(永久等待启动完成,`notify` 类型慎用)。
> - `TimeoutStopSec=0` → **立即 `SIGKILL`**(不等待优雅退出)。
> - 其余数值配置项不接受 0(报 `InvalidValue`)。

> **路径语义**:
>
> - `EnvironmentFile`/`WorkingDirectory` **仅支持绝对路径**;相对路径报 `InvalidValue`(避免相对路径依赖进程 CWD 的不确定性)。
> - `ExecStart` 命令路径不存在:fork+exec 后 `execve` 失败 → 子进程 `_exit(127)` → 父进程 `waitpid` 看到退出码 127,判启动 `Failed`。

> **`Option` 空值语义(关键)**:`CapabilityBoundingSet`/`AmbientCapabilities` 用 `Option<Vec<Cap>>` 而非 `Vec<Cap>`,以区分三种状态:
>
> - `None` = **用户未设置** → `CapabilityBoundingSet` 继承(不改动)、`AmbientCapabilities` 不授予。
> - `Some(vec![])` = **用户显式写空**(`CapabilityBoundingSet=` 或 `AmbientCapabilities=`) → 清空/显式空。
> - `Some(vec![...])` = 显式指定能力列表。
>
> 解析器必须能区分"未设置"与"显式清空",否则 `CapabilityBoundingSet=`(清空)会与默认(继承)在语义上撞车。

> **`Environment` / `EnvironmentFile` 格式**:
>
> - `Environment` 每项 `K=V`,含空格的值用 `"..."` 或 `\ ` 转义(与命令分词规则一致),如 `Environment=FOO="a b" PATH=/usr/bin`。
> - `EnvironmentFile` 每行一个 `K=V`,支持 `#` 注释(裸 `#` 视为注释,与 unit 文件规则一致),**不支持多行续接、不支持变量展开**(如 `${FOO}` 不展开)。空行忽略。
> - `EnvironmentFile` 中同名的 `K` 与 `Environment` 里的定义,后者(`Environment` 直接项)优先。

##### `Type` 启动类型语义详解

> **决策(已定)**:M0~M2 仅支持 `simple`/`oneshot`/`notify` 三种类型,`forking` **移入 P2 扩展项**(见 §7.10)。原因:传统 daemon 常 double-fork,守护进程被 PID 1 收养后,rinit 无法把"这个被收养的孤儿"与某个 unit 关联(无 PIDFile、M2 又未启用 cgroup),`SIGCHLD`+`waitpid` 只能看到主进程退出、看不到守护进程 PID,导致无法在守护进程崩溃时重启它。嵌入式场景绝大多数服务可改为前台运行,砍掉 forking 是最省事且正确的方案。

| Type      | 启动完成判定                     | 适用场景             | 注意                                               |
| --------- | -------------------------- | ---------------- | ------------------------------------------------ |
| `simple`  | fork 后立即视为启动完成             | 常驻前台进程           | 最常用,无父子分离                                        |
| `oneshot` | 主命令退出(码 0)即完成              | 初始化/一次性任务        | 无长期进程,成功后进入 `Active(Exited)` 态(见 §1.3(3)),依赖视为满足 |
| `notify`  | 等待子进程通过 socket 发 `READY=1` | 支持 sd_notify 的服务 | 最精确,但要求服务配合(协议见 §1.3(12))                          |

> `forking`(P2,见 §7.10):若日后确有无法前台化的守护进程,通过 `PIDFile=` + cgroup v2 跟踪(cgroup 已 M5 启用)实现,不在 M0~M2 范围。

##### `Restart` 策略语义详解

> 语义与 systemd 保持一致(`on-abnormal` 是 `on-failure` 的子集,区别在于不含"非零退出码"):

| 取值            | 触发重启的退出情况                                       |
| ------------- | ----------------------------------------------- |
| `no`          | 永不重启                                            |
| `always`      | 无论退出码/信号都重启(含退出码 0)                              |
| `on-success`  | 仅退出码 0 且未收到信号时重启                                 |
| `on-failure`  | **非 0 退出码 + 被信号终止 + 超时 + watchdog**(即"异常"的完整集合) |
| `on-abnormal` | **被信号终止 + 超时 + watchdog**(不含非 0 退出码)              |

> 边界澄清(消除重叠歧义):
>
> - `on-failure` = 非 0 退出码 ∨ 被信号终止 ∨ 超时 ∨ watchdog。
> - `on-abnormal` = 被信号终止 ∨ 超时 ∨ watchdog(与 `on-failure` 之差:仅不含"非 0 退出码")。
> - `on-success` = 退出码 0 且未被信号终止;被信号终止即使退出码 0 也**不**重启。

> 重启次数受 `StartLimitBurst`/`StartLimitIntervalSec` 熔断约束,超过则进入 `failed` 不再重启。

##### 解析器错误分类(`ParseError`)

| 错误类型               | 场景                           |
| ------------------ | ---------------------------- |
| `UnknownSection`   | 出现 `[Unit]`/`[Service]` 之外的段 |
| `UnknownKey`       | 段内出现未定义配置项                   |
| `InvalidValue`     | 取值类型/枚举非法(如 `Restart=maybe`) |
| `MissingExecStart` | service 单元缺 `ExecStart`      |
| `DuplicateKey`     | 单值配置项重复出现                    |
| `DuplicateUnit`    | 加载阶段发现两个文件定义**同名单元**(非单文件内,跨文件)   |
| `MissingDependency` | `Requires=`/`After=` 引用的单元文件不存在(启动前检测)    |

> 解析失败策略:**整文件拒绝加载**,错误信息含 `文件:行号:原因`,便于定位。`DuplicateUnit` 属于**加载期**(跨文件)检测:扫描 `/etc/rinit/units/` 时若两个文件解析出同名单元,整体拒绝并报错。

#### (2) `manager` 模块 —— 单元管理器(调度核心)

- 持有 `alloc::collections::BTreeMap<UnitName, Unit>` 与状态机(`rinit-core` 为 `no_std`,不用 `std::collections::HashMap`;`BTreeMap` 无需外部 hash 依赖,且保证确定性遍历顺序,便于测试与日志复现)。
- 对外提供 `start/stop/restart/reload/status` 操作。
- 依赖满足后触发并行启动:使用**有向无环图(DAG)+ 就绪信号**调度,而非串行遍历。
- 关键并发模型:调度器产生"待执行任务"队列,交给运行时层异步执行,通过 `watch_channel` / 事件回调回报状态。

#### (3) `state` 模块 —— 状态机

```
                         ┌─────────────┐
                         │   Inactive  │◄──────────────────────┐
                         └──────┬──────┘                       │
                          start │                              │
                         ┌──────▼──────┐    start-up complete  │
              ┌─────────►│   Starting  ├──────────────┬────────┤
              │          └──────┬──────┘              │        │
              │                 │ failed              │(oneshot)│
              │          ┌──────▼──────┐       ┌──────▼──────┐  │
              │          │    Failed   │       │    Active   │  │
              │          └──────┬──────┘       └──┬───┬───┬──┘  │
              │                 │(退避)      stop │   │   │     │
              │          ┌──────▼──────┐          │   │   │     │
              │          │ RestartWait │◄─────────┘   │   │     │
              │          └──────┬──────┘   (异常退出   │   │     │
              │                 │           +满足Restart)│     │
              │   (退避超时/熔断) │                     │   │     │
              │                 └────────► Failed ◄────┘   │     │
              │                    (异常退出+不满足Restart) │     │
              │                                            │     │
        ┌─────┴──────┐              ┌───────────────┐      │     │
        │Active(Exited)│             │              │      │     │
        └─────┬──────┘              │              │      │     │
              │ stop/reset          │              │      │     │
              │                     ▼              ▼      │     │
              │              ┌─────────────────────────┐ │     │
              └─────────────►│      Deactivating       │◄┘     │
                             └────────────┬────────────┘       │
                                          │ stop 完成            │
                                          └─────────────────────┘
```

> 图注(文字语义为准,图仅示意):
>
> - **主启动路径**:`Inactive --start--> Starting --start-up complete--> Active`(或 oneshot 成功 → `Active(Exited)`)。
> - **失败路径**:`Starting --failed--> Failed`。
> - **重启路径**:`Failed --RestartSec 退避--> RestartWait --退避超时--> Starting`;退避超时或熔断则留在 `Failed`。
> - **`Active` 异常退出(两条,右列)**:满足 `Restart` → `RestartWait`;不满足 → `Failed`。
> - **停止路径**:`Active`/`Active(Exited)` --stop--> `Deactivating` --stop 完成--> `Inactive`;`Active(Exited)` 无进程,`Deactivating` 空转后立即到 `Inactive`。
> - **手动 start 打断退避**:`RestartWait --start--> Starting`(取消退避,见 §1.3(13))。

> 注:
>
> - `RestartWait` 是 `Failed`(或 `Active` 异常退出)到再次 `Starting` 之间的短暂退避态,由 `RestartSec` 控制,退避超时或熔断(`StartLimitBurst`)则留在 `Failed` 不再重启。
> - `Active(Exited)` 是 `oneshot` 类型服务命令成功退出(码 0)后的状态:无存活进程,但**仍视为已激活**,其 `Requires`/`After` 依赖视为满足,下游可继续启动。与 `Inactive` 的区别在于它保留了"本次已成功运行过"的语义,供依赖判定与 `rctl status` 区分。
> - **`Active(Exited)` 的停止路径**:无存活进程可停,故 `Active(Exited) --stop--> Inactive` **直接迁移**(不进 `Deactivating`,避免空转);若实现上仍走 `Deactivating`,则需说明此路径下 `TimeoutStopSec` **不生效**(无进程可杀,立即完成)。测试断言以"直接到 `Inactive`"为准。
> - **与 systemd 的有意差异**:systemd 的 `oneshot` 默认退出后回 `inactive`,需 `RemainAfterExit=yes` 才保持 active;rinit 的 `oneshot` 成功退出后**总是**进入 `Active(Exited)`,**不提供 `RemainAfterExit=`**(无需该配置项)。这是简化决策,文档明确记录。

状态迁移表采用 `match` 显式穷举(编译期保证不漏状态),禁止隐式 fallthrough。

#### (4) `restart` 模块 —— 重启退避策略

- 实现指数退避 + `StartLimitBurst/StartLimitIntervalSec` 熔断(在时间窗内失败 N 次则进入 `failed` 不再重启)。
- **退避序列(明确定义)**:
  - `RestartSec` 是**基准值**(默认 2s),第 `n` 次连续重启的延迟 = `RestartSec * 2^(n-1)`。
  - **上限** = `RestartSec * 2^5`(即 32 倍,如默认 2s → 最大 64s),到达上限后不再增长。
  - **抖动(jitter)**:`delay = base + Random(0, base/4)`(**只加不减**,避免延迟缩短导致惊群),避免多个服务同时重启造成惊群。
  - **`n` 重置条件(关键)**:`n` 在服务成功进入 `Active` 且**持续运行超过 `StartLimitIntervalSec`** 后重置为 1(与 systemd 的失败计数重置对齐,保守策略:避免抖动服务反复 2s 重启);若进程只跑很短(< `StartLimitIntervalSec`)就崩溃,`n` 继续累加。即"稳定运行一个窗口时长才算一次成功"。
  - 序列示例(基准 2s):`2 → 4 → 8 → 16 → 32 → 64 → 64 → 64...`(每次叠加 `Random(0, base/4)` 抖动)。
- **熔断窗口**:`StartLimitIntervalSec` 使用**滑动窗口**,记录最近 N 次失败的时间戳;窗口内失败次数 ≥ `StartLimitBurst` → 进入 `failed` 不再重启。
- 退避上限与抖动(jitter)避免惊群重启。

#### (5) `health` 模块 —— 健康检测

- 支持三种策略:进程存活检测、`HealthCheck` 命令退出码检测、`Type=notify` 的 `READY=1` 通知。
- 健康失败与崩溃区分对待:崩溃走重启策略,健康失败走"标记 unhealthy + 可选重启"。

#### (6) `runtime` 的 `process` 模块 —— 进程、进程组与僵尸回收

- 进程创建:普通服务用 `libc::posix_spawn`,需降权/能力设置的用受控 fork+exec(取舍见 §2.3「进程创建」)。
- **进程组**:每个服务默认创建**新进程组**(fork 后子进程 `setpgid(0,0)`),这样停止时能对整棵子进程树发信号;停止服务用 `kill(-pgid, SIGTERM)`(而非只杀单个 PID),确保守护进程再 fork 出的孙进程也能收到停止信号。
- **session/控制终端**:默认**不** `setsid`;仅 `getty` 类需要控制终端的服务才 `setsid` + 打开控制终端,对应单元配置加 `TTYPath=`/`TTYReset=`(P2,见 §7.10)。
- 作为 PID 1,`SIGCHLD` 由 `tokio::signal` 捕获,在 `spawn_blocking` 内循环 `waitpid(-1, ..., WNOHANG)` 直到 `ECHILD`,**统一回收所有子进程**(无论 `posix_spawn` 还是 fork+exec 创建),这是僵尸回收的唯一正确姿势(避免 `SIG_IGN` 在非 Linux 上不可靠,以及要处理多个同时退出的子进程)。
- 子进程 `double-fork` 不推荐(会导致失控进程脱离监控),统一由 rinit 直接管理。
- **孤儿进程**:PID 1 本身就是所有孤儿进程的父进程(内核保证),**无需也不应**设置 `PR_SET_CHILD_SUBREAPER`(该 prctl 只对非 PID 1 的进程有意义);被收养的孤儿通过上述统一 `waitpid` 循环一并回收。

#### (7) `logger` 模块 —— 日志

- 每个服务 stdout/stderr 重定向到 rinit 持有的管道,读端接入事件循环。
- 缓冲策略(**二选一**,按服务数量在编译期/配置期决定):
  - **每服务独立环形缓冲**:默认 16–64KB/服务(嵌入式内存约束下的量级,可按单元配置调),超限自动丢弃最旧。
  - **全局共享环形缓冲**:服务数量多(如 >20)时改用单一共享缓冲,按 `<unit>` 前缀区分,省去 N 个独立缓冲的元数据与碎片。
- `journalctl` 式按需查询,支持 `rctl status <unit> --tail` 查看。
- 可选持久化到 `/var/log/rinit/` 或 tmpfs(仅全局缓冲场景默认持久化,每服务缓冲默认不落盘)。

#### (8) 关机/重启编排

- 处理 `SIGINT/SIGTERM/SIGPWR` + `reboot(RB_AUTOBOOT)` 系统调用。
- 按依赖**逆拓扑顺序**优雅停止服务,`TimeoutStopSec` 超时后 `SIGKILL`。
- **`Conflicts` 对的停止顺序**:关机逆拓扑停止若遇到 `Conflicts` 对,**先停"被冲突方"再停"发起方"**(与启动顺序相反,保证互斥关系在停机时也一致解消)。
- 卸载文件系统(除 root)、`sync`、最后调用 `reboot(2)`。
- **兜底**:`reboot(2)` 返回失败或停止阶段卡死时,走终极方案 —— `sysrq`(`echo b > /proc/sysrq-trigger`)或直接 `exec` 到 `busybox reboot -f`;若仍有硬件 watchdog(§7.5)则停止喂狗让其复位。关机路径不允许返回"关不掉"的状态。

#### (9) 内置初始化单元(`rinit-init.service`)

rinit 在启动早期**内置**一个逻辑初始化单元,负责服务启动前必须完成的系统级准备,不依赖外部单元文件。该单元以 `oneshot` 语义运行于所有服务单元之前,是所有 `After=rinit-init.service` 引用的锚点。

##### 挂载时序(两波挂载,决策已定)

**第一波 · 硬编码挂载内核虚拟文件系统**(`rinit-init.service` 内,顺序固定):

| 顺序 | 挂载点       | 类型       | 失败处理 | 说明                                                                 |
| -- | --------- | -------- | ---- | ------------------------------------------------------------------ |
| 1  | `/run`    | tmpfs    | 致命   | **必须最先挂**,后续 PID 文件/socket/控制 socket 都依赖它;initramfs 里的 `/run` 可能只读 |
| 2  | `/proc`   | proc     | 致命   | 后续 cgroup、`/proc/mounts` 读取都依赖;失败即 abort                           |
| 3  | `/sys`    | sysfs    | 致命   | cgroup 前置                                                          |
| 4  | `/dev`    | devtmpfs | 致命   | `mount -t devtmpfs devtmpfs /dev`,内核自动填充,无需 udev                   |
| 5  | `/tmp`    | tmpfs    | 可降级  | 失败仅告警,不阻断启动                                                        |
| 6  | cgroup v2 | cgroup2  | 可降级  | **预留代码位,M5 启用**,默认跳过(见下)                                           |

挂载调用走 `rinit-runtime::syscall` 薄封装(直接 `libc::mount`,见 §2.3),在 `spawn_blocking` 内执行(阻塞 syscall,遵循 §2.1⑤)。

**挂载前目录存在性**:`mount(2)` 要求挂载点已存在。initramfs 里可能缺 `/run`、`/proc`、`/sys`、`/tmp`,故**每个致命挂载点挂载前先 `mkdir -p`**(`/dev` 由 devtmpfs 挂载时内核自动创建,可省)。顺序:先 `mkdir` 再 `mount`。

**"abort" 的具体动作(致命挂载失败)**:

- 致命挂载(`/run`/`/proc`/`/sys`/`/dev`)失败时的"abort"**不是**简单 `panic!` 后挂死,而是:
  1. 尝试 `reboot(2)` 系统调用(`RB_AUTOBOOT`)立即复位 —— 这是 fail-closed 的正确姿态(复位而非黑屏挂死)。
  2. 若 `/proc` 挂载失败(极早期,`/proc` 本身不可用,`sysrq-trigger` 不可用),仍走 `reboot(2)`(不依赖 `/proc`)。
  3. 若 `reboot(2)` 也失败,才进入死循环 `for(;;) pause()`,等硬件 watchdog(§7.5)复位。
- 即"致命失败 → 立即复位",绝不允许 PID1 静默退出导致内核 panic 后挂死。

**第二波 · 静态 fstab 解析**(可选,`/etc/fstab` 存在时才执行):

- 极简解析器:仅支持 `device mountpoint fstype options dump pass` 六字段,忽略 `noauto` 项,不做 UUID/LABEL 解析(嵌入式分区通常用设备节点或 PARTUUID,按需最小实现)。
- **不做 generator、不生成 mount 单元**:直接按 fstab 顺序逐个 `mount(2)`,失败记日志并继续(与 systemd 的依赖排序解耦,换简单换内存)。
- 典型用途:挂载可写数据分区(如 `/data`)、持久化日志分区。
- 触发条件:内核 cmdline 无 `nofstab` 时才解析;initramfs 阶段若 root 已是最终 rootfs,则跳过 fstab。

##### 其他职责

- 设置主机名(可选)、安装 PID1 信号 handler、创建控制 socket。
- **cgroup v2 预留**:`/sys/fs/cgroup` 挂载代码位留好,默认由编译 feature(`cgroup`)或配置文件开关控制;M0~M4 不挂不影响核心功能,M5 打开即可,避免启动时序回改。

##### 与 systemd 的差异(明确不做)

| systemd 做法                                      | rinit 做法                      | 理由                                       |
| ----------------------------------------------- | ----------------------------- | ---------------------------------------- |
| `systemd-fstab-generator` 把 fstab 转 `.mount` 单元 | 静态 fstab 直接 `mount(2)`        | 嵌入式无动态挂载/依赖排序需求,省 generator 进程与 mount 单元 |
| mount 单元纳入依赖引擎                                  | 挂载是顺序动作,非单元                   | 挂载失败只告警不参与依赖拓扑                           |
| 第三波服务私有挂载(`PrivateTmp=` 等)                      | 不做(仅 `WorkingDirectory=` 基础项) | 见 §2.2 沙箱裁剪                              |

#### (10) 控制 socket 安全(`/run/rinit/control.sock`)

- **文件权限**:`bind` 后 `chmod 0600`(仅 root 可连接),socket 放在 `/run/rinit/` 目录(该目录 `0700`)。
- **umask 竞态防护**:`bind(2)` 创建的 socket 权限由进程 `umask` 决定,`chmod` 前存在极短权限窗口。因 `/run/rinit/` 目录 `0700` 已挡住非 root 访问,此窗口**实际为零**;但为稳妥,`bind` 前临时设 `umask(0o177)`,`bind` 后恢复。
- **连接鉴权**:每个新连接用 `getsockopt(SO_PEERCRED)` 读取对端 `pid/uid/gid`,**非 root(uid != 0)一律拒绝**;鉴权发生在读取任何命令之前。
- **协议最小化**:命令解析用自研二进制协议(长度前缀 + 命令码),拒绝未授权命令、超长 payload(防资源耗尽)。**协议头含版本号字段**,`rctl` 连接后先校验版本,**硬拒绝**(返回错误码,`rctl` 提示升级),不做向后兼容降级。
- 这套设计使 `rctl` 无需守护进程凭证,依赖 Unix socket 权限模型本身即可安全。

#### (11) Job 队列与目标状态合并

即使简化版也引入 **Job 队列**概念,把"用户/依赖请求的最终状态"与"当前实际状态"解耦:

- **目标状态(target state)**:每个单元维护一个"期望状态"(`Started`/`Stopped`),来自用户命令(`rctl start/stop`)或依赖图(`Requires`/`Conflicts`/关机)。
- **实际状态(actual state)**:状态机的真实落点(`Active`/`Inactive`/`Failed` 等)。
- **Job**:一个单元的一次"目标状态变更"为一个 Job,进入全局 Job 队列;调度器按依赖拓扑消费 Job 队列,把目标状态**合并**为对每个单元的单一期望(如一个单元同时被多个依赖要求 started,只计一次)。
- **合并规则(优先级优先于时间戳,正式规则)**:`start` 与 `stop` 冲突时,按以下**优先级**裁决,同优先级才按最新时间戳:
  1. **关机 stop > 用户 stop > 用户 start > 依赖 start**。
  2. 同优先级冲突时,取**最新请求**(时间戳晚者胜)。
  - `Conflicts` 要求停的单元,其目标状态强制合并为 `Stopped`(归入"依赖"级,但 `Conflicts` 停与被依赖方 start 冲突时按优先级裁决)。
  - 例:关机 stop 在 T1 到达、用户 start 在 T2>T1 到达 → **stop 胜**(优先级高于时间),而非按"最新请求"让 start 胜。
- 收益:避免多个来源对同一单元重复/冲突操作,保证状态收敛到一致终态,也为后续 `rctl reload`(§7.6)的差异计算打基础。

#### (12) `notify` 协议(`Type=notify` 的 READY 握手)

`Type=notify` 的启动完成判定依赖子进程主动通知,协议如下(与 systemd `sd_notify` 的最小兼容子集):

- **socket 创建**:每个 `notify` 单元启动时,rinit 创建一个 **Unix datagram socket**,路径 `/run/rinit/notify/<unit>.sock`(目录 `/run/rinit/notify/` 为 `0755`),子进程通过环境变量 `NOTIFY_SOCKET=/run/rinit/notify/<unit>.sock` 获知。
- **消息格式**:子进程向该 socket `send` 文本行,`KEY=VALUE` 形式(与 unit 文件同构):
  - `READY=1` → 服务就绪,启动完成判定通过,进入 `Active`。
  - `STATUS=<文本>` → 更新状态描述(展示在 `rctl status`),不影响状态机。
  - `WATCHDOG=1` → **延后到 P2**(依赖 §7.5 看门狗),M2 忽略。
- **超时**:`TimeoutStartSec` 内未收到 `READY=1` → 先发 `SIGTERM`,再过 `TimeoutStopSec` 仍不退则 `SIGKILL`,转 `Failed`。
- **归属**:socket 随单元生命周期创建/销毁;服务停止时 `unlink` 该 socket 路径。
- **安全性(socket 权限方案,已定稿)**:采用 **`chown` + `chmod 0660`** 方案 —— rinit `bind` 后对 socket `chown(uid, gid)`(uid/gid 来自 `User=`/`Group=` 解析结果)+ `chmod 0660`,确保**只有该服务用户可写**,防止其他服务伪造 `READY=1`。目录 `/run/rinit/notify/` 为 `0755`(root 拥有)。这是唯一能精确控制"仅服务自身可写"的方案,不采用 sticky/`0666`(有伪造风险)或目录 `0770`(精度不够)。
- **umask 竞态防护**:notify socket 目录 `0755` 无法靠目录挡权限窗口,故 **`bind` 前临时设 `umask(0o117)`**(结果 `0660`),`bind` 后恢复,消除 `chmod` 前的写入窗口。

#### (13) Job 与状态机的交互

Job 是"期望状态",状态机是"实际状态",二者之差决定动作。规则如下:

| 场景                              | 动作                                                       |
| ------------------------------- | -------------------------------------------------------- |
| start job 遇 `Active(Exited)`      | 视为**已满足**,Job 直接完成(oneshot 已成功过,无需重启)                    |
| start job 遇 `Active`             | 视为已满足,Job 直接完成                                          |
| start job 遇 `Deactivating`       | **排队等待**停止完成后再启动(不取消停止);或按策略"取消停止后重启"——M2 采用**排队等待**(简单且安全) |
| start job 遇 `Failed(StartLimitHit)` | **清除熔断计数并重试**(与 systemd `systemctl start` 一致),退避窗口清零后重新启动   |
| start job 遇 `Failed(StartFailure)` | **直接重试**(无熔断锁定,等同于首次启动)                                  |
| start job 遇 `RestartWait`         | **取消退避定时器,立即转 `Starting`**(用户手动 start 优先于自动退避,与 stop 遇 RestartWait 对称) |
| stop job 遇 `RestartWait`         | **取消退避定时器**,立即转 `Deactivating`(停止优先于重启)                |
| `Requires` 依赖失败                  | 下游 start job 标记 `Failed`,不再启动(与 `Wants` 弱依赖失败仅告警区别)         |
| `Conflicts` 要求停的单元正在 `Starting`  | 等其进入 `Active`/`Active(Exited)` 后立即停(不打断 `Starting` 中间态);若其在 `Starting` 期间进入 `Failed`/`Inactive`,则**冲突已消解,发起方正常启动** |
| 关机时的逆拓扑 stop job 与用户 `rctl stop` job | 合并到同一目标状态 `Stopped`;关机 stop 优先级高于用户 start(见 §1.3(11) 合并规则)  |

**`Failed` 的两种子状态(关键区分)**:

| Failed 子状态           | 触发条件                          | 手动 `rctl start` 行为        | `rctl reset-failed` |
| -------------------- | ----------------------------- | ------------------------ | ------------------- |
| `StartFailure`       | 启动本身失败(无 `Restart` 或 `Restart=no`) | 直接重试                      | 清失败状态(可选)           |
| `StartLimitHit`      | 熔断(`StartLimitBurst` 窗口内失败超限) | **清熔断计数后重试**              | 清熔断计数 + 失败状态        |

- 单元状态结构增加 `FailedReason` 字段(`StartFailure`/`StartLimitHit`),用于区分上述两种情况。
- `rctl reset-failed <unit>` 子命令:清空熔断计数与失败状态,作为"逃生舱",把 `StartLimitHit` 单元从锁定态解救出来(与 `rctl start` 的隐式清计数不同,`reset-failed` 不立即启动)。

**动作推导公式**:对每个单元,`target_state`(来自 Job 合并)与 `actual_state`(状态机)之差 → 若 `target=Started` 且 `actual∈{Inactive, Failed, Active(Exited), RestartWait}` 则发起启动(其中 `Failed` 再按上表区分 `StartLimitHit`/`StartFailure` 处理,`RestartWait` 按上表"取消退避立即转 Starting"处理);若 `target=Stopped` 且 `actual∈{Starting, Active, RestartWait}` 则发起停止;相等则无操作。此公式让状态机不出现隐式分支,全部显式可测。

---

## 2. 技术选型

| 项     | 选择                                                                                               | 理由                                                                    |
| ----- | ------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------- |
| 语言版本  | Rust stable,MSRV 1.99(当前最新 stable)                                                               | 稳定优先,不追 nightly;利用 1.99 已稳定的新特性(见下)                                   |
| 异步运行时 | **tokio**(`current_thread` 单线程 runtime;特性仅 `rt`/`signal`/`time`/`net`/`sync`/`io-util`/`macros`) | 团队熟悉、生态成熟;`current_thread` 收敛单线程控制内存与避免 fork 死锁;tokio 只做信号/定时/IO,不碰进程 |
| 内存分配  | 默认全局分配器;关键路径预留 `bump`/`arena`                                                                    | 平衡开发效率与内存                                                             |
| libc  | GNU glibc(**动态链接**为主,静态 fallback 见下)                                                              | 与目标系统一致,无交叉 libc 依赖;目标环境需含 glibc                                      |
| 目标    | `aarch64-unknown-linux-gnu`                                                                      | 嵌入式交叉编译;开发机 Debian 13;M0 起步静态链接,动态 glibc 为后续优化(见 §2 风险缓解)                    |
| 配置格式  | 自研 INI-like 解析器(`rinit-core` 内,不引重型 crate)                                                       | 可控、体积小                                                                |
| 日志    | 自研环形缓冲                                                                                           | 不引 tracing(体积/依赖膨胀)                                                   |
| 测试    | `cargo test` + `proptest` + `cargo-fuzz` + qemu                                                  | 见 §4                                                                  |

> **tokio 落地要点(PID 1 场景必须遵守)**:见 §2.1「tokio 使用规范」。

**动态 glibc 作为 PID 1 的风险与缓解(重要)**:

PID 1 在挂载 rootfs 之前就依赖 `/lib/ld-linux-aarch64.so.1` 和 `libc.so.6`,若 initramfs 里的动态库与构建工具链不匹配,`rdinit=/sbin/rinit` 阶段会直接 `GLIBC_2.xx not found` 或 exec 失败,系统黑屏。缓解措施:

1. **静态 fallback(优先)**:提供 `rinit-static` 构建目标(glibc 静态链接或 musl),至少保证 M0 能在 qemu 里稳定 boot。即使主用动态版,静态版作为"保险丝"存在。
2. **sysroot 构建**:动态链接必须使用**目标 rootfs 的 sysroot** 交叉编译(而非默认的交叉工具链 sysroot),确保 glibc 版本与目标一致。
3. **initramfs 完整性**:M0 的 initramfs 必须包含 `ld-linux-aarch64.so.1` 与 `libc.so.6`,且**版本与构建工具链严格一致**(见 §4.3 的 initramfs 构成)。
4. **CI 符号校验**:在 CI 中用 `objdump -T rinit` 提取所需 `GLIBC_xx` 符号版本,与目标 rootfs 的 `libc.so.6`(也用 `objdump -T` 导出)做比对,不一致即构建失败。
5. **决策**:M0 起步阶段**默认静态链接**(glibc static 或 musl)跑通 qemu,动态 glibc 作为后续优化(省内存/体积)切换项,切换时再补 sysroot + CI 校验。

**Rust 版本策略(MSRV 1.99)**:

- 采用 `rust-toolchain.toml` 锁定 `stable` channel,`Cargo.toml` 的 `rust-version = "1.99"` 声明 MSRV,CI 用 `cargo msrv --verify`(独立工具 `cargo-msrv`)或 `rustup run 1.99 cargo build` 校验。
- 1.99 已稳定的特性可直接使用,不必为旧版本保守:
  - `let ... else`、`std::sync::OnceLock`/`LazyLock`(替代 `lazy_static`,但**注意**:§0.2 原则 2 已要求避免全局可变状态,`LazyLock` 仅限**只读常量表**如能力名映射,不得用于可变运行时状态)。
  - `core::error::Error` trait(与 `thiserror` 协同,错误类型可入 `no_std`)。
  - 改进的 `unsafe` 语法与 `offset_of!` 宏(平台抽象层 HAL 有用)。
  - `impl Trait` 在关联类型中的更完整支持(简化 trait 定义)。
- 仍然**不使用 nightly-only** 特性(`#![feature(...)]`),保证可用发行版工具链交叉编译。

**`no_std + alloc` 落地方式(`rinit-core`)**:

- `rinit-core` 声明 `#![no_std]` + `extern crate alloc;`,Cargo.toml 仅依赖 `core`/`alloc`(错误类型用 `core::error::Error`,集合用 `alloc::vec::Vec`/`alloc::collections::BTreeMap` 等)。
- 但在**本仓库内由 `std` 测试/二进制通过 `extern crate rinit_core;` 调用**,即 rinit-core 本身 `no_std`,宿主仍用 `std` 链接——这样既保持下沉可能,又不必现在就写 panic handler 和 allocator。
- 二进制 `rinit` 与 `rctl` 用 `fn main()` + `std` 起步,**不强制 `#[no_main]`**;将来 profiling 证明 `std` 是瓶颈时,再给 `rinit` 加 `#[no_main]` + 全局 allocator + panic handler,而 `rinit-core` 无需改动。

### 2.1 tokio 使用规范(PID 1 场景)

tokio 可用,但 PID 1 有硬约束,以下规范必须严格遵守:

**① runtime 选择:强制 `current_thread`**

```rust
let rt = tokio::runtime::Builder::new_current_thread()
    .enable_all()          // 启用 I/O + time + signal
    .build()?;
rt.block_on(rinit_main())?;
```

- 不用 `new_multi_thread()`:多线程 runtime 会因 `fork()` 只复制当前线程而在子进程里留下"半死"的线程状态,极易死锁。
- `current_thread` 把所有任务收敛到单线程,天然规避 fork 死锁,内存占用也更贴近嵌入式预算。

**② Cargo 特性最小化**

```toml
tokio = { version = "1", features = [
    "rt", "signal", "time", "net", "sync", "io-util", "macros"
] }
```

- **不开启 `rt-multi-thread`**:生产只用 `current_thread` 单线程 runtime,`rt-multi-thread` 仅在测试代码需要时临时启用(用 dev-dependencies 独立声明)。
- **不开启 `process`**:进程创建走 `libc::posix_spawn` / 受控 fork+exec(见 §2.3),回收统一 `waitpid`,tokio 只负责信号、定时、IO 事件。
- 禁用 `full`(会拉入不需要的 `tracing`、`fs` 等)。

**③ 僵尸回收:统一 `waitpid(-1, WNOHANG)`,不依赖任何 reaper**

- 进程由 `posix_spawn`/fork+exec 创建,**不使用 `tokio::process`**,因此没有 reaper 可依赖;作为 PID 1 要回收**所有**子进程,唯一可靠做法是监听 `SIGCHLD` 后 `waitpid(-1, ..., WNOHANG)` 循环至 `ECHILD`。
- 方案:`tokio::signal::unix::signal(SignalKind::child())` 接到通知后,在**阻塞线程**(`spawn_blocking`)或手动 `waitpid` 中回收,避免在 async 上下文里做阻塞 wait。

**④ 信号处理:统一走 `tokio::signal`**

```rust
let mut sigchld = signal(SignalKind::child())?;
let mut sigterm = signal(SignalKind::terminate())?;
let mut sigint  = signal(SignalKind::interrupt())?;
```

用 `tokio::select!` 聚合信号、控制 socket、日志管道、定时器,主循环逻辑清晰且无忙等。

**⑤ 阻塞系统调用放 `spawn_blocking`**

- 挂载文件系统、`waitpid`、`sync`、关机时的 `reboot(2)` 等同步 syscall,不得在 async 线程内直接阻塞,统一 `tokio::task::spawn_blocking`。
- **关键区分**:`spawn_blocking` 会引入额外线程,而 **`fork` 绝不能在 `spawn_blocking` 内发生**(否则 fork 时进程内有多个线程 → 子进程继承半死锁状态)。因此:
  - 阻塞 syscall(`mount`/`waitpid`/`sync`)→ 可放 `spawn_blocking`(这些不 fork)。
  - `fork`(及降权的 fork+exec)→ 在 `current_thread` 主 async 线程**直接同步**执行(此刻单线程,安全);rinit **不创建常驻后台线程**,`spawn_blocking` 只在无 fork 的阻塞场景短时使用。

**⑥ 内存红线不放松**

- 引入 tokio 后,基线 RSS 会高于手写循环(约 +1~2MB),需在 M0 后实测校准 §0.1 的量化指标。
- 依赖树保持精简:`tokio` 只开必要特性,不顺手引 `tracing`、`anyhow` 全家桶;错误处理用自研 `Error` enum 或 `thiserror`。

**⑦ panic 策略:panic = abort,不写 catch_unwind**

- 生产构建 `panic = "abort"`(profile.release 与目标 profile 均设),**不写 `catch_unwind`**。
- 理由:panic 代表逻辑 bug,续命(try/catch)只会掩盖错误;PID1 的正确姿态是**让 watchdog(§7.5)兜底复位**,而非试图在已损坏的状态里继续运行。
- **前提**:此策略依赖 watchdog 自愈。若目标设备**无硬件 watchdog**,则需额外提供软件兜底:**panic 输出路径明确为 `/dev/kmsg`(内核日志,串口可见)** —— 通过 `std::panic::set_hook`(即使 `panic=abort`,hook 仍会在 abort 前运行)直接 `write(2)` 到 `/dev/kmsg`,**绕过 stderr**(PID1 的 stderr 在 initramfs 阶段可能未设置/未指向 console)。否则 panic 即静默死机,排查困难。
- **panic hook 实现要点**:① 依次尝试 `/dev/kmsg`、`/dev/console`、`/dev/ttyS0` 多个路径,首个 `open` 成功即写、即返回(极早期 `/dev` 未挂载时,靠 console/tty 兜底);② `write` 失败**静默不计**(这是诊断辅助,不是正确性路径),**不重试、不递归**(绝不能在 hook 内再 panic);③ hook 本身只做 `open`/`write`/`_exit` 这类 async-signal-safe 调用。
- 编译期配套:启用 `overflow-checks = true`(release 也开),把整数溢出从**默认的静默 wrapping**转为**可控 panic(进而 abort)**,配合单测在 debug 下捕获 panic 定位。注意:release 默认整数溢出是 wrapping 而非 UB,`overflow-checks` 的作用是让这类潜在 bug 显式化而非"防止 UB"。

---

## 2.2 范围界定:systemd 特性 vs rinit 实现范围

rinit 是 systemd 的**最小核心切片**,聚焦「进程生命周期 + 服务编排 + 守护」这一条主线。

### 逐项对照表

| 类别              | systemd 特性                  | rinit 计划 | 说明                               |
| --------------- | --------------------------- | -------- | -------------------------------- |
| PID 1           | 完整 PID1 + 引导目标              | ✅ 实现     | 核心功能,嵌入式必需                       |
| 服务单元解析          | 完整 unit 格式 + 模板 + drop-in   | ✅ 简化版    | 自研 INI-like,不兼容,无模板/drop-in      |
| 依赖解析            | 事务引擎 + 10 种依赖关系             | ✅ 子集     | 只做 `Requires/Wants/Conflicts/After/Before`(`After/Before` 仅排序,`Conflicts` 互斥) |
| 并行启动            | socket/事务并行                 | ✅ 实现     | DAG + 就绪信号                       |
| 服务守护            | 8 种重启策略 + 看门狗               | ✅ 子集     | `on-failure` 型 + 熔断;看门狗可选        |
| 健康检测            | sd_notify + watchdog        | ✅ 实现     | 命令探测 + notify READY              |
| 生命周期管理          | 完整 + reload                 | ✅ 实现     | start/stop/restart/status        |
| 日志收集            | journald 二进制日志              | ✅ 简化版    | 环形缓冲文本日志,无压缩/校验                  |
| 僵尸回收            | subreaper + waitpid         | ✅ 实现     | 一致                               |
| 关机/重启           | 事务性逆序停止                     | ✅ 实现     | 逆拓扑 + 超时 SIGKILL                 |
| CLI 工具          | systemctl(数百子命令)            | ✅ 子集     | rctl 几个核心子命令                     |
| target 抽象       | 完整 target                   | ✅ 子集     | 汇聚点语义,核心项(M1,见 §7.2)             |
| socket 激活       | 完整                          | ⚠️ 可选    | 列为 P2 进阶                         |
| cgroup v2       | 全量资源控制                      | ⚠️ 子集    | CPU/内存基础限制(M5)                   |
| 权限降级/沙箱         | 命名空间/seccomp/LSM            | ⚠️ 子集    | `User=`/`NoNewPrivileges=` 基础项   |
| 设备管理(udev)      | 完整                          | ❌ 不做     | 依赖外部工具或 devtmpfs                 |
| 网络/DNS/时间       | networkd/resolved/timesyncd | ❌ 不做     | 用 busybox/专用工具替代                 |
| 登录会话(logind)    | 完整                          | ❌ 不做     | 嵌入式无多用户会话需求                      |
| D-Bus 集成        | 系统总线                        | ❌ 不做     | 用轻量 Unix socket 协议               |
| 模板/%i 实例化       | 完整                          | ❌ 不做     | 嵌入式服务数量少                         |
| 定时器/路径/mount 单元 | 完整                          | ❌ 不做     | 用 cron 或脚本替代                     |
| 容器/nspawn/便携服务  | 完整                          | ❌ 不做     | 超出嵌入式范围                          |

### 范围界定结论

**实现**(✅):覆盖 systemd 的「服务管理」主干 —— 这是嵌入式场景使用频率最高、也最不可替代的部分,对应你原计划的 8 项核心功能。

**可选扩展**(⚠️):在核心功能稳定后按需追加,优先级见 §7 建议清单。

**明确不做**(❌):四类被砍掉的特性,在资源受限设备上要么用不上、要么能用更轻量工具替代:

1. 大而全的工具生态(systemctl 数百子命令、分析工具、各类 *ctl)
2. 周边子系统(网络、DNS、时间、设备、登录会话)—— 由 busybox 或专用组件负责
3. 重型安全沙箱(命名空间全套、seccomp、LSM)—— 只保留降权等基础项
4. 高级调度能力(socket 激活、事务引擎、模板、定时器单元)

> **图例**:✅ 实现 / ⚠️ 可选或子集 / ❌ 不做

---

## 2.3 libc 薄封装规范(不用 nix)

**决策**:系统调用**直接使用 `libc` crate,不引入 `nix`**。理由:

- `nix` 本身依赖 `libc`,直接用 libc 少一层依赖,交叉编译更稳、体积更小,契合嵌入式约束。
- PID 1 实际需要的 syscall 仅十几个,自封装成本极低且完全可控。
- `nix` 的抽象在"精确控制 fork 后子进程每一步"场景反而需要理解其内部行为,多一层间接。

**补偿措施**:在 `rinit-runtime` 内建 **`syscall` 薄封装子模块**,弥补 `nix` 缺席的类型安全与错误检查:

```
rinit-runtime/src/syscall/
├── mod.rs          # 统一 re-export
├── process.rs      # posix_spawn / fork+exec / waitpid / kill 的安全封装
├── mount.rs        # mount(2) / umount2 封装
├── signal.rs       # signal / sigprocmask / prctl(PR_SET_*)
├── cred.rs         # setuid / setgid / initgroups / capset
└── reboot.rs       # reboot(2) / sync
```

**进程创建:posix_spawn vs fork+exec 的取舍**:

| 场景                                                           | 用哪种                 | 理由                                                                                            |
| ------------------------------------------------------------ | ------------------- | --------------------------------------------------------------------------------------------- |
| **无降权/无特殊 fd 需求**的普通服务启动                                     | `libc::posix_spawn` | 内核/glibc 优化路径,无 fork 复制开销,天然规避多线程 fork 死锁,代码更简                                                |
| **需要降权**(`User=`/`Group=`)、`NoNewPrivileges`、capability 设置   | **受控 fork+exec**    | `posix_spawn` 无法在 exec 前执行 `setuid`/`prctl`/`capset` 等子进程初始化,必须 fork 后、exec 前手动完成             |
| **需要重定向 stdin/out/err 到管道、`WorkingDirectory`、`Environment`** | 两者皆可                | 重定向用 `posix_spawn_file_actions_t`;`WorkingDirectory` 用 `posix_spawn_file_actions_addchdir_np`(glibc 扩展);**环境变量通过 `posix_spawn` 的 `envp` 参数传递**(不是 `posix_spawnattr_t`);fork+exec 在子进程内 dup2/chdir/setenv |
| **需要新进程组(`setpgid`,见 §1.3(6))**                             | 两者皆可                | `posix_spawn` 用 `posix_spawnattr_setflags(&attr, POSIX_SPAWN_SETPGROUP)` + `posix_spawnattr_setpgroup(&attr, 0)`;fork+exec 在子进程内 `setpgid(0,0)` |

**受控 fork+exec 的约束**(必须遵守):

- **fork 安全的前提是单线程**:`fork` 只复制调用线程,若进程内有其他线程存活(如 `spawn_blocking` 线程池、后台线程),fork 后子进程会拿到"半死"的锁与分配器状态,极易死锁。因此**fork 绝不发生在 `spawn_blocking` 内部**(那是另一个线程),而是发生在 `current_thread` 主 async 线程**直接同步调用**的受控代码路径中;`current_thread` 保证此刻进程只有一个线程,rinit 也**不创建任何常驻后台线程**(§2.1⑤ 的 `spawn_blocking` 仅用于无 fork 场景的阻塞 syscall,如 `mount`/`waitpid`)。
- fork 后子进程**只调用 async-signal-safe 的 libc 函数**(`dup2`/`chdir`/`setuid`/`setgid`/`setpgid`/`prctl`/`execve` 等),绝不触碰分配器或锁。
- fork 返回后立即 `exec`,失败用 `_exit(127)` 退出子进程(不能 `panic`/`exit`,避免触发父进程复制的 atexit handler)。
- **统一回收**:无论哪种创建方式,回收一律走 §1.3(6) 的 `waitpid(-1, WNOHANG)` 循环,不依赖 `posix_spawn` 或 `tokio::process` 内部的 reaper。

**封装层规则**:

- **`unsafe` 全部集中在此层**,业务代码(`manager`/`unit` 等)不出现裸 `unsafe`。
- 每个封装函数返回 `Result<T, SyscallError>`,内部检查返回值并带上 `errno`(`std::io::Error::last_os_error()`),绝不让错误静默丢失。
- C 字符串用 `std::ffi::CString` 构造,`mount` 等需要 `*const c_char` 的参数在封装内部安全转换。
- 该层是唯一允许直接触碰 `libc` 的模块,便于逐函数审计(这是 PID1 代码最该有的边界)。

**依赖清单**(锁定最小化):

```toml
# rinit-runtime
libc = "0.2"      # 唯一 syscall 底层,不引 nix
```

**错误层次(跨 crate 的错误转换)**:

```
rinit-core::CoreError        (core::error::Error,no_std 可用,纯逻辑错误)
        │  From / 手动映射
        ▼
rinit-runtime::SyscallError  (封装层:errno → 结构化错误)
        │
        ├── rinit-runtime::RuntimeError   (运行时错误,含 SyscallError + IO + 状态机违规)
        ▼
rinit / rctl 顶层 Error enum  (聚合 CoreError + RuntimeError,统一日志/退出码)
```

- `rinit-core` 只定义 `CoreError`(解析/依赖/状态机逻辑错误),不感知 syscall。
- `rinit-runtime` 的 `SyscallError` 通过 `From<CoreError>` 与 `From<io::Error>` 收拢,`RuntimeError` 再包裹。
- 顶层二进制用一个 `Error` enum 聚合,实现 `Display`/`Error`,统一决定日志与退出码。

---

## 3. 分阶段开发计划与里程碑

### 里程碑总览

| 里程碑    | 内容                  | 交付物                    | 可验证标准                                |
| ------ | ------------------- | ---------------------- | ------------------------------------ |
| **M0** | 项目脚手架 + 最小 PID 1    | 能 boot、能回收僵尸、能拉起硬编码服务  | qemu 内 `rinit` 作为 PID1 启动,`ps` 看到子进程 |
| **M1** | 单元配置解析 + 依赖调度       | 解析器 + DAG + 并行启动       | 单测 + 多服务并行启动演示                       |
| **M2** | 生命周期 + 守护(重启/健康/退避) | 完整状态机 + 重启策略           | 故障注入测试服务自动重启                         |
| **M3** | 控制平面 rctl + 日志      | CLI + socket 协议 + 日志收集 | `rctl status/start/stop` 可交互         |
| **M4** | 关机重启 + 强化           | 优雅关机 + 边界错误处理          | 长稳 soak + 泄漏检测                       |
| **M5** | 生产打磨                | cgroup、资源限制、文档、打包      | 通过验收清单                               |

### 阶段详细拆解

#### M0 —— 脚手架与最小 PID 1(约 1 周)

- [ ] workspace 结构 + `fn main()` 入口 + `current_thread` runtime。
- [ ] 搭建 `current_thread` runtime + `tokio::signal` 处理 `SIGCHLD`,实现 `waitpid` 循环回收僵尸。
- [ ] 第一波硬编码挂载:`/run`(tmpfs)→`/proc`→`/sys`→`/dev`(devtmpfs)→`/tmp`,顺序固定,见 §1.3(9)。
- [ ] 硬编码启动一个测试服务(先不读配置文件)。
- [ ] 验证:qemu 内 `rinit` 作为 `rdinit=/sbin/rinit` 启动,能 fork/exec 子进程并回收。

#### M1 —— 配置解析与依赖并行启动(约 2 周)

- [ ] 完成简化 unit 文件解析器 + 完整错误类型。
- [ ] 依赖 DAG:拓扑排序 + 环检测 + 错误报告。
- [ ] 单元管理器调度:依赖满足即并行启动。
- [ ] `Requires/Wants/After/Before` 语义落地。
- [ ] 验证:10+ 服务依赖图并行启动,启动耗时明显低于串行。

#### M2 —— 生命周期与守护(约 2 周)

- [ ] 完整状态机 + 迁移表。
- [ ] `Type`(simple/oneshot/notify)支持(`forking` 移入 §7.10 P2)。
- [ ] 重启策略:指数退避 + StartLimit 熔断。
- [ ] 健康检测(命令 + notify `READY=1` + `STATUS=`)。
- [ ] Job 队列与状态机交互(§1.3(13))落地。
- [ ] 验证:kill 掉服务进程,观察自动重启与熔断。

#### M3 —— rctl 控制平面与日志(约 2 周)

- [ ] 本地 Unix socket 控制协议(JSON 或自定义二进制,倾向二进制以省内存);**协议含版本号字段**,`rctl` 连接后先校验版本,不匹配则拒绝(避免新旧二进制混用)。
- [ ] **控制 socket 安全**:`/run/rinit/control.sock` 权限设为 `0600`(仅 root);每个连接用 `SO_PEERCRED` 获取对端 uid/gid/pid,非 root 连接一律拒绝(写入前校验)。见 §1.3(10)。
- [ ] `rctl` CLI:`list/status/start/stop/restart/reset-failed/log`。**退出码约定**:`0` 成功;`1` 通用错误;`2` 用法错误(参数错);`3` 单元不存在;`4` 操作冲突(如对 `Active(Exited)` 执行 `restart` 无意义)。
- [ ] 日志环形缓冲 + stdout/stderr 捕获 + 持久化。
- [ ] 验证:命令行可完整控制服务并查看日志。

#### M4 —— 关机重启与强化(约 2 周)

- [ ] 优雅关机/重启编排(逆拓扑停止 + 超时 SIGKILL + reboot 系统调用)。
- [ ] 边界错误处理审计:所有系统调用返回值检查、panic-free 生产路径。
- [ ] 长稳 soak 测试 + 内存泄漏检测(见 §4)。
- [ ] 验证:72h 长稳无泄漏、无僵尸堆积、无 fd 泄漏。

#### M5 —— 生产打磨(约 1-2 周)

- [ ] cgroup v2 集成(CPU/内存限制、进程隔离)。
- [ ] socket 激活(可选,进阶)。
- [ ] 交叉编译脚本 + 打包(initramfs/busybox 集成)。
- [ ] 文档、示例 unit、验收清单。

> **总工期估算:约 10-12 周**(单开发者节奏;团队可并行压缩)。

---

## 4. 测试策略

### 4.1 单元测试(每个 commit 必跑)

- **解析器**:合法/非法/边界输入全覆盖;`proptest` 属性测试随机输入不 panic。
- **DAG**:环检测、拓扑序正确性、空图、自依赖。
- **状态机**:所有合法/非法迁移穷举断言。
- **重启策略**:退避序列、熔断阈值边界。

### 4.2 集成测试

- `rinit-core` 与 `rinit-runtime` 在 host(x86)上,用 **fake 运行时**(注入 mock 的 fork/exec/socket)做端到端逻辑测试,无需真机。
- 进程相关逻辑用真实 `posix_spawn`/fork+exec 在 host 上测(不依赖 aarch64)。

### 4.3 嵌入式环境验证(qemu)

**环境规格**(按你的要求):

- 目标架构:`aarch64`
- 机器:`virt`(QEMU `-machine virt`)
- CPU:`cortex-a57`(virt 默认支持的 64 位核,与主流嵌入式 SoC 性能相当)
- 内存:`-m 128M` 模拟 128MB 约束
- **内核:Linux LTS**(当前推荐 6.6 LTS,长期维护至 2026 年后;如需更保守可用 6.1 LTS),裁剪 config 仅保留 aarch64/virt 必需项,进一步贴近真实嵌入式内核体积
- **用户态工具:busybox(stable)**—— 提供 shell、`getty`、`login`、`mount`、`init` 的辅助命令等,作为 rinit 之外的全部用户态环境

**根文件系统(initramfs 构成)**:

```
initramfs/
├── sbin/rinit                 # 本系统主程序(M0 阶段静态链接,见下)
├── lib/                       # 动态 glibc 库(仅切换动态链接后需要,版本须与工具链一致)
│   ├── ld-linux-aarch64.so.1
│   └── libc.so.6
├── bin/                       # busybox 多调用二进制(symlink 集合)
│   ├── busybox
│   ├── sh -> busybox
│   ├── getty -> busybox
│   ├── login -> busybox
│   ├── mount -> busybox
│   ├── ps -> busybox
│   └── ...
├── etc/
│   ├── rinit/units/           # 服务单元文件
│   ├── passwd                 # getty/login 所需
│   └── inittab                # 若用 busybox init 作对照测试(可选)
└── dev/                       # devtmpfs 挂载点(运行时填充)
```

> **`init` 符号链接说明**:使用 `rdinit=/sbin/rinit`(见 qemu 启动脚本)时,内核直接 exec `/sbin/rinit`,**无需** `init -> /sbin/rinit` 符号链接(该链接仅在走默认 `rdinit=/init` 路径时需要,此处冗余,可删)。
>
> **动态链接的版本一致性(重要)**:若切换动态 glibc,`lib/` 下的 `ld-linux-aarch64.so.1` 与 `libc.so.6` 必须来自**与构建工具链相同的 sysroot**,否则 `rdinit` 阶段 `GLIBC_xx not found` 黑屏;CI 用 `objdump -T` 校验(见 §2 风险缓解)。M0 起步**默认静态链接**(glibc static 或 musl),规避此风险。

**busybox 配置要点(stable 分支)**:

- 从 busybox `stable` 分支拉取(如 `1_36_stable`),固定版本锁定,避免 `master` 引入不稳定。
- 开启:`CONFIG_ASH`(shell)、`CONFIG_GETTY`、`CONFIG_LOGIN`、`CONFIG_MOUNT`、`CONFIG_PS`、`CONFIG_INIT`(仅作对照)、`CONFIG_DEVMEM` 等嵌入式必需项。
- 关闭:`CONFIG_UDHCPC` 等网络组件(由外部工具负责)、桌面相关无用的 applet,控制体积。
- 静态链接 busybox 到目标(用 `aarch64-linux-gnu-gcc -static` 或 musl,独立于 rinit 的 glibc 动态链接),这样 busybox 本身无动态依赖、initramfs 更简单。

**内核与 busybox 的获取/构建(建议脚本化)**:

```bash
# 内核(LTS)
git clone --depth 1 --branch v6.6 https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux.git
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig
# 再裁剪 config 或直接用 virt 最小配置
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j$(nproc) Image

# busybox(stable)
git clone --depth 1 --branch 1_36_stable https://git.busybox.net/busybox
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- defconfig
make menuconfig   # 按上述要点增删 applet
make ARCH=arm64 CROSS_COMPILE=aarch64-linux-gnu- -j$(nproc)
```

**开发机环境**:Debian 13(trixie)

**交叉编译链**(Debian 13 直接用 apt 安装,无需 zig cc):

```bash
# 安装 aarch64 交叉工具链 + qemu + 内核构建工具
sudo apt install gcc-aarch64-linux-gnu qemu-system-arm qemu-user-static bc flex bison libssl-dev

# 添加 Rust 目标
rustup target add aarch64-unknown-linux-gnu

# 构建 rinit
cargo build --release --target aarch64-unknown-linux-gnu
```

`.cargo/config.toml` 示例:

```toml
[target.aarch64-unknown-linux-gnu]
linker = "aarch64-linux-gnu-gcc"
```

**qemu 启动脚本(示例)**:

```bash
qemu-system-aarch64 \
  -machine virt \
  -cpu cortex-a57 \
  -m 128M \
  -kernel Image \
  -initrd initramfs.cpio.gz \
  -append "console=ttyAMA0 rdinit=/sbin/rinit" \
  -nographic
```

**引导后提供的交互终端(getty)示例单元**:

```ini
# /etc/rinit/units/getty.service —— 用 busybox getty/login 提供串口登录
[Unit]
Description=Serial console getty
After=rinit-init.service

[Service]
Type=simple
ExecStart=/bin/getty -L ttyAMA0 115200 vt100
Restart=always
```

**验证项**:

1. 启动速度:内核启动到 `rinit` 拉首个服务的时间戳测量。
2. 内存占用:`/proc/<pid>/status` 的 VmRSS 在 128M 内留有充足余量。
3. 故障注入:kill 服务进程、触发 OOM、断电模拟(SIGPWR)。
4. 长稳:qemu 内跑 soak 脚本 ≥ 24h。
5. 交互登录:通过 getty 串口登录,验证 busybox shell/login 与 rinit 协同正常。

### 4.4 专项测试

- **泄漏检测**:`miri` 仅检查 `rinit-core` 的**纯逻辑**(解析器/依赖 DAG/状态机迁移,无 syscall);`rinit-runtime` 因涉及 syscall 不跑 miri;运行时用 `valgrind`(host 版)测;长期 RSS 曲线监控。注意 `rinit-core` 是 `no_std + alloc`,`miri` 测试需**独立 harness**(`extern crate alloc;` + 自定义 allocator,不依赖 `std` 测试运行时),并配 `MIRIFLAGS`。
- **fuzz**:`cargo-fuzz` 对解析器和 socket 协议解码器做 fuzz。
- **并发/收敛验证**:`loom` 只适用于多线程共享状态模型,而 rinit 是 `current_thread` 单线程,故**不用 loom**;改用 `proptest`/`quickcheck` 对 **Job 队列与状态机**做"任意 Job 序列 → 状态收敛到一致终态"的属性测试。
- **性能基准**:`criterion` 测解析吞吐、DAG 调度延迟。

---

## 5. 潜在技术风险与应对

| #  | 风险                                     | 影响 | 应对方案                                                                                                        |
| -- | -------------------------------------- | -- | ----------------------------------------------------------------------------------------------------------- |
| 1  | **PID 1 崩溃即系统崩溃**,且内核会忽略 PID1 收到的未处理信号 | 致命 | 生产路径零 `panic`;`panic=abort` 依赖 watchdog 复位兜底(无 watchdog 设备则输出 panic 信息到串口);关键信号全部显式处理;不处理 `SIGKILL`(内核强制保护) |
| 2  | **僵尸进程回收不彻底**导致 PID 耗尽                 | 高  | 唯一 `waitpid(-1,WNOHANG)` 循环 + `tokio::signal` 捕获 `SIGCHLD`(PID1 天然是孤儿父进程,无需 `PR_SET_CHILD_SUBREAPER`);soak 测试断言无 `<defunct>` |
| 3  | **依赖环 / 依赖缺失**导致启动死锁                   | 高  | DAG 构建期检测环并显式报错;缺失依赖 `Wants` 降级、`Requires` 失败可见                                                             |
| 4  | **内存占用失控**(std/分配器/日志膨胀)               | 高  | 环形缓冲限流;`no_std` 局部下沉;RSS 曲线纳入 CI 门禁;关键结构用 arena                                                             |
| 5  | **fork 后死锁 / 多线程问题**(fork 只复制当前线程)     | 中  | `current_thread` 单线程 runtime + 不建常驻后台线程;fork 在主 async 线程直接同步执行,绝不进 `spawn_blocking`;fork 后子进程只走 async-signal-safe 调用 |
| 6  | **交叉编译 glibc/aarch64 工具链问题**           | 中  | 用发行版 `aarch64-linux-gnu-gcc` 交叉工具链;CI 矩阵覆盖;锁定工具链版本                                                    |
| 6b | **动态 glibc 版本不匹配**导致 PID1 启动即黑屏         | 高  | M0 起步默认静态链接;动态版用目标 rootfs sysroot 构建;CI 用 `objdump -T` 校验 `GLIBC_xx` 符号与目标 `libc.so.6` 一致;initramfs 必含匹配的 `ld-linux`+`libc.so.6`(见 §2 风险缓解) |
| 7  | **启动竞态**(依赖"就绪"判定不准)                   | 中  | `Type` 精确建模(`forking` 已砍,移 P2)+ `notify` READY 协议;`TimeoutStartSec` 兜底;可加 `ExecStartPre` 探活                                |
| 8  | **关机 hang**(某服务不响应停止)                  | 中  | `TimeoutStopSec` 强制 SIGKILL + 整体关机超时看门狗 + 逆拓扑顺序停止                                                           |
| 9  | **qemu 与真实硬件差异**(设备树、驱动、时序)            | 中  | qemu 用于逻辑验证,关键指标以真实板卡为准;保留硬件 bring-up 阶段                                                                    |
| 10 | **生态/兼容预期落差**(用户期望完整 systemd 特性)       | 低  | 明确 scope:简化格式 + 核心子集;文档写清"不兼容 systemd";提供迁移说明                                                               |
| 11 | **传统 daemon 无法前台化**(用户依赖 double-fork)    | 低  | `forking` 移到 §7.10 P2(靠 `PIDFile=`+cgroup);过渡期引导用户把服务改为前台运行(`simple`/`notify`)                                 |

---

## 6. 验收清单(Definition of Done)

- [ ] 作为 PID 1 在 aarch64 qemu(128M)稳定 boot,无内核 panic。
- [ ] 20 个服务依赖并行启动,耗时与串行比有明显提升。
- [ ] 服务崩溃自动重启、熔断生效、健康检测可用。
- [ ] `rctl` 可完整启停/查询/看日志。
- [ ] 优雅关机/重启无残留僵尸、无 fd 泄漏。
- [ ] 72h soak 测试 RSS 稳定、无泄漏、无 PID 耗尽。
- [ ] 二进制 ≤ 2MB(动态 glibc)或 ≤ 4MB(静态,见 §2 风险缓解)。
- [ ] 单元测试覆盖率 ≥ 80%(核心模块 ≥ 90%)。
- [ ] 文档齐全:架构、单元格式、rctl 用法、移植指南。

---

## 7. 扩展项详细设计(⚠️ 项落地方案)

你列的核心功能已完整,以下是建议新增的 ⚠️ 项(见 §2.2 对照表)的**具体设计**,按优先级排列。每项给出:设计目标、配置格式、核心流程、与核心引擎的交互点、边界处理。

### 7.1 P0 —— 信号安全与 PID1 语义(独立模块 `signal`)

**设计目标**:明确 PID1 对信号的正确处理方式,这是正确性根基。

**内核关键规则**(必须内化):

- PID1 未显式安装 handler 的信号会被内核**静默忽略**(`SIG_DFL` 对 PID1 无效)。
- `SIGKILL`/`SIGSTOP` 对 PID1 永远不可拦截。

**信号处理矩阵**:

| 信号                 | 处理        | 说明                                 |
| ------------------ | --------- | ---------------------------------- |
| `SIGCHLD`          | 回收僵尸      | 触发 `waitpid(-1, WNOHANG)` 循环       |
| `SIGTERM`/`SIGINT` | 优雅关机      | 进入 shutdown 编排                     |
| `SIGPWR`           | 低电/掉电     | 快速关机:`TimeoutStopSec` 上限压到 **2s**,总关机预算 **≤ 5s**(掉电场景时间极短,优先保数据而非优雅停止) |
| `SIGUSR1`          | 预留(可选热加载) | 后续绑定 `reload`                      |
| `SIGHUP`           | 重读配置(可选)  | 或作为 reload 触发                      |
| `SIGWINCH`/其他      | 显式忽略      | 用 `signal(Sig, SIG_IGN)` 显式声明,避免歧义 |

**实现要点**:

```rust
// 统一走 tokio::signal,聚合到 select! 主循环
let mut sigchld = signal(SignalKind::child())?;
let mut sigterm = signal(SignalKind::terminate())?;
// 其余不需要的用 signal(SignalKind::from_raw(x)) + 显式 ignore
```

### 7.2 P0 —— target 抽象(汇聚点语义)

**设计目标**:提供 `network.target`/`multi-user.target` 这类"无进程的汇聚单元",作为依赖编排的锚点,避免服务直接互依赖导致的图脆弱。

**单元类型扩展**:通过 `[Unit]` 段的 `Type=target`(见 §1.1 契约表)或 `.target` 后缀识别,target 单元**无 ExecStart、不产生进程**,仅作为 DAG 中的汇聚节点。

**配置示例**:

```ini
[Unit]
Description=Network ready
Type=target
After=network.service dhcpcd.service
```

```ini
[Service]
ExecStart=/usr/bin/app-server
After=network.target          # 只需等"网络就绪"这个抽象点
# 而非直接依赖 network.service 的具体实现
```

**核心流程**:

1. 解析器识别 target 单元,`UnitConfig.kind = Target`。
2. DAG 中 target 作为普通节点参与拓扑排序。
3. 调度时,target 的"就绪"判定 = 其 `After=` 依赖**全部达到终态**(`Active`/`Active(Exited)`/`Failed`),且**无 `Failed`**(即 `Active` 或 `Active(Exited)`);无需进程。
4. target 一旦 active 立即释放下游,无需守护逻辑。
5. **target 可依赖 target**:target 作为 DAG 普通节点,可被其他 target 依赖(`After=` 指向 target),参与拓扑排序;就绪判定**递归到叶子节点**(即所有间接依赖的 service/target 均达终态且无 Failed)。
6. **target 上的 `Requires=` 与 service 一致**:`network.target` 声明 `Requires=dhcpcd.service` 时,若 `dhcpcd` 失败,则 `network.target` 也判 `Failed`(强依赖语义不因 target 而改变)。即 target 既关心 `After=` 排序(就绪判定),也关心 `Requires=` 激活(强依赖)。

**边界处理**:

- target 无 `After=` 依赖时,视为**立即就绪**(等价 `implicit target`)。
- 循环依赖检测仍生效,报错信息中 target 与 service 一视同仁。
- 默认引导目标:约定 `multi-user.target` 为 `default.target`,M0 阶段用硬编码,M1 支持 `rinit` 命令行参数 `--default` 指定。

**收益**:服务单元与底层实现解耦(网络从 dhcpcd 换成 systemd-networkd 时,业务服务单元无需改)。

### 7.3 P1 —— cgroup v2 集成(资源限制与进程分组)

**设计目标**:为每个服务建立独立 cgroup,提供 CPU/内存硬限制,防止单服务拖垮整机(128M 内存约束下的关键保障)。

**配置格式**(`[Service]` 段扩展):

```ini
[Service]
ExecStart=/usr/bin/worker
CgroupCPUMax=50%              # CPU 上限(百分比或 quota/period)
CgroupMemoryMax=32M           # 内存硬上限
CgroupMemoryHigh=24M          # 软上限(先回收 cache)
CgroupTasksMax=64             # 进程/线程数上限
```

**核心流程**:

1. **启动时**:在 `/sys/fs/cgroup` 下建 `rinit/<unit>` 目录(`rinit.slice` 是 systemd 术语,改用 `rinit/` 前缀更贴切),把 `ExecStart` fork 出的 PID 写入 `cgroup.procs`。
2. **资源写入**:将配置值写入 `cpu.max`、`memory.max`、`memory.high`、`pids.max`。
3. **进程归属**:子进程 fork 后自动继承父进程 cgroup(内核保证),无需额外跟踪孙进程。
4. **清理**:服务停止时,待 cgroup 内进程退出后 `rmdir` 该目录。**退出确认流程**:停止时向 `cgroup.procs` 中所有 PID 发 `SIGTERM` → 等 `TimeoutStopSec` → 超时发 `SIGKILL` → 通过 `waitpid` + 读取 `cgroup.events` 的 `populated=0` 确认所有进程已死,再 `rmdir`。

**CPU 配额转换(`CgroupCPUMax`)**:`50%` → `cpu.max = "50000 100000"`(quota=50000µs,period=100000µs);`200000 100000` 形式则直接写入。转换在解析期完成,存入 `CpuQuota` enum。

**边界处理**:

- cgroup v2 未挂载(`/sys/fs/cgroup/cgroup.controllers` 不存在)时,**降级为无限制运行 + 日志警告**,不阻断启动。
- 必须启用 `cgroup` 内核模块且 init 挂载为 v2 统一层级,否则 M5 需检查内核 config。
- `CgroupMemoryMax` 触发 OOM 时,内核 OOM-killer 在 cgroup 内挑选进程杀,不影响整机。
- 与健康检测联动:检测到 OOM kill 后进入重启策略。
- **降权与 cgroup 写入顺序**:若单元同时有 `User=` 降权和 cgroup 限制,**先 fork**,父进程(仍是 root)把子进程 PID 写入 `cgroup.procs`,子进程再降权(而非子进程自己写,避免降权后无写权限);或由父进程在 `setuid` 前完成写入。
- **`rmdir` 失败处理**:若 cgroup 内仍有僵尸或 D 状态进程,`rmdir` 会 `EBUSY`;此时**保留目录并告警,不阻塞停止流程**(下次启动复用或清理),极端情况(D 状态进程卡死)允许目录残留。

### 7.4 P1 —— 进程沙箱与权限降级

**设计目标**:以最小特权运行服务,提升嵌入式设备安全性,避免一个被攻破的服务拿到 root。

**配置格式**(`[Service]` 段扩展,采纳 systemd 命名但仅基础子集):

```ini
[Service]
ExecStart=/usr/bin/api-server
User=nobody                    # 降权运行
Group=nogroup
NoNewPrivileges=true           # 禁止子进程提权(setuid 失效)
CapabilityBoundingSet=          # 清空能力集(最严格)
AmbientCapabilities=NET_BIND_SERVICE   # 仅授予必要能力
```

**核心流程**:

1. fork 后、exec 前,在子进程内按序执行降权(顺序敏感,见边界):
   - `prctl(PR_SET_NO_NEW_PRIVS, 1)`
   - `capset()` 设置 bounding/ambient 能力集
   - `setgid()` → `initgroups()` → `setuid()`
2. exec 目标程序。

**边界处理(顺序强约束)**:

- **降权必须在 `setuid` 之前**完成能力集设置,否则 `setuid` 后可能失去 `CAP_SETPCAP` 无法再调整。
- `setuid` 必须在 `setgid` 之后,且 `initgroups` 补全附加组。
- 降权失败(如用户不存在)必须 **fail-closed**:不 exec,报错进 Failed 状态。
- `NoNewPrivileges=true` 开启后,目标程序中的 setuid 二进制将无法提权;**需要 setuid 提权的服务不能开启此项**(这是安全增强,不是透明开关)。
- **`User=`/`Group=` 的 uid/gid 解析**:自研最小 `/etc/passwd`、`/etc/group` 解析(`name:passwd:uid:gid:...` 冒号分隔,取 uid/gid 字段),**不依赖 NSS**(`getpwnam`/`getgrnam` 会引入 glibc NSS 动态加载,嵌入式不可控);纯数字则直接当 uid/gid。解析失败(用户不存在)→ fail-closed。
- **cap 名称映射(位置已定)**:提供只读常量表(如 `NET_BIND_SERVICE` → `10`、`SYS_ADMIN` → `21`),**放在 `rinit-runtime::syscall::cred`**(该层有 `std`),用 `LazyLock<HashMap<&str, u32>>`(符合 §0.2 原则 2 的"只读常量表"豁免);**不放 `rinit-core`**(no_std 无 `std::LazyLock`/`HashMap`)。供 `capset` 使用。
- 用 `rinit-runtime::syscall` 薄封装(直接 `libc::prctl`/`libc::capset`/`libc::setgid` 等),`unsafe` 集中在封装层,对外暴露安全 `Result`,见 §2.3。

> **`WorkingDirectory` 不存在时 fail-closed**:`chdir` 前先校验目录存在且可访问,失败则不 exec、转 `Failed`(而非忽略继续,避免服务在错误目录下运行)。

**命令执行用户矩阵(Exec*/HealthCheck 与 `User=`/`Group=` 的关系)**:

> 与 systemd 一致:所有 `Exec*` 与 `HealthCheck` 命令默认以 `User=`/`Group=` 指定的用户运行,避免特权操作被意外允许。

| 命令              | 默认运行用户        | 说明                                                                 |
| --------------- | ------------- | ------------------------------------------------------------------ |
| `ExecStart`     | `User=`/`Group=` | 主命令,按 §7.4 流程降权                                                     |
| `ExecStartPre`  | `User=`/`Group=` | 与 systemd 一致,以服务用户运行;若需 root 权限的前置操作,**不支持** `+` 前缀,请改用 `rinit-init.service` 做系统级准备 |
| `ExecStartPost` | `User=`/`Group=` | 同上                                                                 |
| `ExecStop`      | `User=`/`Group=` | 同上                                                                 |
| `HealthCheck`   | `User=`/`Group=` | 以服务用户运行(探测服务自身状态);不建议以 root 跑(避免探测命令本身成为提权面)。若确需 root 权限(如读 `/proc/<pid>/status` 受限字段),编写 setuid 辅助脚本,或改用 `/proc` 对同 uid 可读的字段 |

> **显式决策**:rinit **不支持** systemd 的 `PermissionsStartOnly=` 和 `+`/`-`/`!` 命令前缀;需要 root 权限的准备工作一律放 `rinit-init.service`(系统级 oneshot 单元,见 §1.3(9))或由服务自身在降权前处理。这是简化决策,文档明确记录,避免用户误用 `ExecStartPre` 做 root 操作。

### 7.5 P2 —— 硬件看门狗(`/dev/watchdog`)

**设计目标**:利用硬件 watchdog 实现整机级自愈——rinit 主循环卡死时自动复位,而非静默挂死。

**配置**:编译期或 `/etc/rinit.conf` 全局开关,非单元级:

```ini
[Global]
WatchdogDevice=/dev/watchdog
WatchdogTimeoutSec=30
```

**核心流程**:

1. 启动时打开 `/dev/watchdog`(打开即激活,内核开始计时)。
2. 主循环每次迭代(或独立 `tokio::time::interval`)执行 `write(fd, "0")` 喂狗。
3. 喂狗间隔取 `WatchdogTimeoutSec/2`,留足余量。

**边界处理**:

- 喂狗必须发生在主循环的**健康检查点**(而非独立线程无脑写),这样主循环卡死 → 停止喂狗 → 硬件复位,才算真正自愈。
- 优雅关机时,写 `"V"` 做 magic close,告知硬件"这是主动关机,别复位"。
- 设备不存在时静默禁用(嵌入式可能无 watchdog 硬件)。

### 7.6 P2 —— 配置热加载(`rctl reload`)

**设计目标**:不重启整机即可重读单元文件,减少变更成本。

**核心流程**:

1. `rctl reload` 通过控制 socket 发送 `Reload` 命令。
2. rinit 收到后:重新解析 `/etc/rinit/units/` 下所有单元文件。
3. **差异计算**:对比新旧单元配置集合,分类处理:
   - 新增单元 → 注册,不自动启动(除非是 default.target 依赖)。
   - 删除单元 → 若在运行,先停止再移除。
   - 修改单元 → 标记 `dirty`,仅对**停止态**单元即时生效;运行中单元延迟到下次重启时生效(避免运行中改配置的竞态)。
4. 解析失败时,**保持旧配置不变 + 返回错误**,不回滚到半更新状态。

**边界处理**:

- 依赖图变化需重建 DAG 并重新做环检测,环则整体拒绝本次 reload。
- 用**双缓冲**:新配置解析完整成功后才原子替换,杜绝中间态。

### 7.7 P3 —— 模板单元与 `%i` 实例化

**设计目标**:一个单元文件派生多个实例,适配"同类型多实例"部署(如多个 worker)。

**配置示例**:

```ini
# worker@.service(注意 @ 后缀表示模板)
[Service]
ExecStart=/usr/bin/worker --id %i
Environment=INSTANCE=%i
```

启动时 `rctl start worker@1` → 实例化为 `worker@1.service`,`%i` 替换为 `1`。

**核心流程**:

1. 解析器识别文件名含 `@` 的为模板单元,提取实例名。
2. 替换 `%i`(实例名)/`%n`(完整名)/`%p`(前缀)。
3. 每个实例在运行时独立成 `Unit`,互不干扰。

**边界处理**:模板单元本身(无实例名)不可直接启动,报错提示需带实例;替换仅在解析期完成一次,运行时不再做字符串插值(性能 + 避免注入)。

### 7.8 P3 —— 单元间 socket 依赖(`Sockets=`)

**设计目标**:表达"服务 A 提供 socket、服务 B 消费该 socket"的显式依赖,避免用启动顺序间接表达导致的竞态。

**配置示例**:

```ini
[Service]
ExecStart=/usr/bin/daemon
Sockets=/run/daemon.sock     # 本单元负责创建
```

```ini
[Service]
ExecStart=/usr/bin/client
NeedsSocket=/run/daemon.sock  # 等待该 socket 存在再启动
```

**核心流程**:

1. `NeedsSocket` 与 socket 提供单元的依赖边写入 DAG。
2. 就绪判定:socket 文件路径存在(非仅进程 Active),作为更精确的 readiness 信号。
3. 可选联动 socket 激活(见 7.9)。

**边界处理**:socket 路径归属不明确(多个单元声明同一 socket)时启动期报错;`NeedsSocket` 指向不存在提供者时按 `Wants` 弱依赖降级处理。

### 7.9 P3 —— socket 激活(按需拉起)

**设计目标**:空闲服务不常驻,收到连接时才拉起,显著节省内存(嵌入式核心收益)。

**核心流程**:

1. 单元声明 `SocketActivate=/run/api.sock`,rinit 代为 `bind`+`listen`。
2. 连接到达 → rinit 拉起服务 → 通过 `SCM_RIGHTS` 把 fd 传给子进程(而非让服务自己 bind)。
3. 空闲超时(如 `IdleTimeoutSec=60`)后停止服务,释放内存。

**边界处理**:

- 服务退出后 rinit 继续持有监听 fd,下次连接再拉起(状态机在 `Active`↔`Inactive` 间由 socket 事件驱动)。
- 复杂度最高,仅建议在明确的内存瓶颈场景落地,否则投入产出比低。

### 7.10 P2 —— `Type=forking`(传统 daemon 化服务)

**设计目标**:支持无法前台化的传统守护进程(已在 M2 从 `Type` 支持中移除,见 §1.1 Type 决策)。

**核心难点**:rinit 必须能**定位守护进程 PID**。传统 daemon 常 double-fork,守护进程被 PID 1 收养后与 unit 无关联。解决方案(结合 cgroup v2,M5 已启用):

1. **`PIDFile=` 配置**:按 systemd 方式,daemon 把 PID 写入指定文件,rinit 读取后确认守护进程 PID,写入对应 cgroup。
2. **cgroup 反查**:把所有 fork 出的进程放进同一 cgroup,用 `cgroup.procs` 反查存活进程;但需区分"主进程"与"守护进程"。
3. **启动完成判定**:主进程退出(码 0)且能定位到守护进程 → 进入 `Active`;无法定位 → `Failed`。

**配置格式**:

```ini
[Service]
Type=forking
ExecStart=/usr/bin/legacy-daemon
PIDFile=/run/legacy-daemon.pid
```

**边界处理**:`PIDFile` 未设置且无法通过 cgroup 唯一确定守护进程时,启动判 `Failed`(fail-closed);仅建议在确有旧守护进程时启用,新服务一律要求前台化(`simple`/`notify`)。

> 说明:此为 P2 扩展,依赖 M5 的 cgroup v2;M0~M2 不含 forking(已明确砍掉)。

### 7.11 P2 —— 终端服务扩展(`TTYPath=`/`TTYReset=`)

`getty` 类服务需要 `setsid` + 控制终端。在 `[Service]` 段增加两个 P2 配置项:

| 配置项        | 类型      | 含义                                     |
| ---------- | ------- | -------------------------------------- |
| `TTYPath`  | `String` | 指定控制终端设备(如 `/dev/ttyAMA0`),rinit 为其打开并设为控制终端 |
| `TTYReset` | `bool`   | 打开终端前是否发送 ioctl 复位,默认 `true`            |

**流程**:`TTYPath` 存在时,fork 后子进程 `setsid()`、打开 `TTYPath` 作为 stdin/out/err 并 `ioctl(TIOCSCTTY)` 设为控制终端,再 exec。这是 §1.3(6) 进程组/session 规则的配套扩展。

### 扩展项优先级与落地时机汇总

| 优先级 | 项              | 落地里程碑 | 触发条件          |
| --- | -------------- | ----- | ------------- |
| P0  | 信号安全(7.1)      | M0    | 必须,正确性根基      |
| P0  | target 抽象(7.2) | M1    | 必须,依赖编排基石     |
| P1  | cgroup v2(7.3) | M5    | 内存约束必须        |
| P1  | 权限降级(7.4)      | M5    | 安全需求          |
| P2  | 看门狗(7.5)       | M5    | 有 watchdog 硬件 |
| P2  | 热加载(7.6)       | M4 后  | 变更频繁时         |
| P2  | `forking`(7.10)  | M5 后  | 有无法前台化的旧守护进程 |
| P2  | 终端服务(7.11)     | 按需    | getty 等需控制终端  |
| P3  | 模板实例化(7.7)     | 按需    | 多实例部署         |
| P3  | socket 依赖(7.8) | 按需    | 服务间 socket 耦合 |
| P3  | socket 激活(7.9) | 按需    | 明确内存瓶颈        |

---

*本计划为 v1.0 定稿(经三轮外部评审修订),建议在 M0 首跑通过后回填更精确的工期与内存基线。后续变更走版本化修订(v1.1、v1.2),不再做结构性调整。*

