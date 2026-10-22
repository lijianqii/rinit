# rinit

一个用 Rust 编写的、类 systemd 的轻量级 PID 1 init 与服务管理器，面向资源受限的
嵌入式 Linux（aarch64、armv7）。完整开发计划见 [`rinit-开发计划.md`](rinit-开发计划.md)；
本 README 记录实现状态。

> 本文是 [`README.md`](README.md) 的中文译本。**英文版是唯一权威状态来源**：
> 数字、断言文本和命令以英文版为准，两者不一致时以 `README.md` 为准。

## 状态：M0–M5 已实现，已在 aarch64 和 armv7 上验证

每个里程碑的范围、以及计划扩展清单（§7.1–§7.11）的每一项都已实现并验证过：
[验收](#验收计划-6-的清单) 一节逐条回答了计划 §6 的清单，只有一个例外——72 小时
soak，它是部署期跑一次 `make soak`，而不是一个功能。

下面列出现有的能力，以及各自的证据。每一条结论要么来自宿主测试（`make check`），
要么来自客户机断言（`make test-system`，`scripts/system-test.sh` 里共 104 条）；
"在宿主机上验证"始终指同一段代码的 fake-runtime 或 `--dev` 形态，"客户机"指真实形态。

| 里程碑 | 内容 | 证据 |
| --------- | ------- | -------- |
| M0 | 通过 `rdinit=` 成为 PID 1；第一波挂载（`/run` → `/proc` → `/sys` → `/dev` → `/tmp` → `/sys/fs/cgroup`），按计划 §1.3(9) 区分致命/可降级；第二波挂载（`/etc/fstab`，计划 §1.3(9)）按文件顺序执行，失败只记录日志、启动继续；`SIGCHLD` + `waitpid(-1, WNOHANG)` 回收子进程；panic hook 写入 `/dev/kmsg` | 客户机："mounted /run…"、"mounted /sys/fs/cgroup"、"mounted /data (fstab)"、"fstab-tmpfs /data tmpfs"、"no rinit panic"；宿主：`crates/rinit-runtime/src/fstab.rs`（6 个测试）、`crates/rinit/tests/smoke.rs` |
| M1 | 从 `/etc/rinit/units` 解析单元文件，解析依赖图，按层级启动单元，同一层内并行 | 客户机："resolved a boot plan"、"startup levels run in dependency order"、"no target was exec'd" |
| M2 | 进程监管（spawn、跟踪、回收、两阶段停止），带退避和熔断的重启策略，健康检查，`Type=notify` 就绪通知，内存日志环及其可选的落盘镜像（计划 §1.3(7)） | 客户机："a crashed service is scheduled for restart"、"the breaker tripped on the failing unit"；宿主：`crates/rinit/tests/m2_lifecycle.rs`（45 个测试） |
| M3 | 控制 socket + `rctl`（`list`、`status`、`start`、`stop`、`restart`、`reset-failed`、`log`、`reload`），退出码契约有文档 | 客户机："rctl list reached the control socket" 及其后的五条断言；宿主：`crates/rctl/tests/control.rs`（9 个测试） |
| M4 | 持续 churn 下的 soak harness（`make soak`），泄漏与描述符修复，`--dev` 模式下控制 socket 对端放宽 | `scripts/soak.sh`（见 [测试](#测试)）；`crates/rinit-core/tests/allocations.rs` |
| M5 | **§7.3–§7.11 已完成。** cgroup v2：`CgroupCPUMax=`、`CgroupMemoryMax=`、`CgroupMemoryHigh=`、`CgroupTasksMax=`，写在 `rinit/<unit>` 下；进程自己把自己放进 cgroup；停止流程向 cgroup 成员发信号；`rmdir` 清理；OOM kill 检测接入重启策略。沙箱：`User=`、`Group=`、`NoNewPrivileges=`、`CapabilityBoundingSet=`、`AmbientCapabilities=`。硬件看门狗：启动时打开 `/dev/watchdog`，由监管循环的健康检查点喂狗，关闭时 magic close。重载：`rctl reload` 重新读取单元目录，增、改、延迟、删，并拒绝无法解析的目录。传统守护进程：`Type=forking` 配 `PIDFile=`，通过文件或单元自己的 cgroup 定位。终端：`TTYPath=`/`TTYReset=` 给服务一个控制终端（计划 §7.11）。模板：`worker@.service`，实例化时展开 `%i`/`%n`/`%p`（计划 §7.7）。socket 作为依赖：`Sockets=`/`NeedsSocket=`，解析成硬依赖并等待路径出现（计划 §7.8）。socket 激活：`SocketActivate=`/`IdleTimeoutSec=`，rinit 绑定 socket，连接到来时启动服务（计划 §7.9）。至此，**计划扩展清单（§7.1–§7.11）的每一项都已实现** | 客户机："a limited unit was placed in its own cgroup"、"the memory hard and soft limits were written in bytes"、"a stopped unit's cgroup was removed"、"the kernel OOM-killed a process inside the unit's cgroup"、"rinit armed the guest's watchdog device"、"the driver took the timeout rinit set"、"the guest outlived the watchdog timeout with the device armed"、"an added unit the boot target does not need was not started"、"a changed unit that is running kept it for the next start"、"a directory that does not resolve was refused whole"、"the unit is active with the daemon the PIDFile named"、"stopping a forking unit stopped the daemon it took over"、"a TTYPath that is not a terminal fails the start"、"a template is not a unit to start"、"every specifier was expanded before the command ran"、"a socket consumer waited for the path before starting"、"a socket that never appears fails the start, naming it"、"rinit bound the socket before any unit started"、"starting it hands the service the listening socket"、"a de-privileged service ran as nobody:nogroup with NoNewPrivs"、"an empty CapabilityBoundingSet left a root service with no capabilities"、"ambient capabilities reached a de-privileged service"；宿主：`crates/rinit/src/cgroup.rs`（19 个测试）、`crates/rinit-core/src/unit/cgroup.rs`（9 个）、`crates/rinit-runtime/src/syscall/cgroup.rs`（14 个）、`crates/rinit/src/watchdog.rs`（6 个）、`crates/rinit-runtime/src/syscall/watchdog.rs`（6 个）、`crates/rinit/src/unit_manager.rs`（reload 本身 5 个，`reload.rs` 是它们之上的策略）、`crates/rinit/tests/m2_lifecycle.rs`（11 个，端到端）、`crates/rinit/src/sandbox.rs`（14 个）、`crates/rinit-core/src/unit/sandbox.rs`（9 个）、`crates/rinit-runtime/src/syscall/cred.rs`（14 个） |

下面是已提交代码在串口控制台上的一次真实启动（`…` 省略内核噪声；除系统测试
会启动验收镜像、因此计划行里还算上了 `units/demo/` 的单元之外，这些行正是
`make test-system` 断言的内容）。这就是 `make run`，启动目标的闭包里是控制台和
DHCP 客户端：

```
[    2.344175] Run /sbin/rinit as init process
rinit: rinit 0.1.0 starting (pid 1)
rinit: mounted /run
rinit: mounted /proc
rinit: mounted /sys
rinit: mounted /dev
rinit: mounted /tmp
rinit: mounted /sys/fs/cgroup
rinit: mounted /data (fstab)
rinit: cgroup v2 ready at /sys/fs/cgroup
rinit: logging to /var/log/rinit/log
rinit: boot plan for 'multi-user.target': 5 unit(s) in 3 level(s)
rinit: plan 0: rinit-init.target (target)
rinit: plan 1: getty.service (service)
rinit: plan 1: network.service (service)
rinit: plan 2: multi-user.target (target)
rinit: starting level 0: 0 service(s)
rinit: level 0 reached: rinit-init.target active
rinit: starting level 1: 2 service(s)
rinit: started getty.service as pid 48
rinit: started network.service as pid 49
rinit: starting level 2: 0 service(s)
rinit: level 2 reached: multi-user.target active
rinit: boot complete: 2 unit(s) started from 'multi-user.target' (2 still running)
rinit: boot timing: first service at 5190ms of kernel uptime (rinit's own boot: 505ms)
rinit: watchdog /dev/watchdog armed: 30s timeout, feeding every 15s

(none) login:
```

这次启动里有两行是测量值而不是步骤。`mounted /data (fstab)` 是计划 §1.3(9) 的
第二波挂载——一条 `/etc/fstab` 记录，在第一波之后挂载；`boot timing: first service
at 5190ms of kernel uptime (rinit's own boot: 505ms)` 是计划 §4.3 的数字，用的是
内核自己的时钟，因此也覆盖了内核启动阶段。两个数字都来自同一次运行，而那次运行的
*宿主机*同时还在跑 72 小时 soak，数字每次启动都会变（同样负载下更早的一次读到
2590 ms 和 239 ms）；客户机断言的是"这个测量取自内核时钟"，不是某个具体数值。

注意计划解析出 **5 个单元**、启动 **2 个**：`units/` 是启动集——两个 target、
控制台及其替代品、以及 DHCP 客户端——并且只启动启动目标的闭包。`shell.service`
会被解析和排序，但不会被激活，因为否则它会和 `getty.service` 抢 `ttyAMA0`。
`network.service` 在闭包内：`multi-user.target` 用 `Wants=` 拉入它（没有网卡的板子不应让 target 失败），qemu 的用户态 DHCP 给
客户机分配 `10.0.2.15/24`、网关 `10.0.2.2` 和 DNS `10.0.2.3`。§7.x 的验收单元放在
`units/demo/`，`make test-system` 会打包它们，`make run` 不会：`priv`/`caps`/
`ambient`（§7.4）、`cglimit`/`cgrun`/`oom`（§7.3）、`forking`（§7.10）、
`logdemo`、`tty`、`sockd`、`sockc`、`api` 和 `worker@` 模板都在闭包之外，需要
从控制台用 `rctl` 显式启动。M2 的 `fault.service` 也是其中之一，所以它演示的
熔断出现在系统测试的日志里，而不是这次启动里。看门狗的 `30s` 是 `make run` 的
取值；`make test-system` 会把它缩短到 `10s`（见下面的 CLI 一节），好让客户机活过
这个超时，从而证明设备确实在被喂。

§7.3 的两半，同样来自控制台。先看*服务自己*读回的限额——它自己的
`/proc/<pid>/cgroup`、它自己的属性文件——然后是一次 OOM kill，rinit 的日志行
后面紧跟内核的：

```
~ # rctl log cglimit.service
cglimit.service: self=0::/rinit/cglimit.service child=0::/rinit/cglimit.service
cglimit.service: cpu=50000 100000 mem=33554432 high=25165824 pids=64

~ # rctl start oom.service
rinit: oom.service: cgroup /sys/fs/cgroup/rinit/oom.service with memory.max (memory.max=16777216)
rinit: started oom.service as pid 105
[   18.114347] Memory cgroup out of memory: Killed process 105 (sh) total-vm:26976kB, anon-rss:16264kB, file-rss:0kB, shmem-rss:1280kB, UID:0 pgtables:80kB oom_score_adj:0
rinit: oom.service was killed by the cgroup OOM killer (memory.max=16777216)
rinit: oom.service will restart in 1.139s
```

`self=0::/rinit/cglimit.service` 是放置结果，`0::` 是 unified 层次结构的标记；
`child=` 是该服务 fork 出来的进程，rinit 什么都没做，内核就把它放进了同一个
cgroup。OOM 单元是一个 shell，它把字符串翻倍直到超过自己的 `memory.max`，所以
kill 来自*cgroup 的* OOM killer；随后 rinit 指出 `SIGKILL` 背后是哪条限额，该单元的
`Restart=on-failure` 把它拉回来（只一次；第二次被杀就会触发熔断）。

§7.5 的看门狗，同样来自控制台。设备是真的——`run-qemu.sh` 挂上 qemu 的
`i6300esb`，内核也带该驱动编译——所以下面的超时是那个驱动对 rinit
`WDIOC_SETTIMEOUT` 的回答，而不是替身文件的。真正有意义的是 uptime：设备在启动后
几秒内就以 10 s 超时被武装，因此一台远晚于此时仍在运行的机器，其监管循环必然在喂狗。

```
~ # d=/sys/class/watchdog/watchdog0; echo wd-ident=$(cat $d/identity) wd-state=$(cat $d/state) wd-timeout=$(cat $d/timeout)
wd-ident=i6300ESB timer wd-state=active wd-timeout=10
~ # u=$(cut -d" " -f1 /proc/uptime); echo wd-uptime=$u
wd-uptime=18.64
```

这些命令是刻意这么写的：控制台会回显输入内容，而那些行会进入断言读取的同一份
日志，所以标记必须由变量拼出（系统测试里的 `v=populated; ... echo cgrun-$v`，这里的
`$d/identity`），而不是字面出现在输入里——断言被自己的回显满足，什么也证明不了。

§7.6 的 reload，是唯一一个证据*必须*来自客户机的功能：initramfs 把单元目录解包到
ramfs（启动集，验收镜像里再加上 `units/demo/`），所以控制台可以在 rinit 运行期间
增、改、删单元文件。下面四次 reload 分别是"什么都没变"、"加了一个"、"一个已停止
的单元被改、一个正在运行的单元被改"、以及"删了一个"：

```
~ # rctl reload
rinit: reload: 0 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 17 unchanged
~ # echo "[Service]" > /etc/rinit/units/reload.service
~ # echo "Type=oneshot" >> /etc/rinit/units/reload.service
~ # echo "ExecStart=/bin/echo reloaded-service" >> /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: registered reload.service
rinit: reload: 1 added (0 started), 0 changed (0 deferred), 0 removed (0 stopped), 17 unchanged
~ # echo "ExecStartPre=/bin/echo reloaded-pre" >> /etc/rinit/units/logdemo.service
~ # echo "ExecStartPost=/bin/true" >> /etc/rinit/units/getty.service
~ # rctl reload
rinit: reload: logdemo.service: new definition in force
rinit: reload: getty.service is running; its new definition applies at the next start
rinit: reload: 0 added (0 started), 2 changed (1 deferred), 0 removed (0 stopped), 16 unchanged
~ # rm /etc/rinit/units/reload.service
~ # rctl reload
rinit: reload: removed reload.service
rinit: reload: 0 added (0 started), 1 changed (1 deferred), 1 removed (0 stopped), 16 unchanged
```

`1 added (0 started)` 一行就是计划里的"注册，不自动启动（除非是 default.target
依赖）"：没有任何东西把 `reload.service` 拉进闭包，所以它存在但不运行。
`getty.service is running` 是延迟——控制台自己继续用它启动时的定义，`ExecStop=`
也包含在内，新定义到下次启动才采纳。最后一次 reload 仍把 `getty.service` 算作
已改变，这不是噪声而是准确的：文件仍与正在运行的定义不同，下次启动仍有东西要采纳。

§7.10 的 `forking` 守护进程，同样来自控制台。`forking.service` 是一个 shell，
它把 `sleep` 放到后台然后退出，这正是双 fork 守护进程的形态——也是唯一一类
"单元对应的进程不是 rinit 启动的那个进程"的单元：

```
~ # rctl start forking.service
rinit: started forking.service as pid 122
rinit: forking.service: main process exited; daemon is pid 123
~ # rctl status --plain forking.service
unit: forking.service
state: active (pid 123)
~ # rctl stop forking.service
rinit: stopping forking.service (pid 123)
rinit: reaped pid 123
```

以 pid 122 启动、`Active` 时是 pid 123：`PIDFile=` 说明这个单元现在对应哪个进程，
单元的限额、停止信号和清理都跟着那个 pid，而不是已经退出的那个。`stopping ...
(pid 123)` 是需要改动的那一半——守护进程从 fork 它的进程继承了进程组，所以按组
发信号会指向一个已经不存在的组，停止流程于是回退到对 pid 本身发信号。客户机断言
之后守护进程确实消失了，而未跟踪的守护进程不会消失。

以及 fail-closed 的边界，用一个运行时写出来的单元演示：

```
~ # rctl start badfork.service
rinit: error: badfork.service: cannot locate the daemon: cannot read PIDFile /run/not-written.pid: No such file or directory (os error 2)
rinit: badfork.service exited: failed
```

`badfork.service` 是 `Type=forking`，`PIDFile=` 从未被写，也没有 `Cgroup*=` 可回退。
两者都是"不要猜"的理由：单元失败，说明它读了哪个文件，并通知依赖它的单元——这就是
计划里的"无法定位 → Failed"，也是本 README 建议新东西不要用 `forking` 的原因。
§7.8 的 socket 依赖和 §7.9 的激活，同样来自控制台。前者是围绕启动顺序的一次
*等待*——提供方是 `Type=simple`，所以它在创建 socket 之前很久就已经 `Active`——
后者是 rinit 为一个故意不运行的单元持有的 socket：

```
~ # rctl start sockd.service          # creates /run/sockd.sock a second after exec
rinit: started sockd.service as pid 142
~ # rctl start sockc.service          # NeedsSocket=: blocks until the path is there
rinit: started sockc.service as pid 145
~ # rctl log sockc.service
sockc.service: socket-present
~ # test -S /run/api.sock && echo api-socket-bound
api-socket-bound
~ # s=$(rctl status --plain api.service | grep state); echo api-state=$s
api-state=state: inactive
~ # rctl start api.service            # the socket rinit bound, handed over on fd 3
~ # rctl log api.service
api.service: activated-fd-3
```

`socket-present` 就是那次等待：消费者对路径执行了 `test -f` 并找到了它，而它只可能
在等待一个尚未创建该路径的提供方之后才做到。`api-state=state: inactive` 正是激活的
全部意义——单元没运行，而 socket 存在——`activated-fd-3` 则是契约抵达服务的证据：
`test -S /proc/self/fd/3` 只对已连接的 socket 描述符为真，所以交给服务的是*监听*
用的那个。客户机无法建立连接（busybox 的 `nc` 不支持 Unix socket），因此
"连接驱动启动"和"空闲停止"由 m2 测试覆盖。

§7.7 的模板单元，是唯一一个*名字*本身就是功能的场景。文件是 `worker@.service`；
在有人请求之前实例并不存在，而展开发生在解析器看到命令之前：

```
~ # rctl start worker@.service
rctl: worker@.service is a template; start an instance, e.g. worker@<name>.service
~ # rctl start worker@42.service
rinit: started worker@42.service as pid 137
~ # rctl log worker@42.service
worker@42.service: worker-instance=42 name=worker@42.service prefix=worker env=42
```

`%i`、`%n`、`%p` 分别是实例名、完整单元名和前缀；`env=42` 是同样的展开发生在
`Environment=` 行里的结果，这也是演示程序两者都打印的原因——命令里的 `$INSTANCE`
是*shell* 在读环境变量，与替换是两套机制。模板本身按其本来面目列出
（`worker@.service: template`）并拒绝启动，而一个实例从那一刻起就是普通单元。

## 目录结构

```
crates/
  rinit-core/         # 与平台无关的引擎（no_std + alloc）
    src/unit/         # parser.rs, dependency.rs, config.rs, job.rs, restart.rs,
                      # notify.rs, sandbox.rs（§7.4 的降权 *计划*）, cgroup.rs
                      # （§7.3 的限额模型）, state/
    src/              # control.rs（协议）, logger.rs, users.rs, error.rs,
                      # ratelimit.rs（控制台输出，按调用点限流）
    tests/allocations.rs
  rinit-runtime/      # 系统调用层、挂载、信号、子进程回收（std/libc/tokio）
    src/syscall/      # process.rs, wait.rs, cred.rs（id + capability）, fs.rs,
                      # mount.rs, socket.rs, signal.rs, reboot.rs, cgroup.rs,
                      # watchdog.rs（喂狗/解除武装协议）
    tests/syscall.rs, tests/shipped_units.rs
  rinit/              # PID 1 二进制
    src/boot.rs       # 启动流程 + 主循环（只做编排）
    src/unit_manager.rs, service.rs, control.rs, logs.rs, log.rs
    src/sandbox.rs    # `User=`/`Group=`/capabilities -> Sandbox（唯一的解析器）
    src/cgroup.rs     # §7.3：在哪里施加限额，以及失败分类
    src/watchdog.rs   # §7.5：本次启动有没有看门狗，以及喂狗节奏
    src/reload.rs     # §7.6：`rctl reload` 应用的差异，及其报告
    src/notify.rs, readiness.rs, deadline.rs, args.rs, manager.rs
    tests/m2_lifecycle.rs, tests/smoke.rs
  rctl/               # 控制 CLI（main.rs, parse.rs, client.rs）+ 测试
units/                # 启动集：两个 target、getty.service（`make run` 启动的
                      # 控制台登录）、network.service（DHCP）和替代品 shell.service
units/demo/           # §7.x 的验收单元；只打进 `make test-system` 启动的镜像，
                      # 并从控制台手动启动
units/device/         # 可部署设备服务；只打进 `run-disk` 的 ext4 镜像
scripts/              # fetch/kernel/busybox/dropbear/initramfs/disk/qemu/tests/soak
```

## 构建

本环境完全离线构建，依赖本地 cargo 缓存：

```bash
cargo build --offline                 # host (debug)
cargo test  --offline                 # unit + integration + smoke tests
cargo clippy --offline --all-targets

# embedded targets (static, default per plan §2 risk mitigation)
cargo build --offline --release --target aarch64-unknown-linux-gnu
cargo build --offline --release --target armv7-unknown-linux-gnueabihf
```

## 测试

五层，从最便宜的开始：

```bash
make check          # fmt + clippy -D warnings + host tests  (no cross toolchain)
make soak           # 60s of sustained churn; watches RSS/fds/zombies
make test-system    # boot the selected ARCH in qemu and assert the acceptance list
make test           # check + test-system
make gates          # acceptance gates: binary size + coverage thresholds
```

`cargo test --offline --workspace`（`make check` 最后一步跑的就是它）在十个测试
二进制里执行 **737 个测试**。五种形态，以及为什么每种都不是多余的：

- **单元测试**紧挨着代码（core 331、rinit 199、rinit-runtime 105、rctl 15），
  包括解析器上的 `proptest` 属性测试；
- **集成测试**在 `crates/*/tests/`：系统调用层面对真实内核（`rinit-runtime` 22 个：
  以非特权身份运行，断言每次降权都*被拒绝*而不是被静默跳过——`setuid(0)`、
  `setgroups`、`PR_CAPBSET_DROP`、`capset` 和环境能力提升各自返回 `EPERM`，并指出
  失败的是哪个调用——只有 root 才能成功的路径留给客户机），`--dev` 模式下的端到端
  生命周期（`rinit/tests/m2_lifecycle.rs` 45 个），控制 CLI（`rctl/tests/control.rs`
  9 个），运行真实二进制的 smoke 测试（`rinit/tests/smoke.rs` 2 个），分配守卫
  （`rinit-core/tests/allocations.rs` 2 个），以及 `shipped_units.rs` 7 个——
  `units/` 和 `units/demo/` 里的每个单元都要过一遍加载器，因此会在启动时失败的单元
  先在这里失败；打包脚本把两个目录拍平时，还会检查它们没有同名文件互相遮蔽；
- **soak harness**（`scripts/soak.sh`，计划 §3 M4/§4.4）：在 churn 下运行 rinit，
  采样 RSS/描述符/线程/僵尸，循环驱动控制 socket，如果稳态不平坦、或 `SIGTERM`
  没有以 0 退出，就判定失败。默认 60 秒是提交时检查；`make soak SOAK_ARGS='--seconds
  259200'` 才是计划要求的 72 小时验收运行。rinit 的控制台是一次运行中唯一没有上界的
  东西——churn 单元是故意不限速的，所以每次重启都会打日志——而它经过的读取器只保留
  断言需要的计数和 2000 行尾部，这就是 72 小时运行只留下约 100 KB、而不是把磁盘写满
  的原因。rinit 自己也对控制台限流，按调用点而不是按消息文本——重启循环反复打印的
  就是那几条消息，只是 pid 不同，所以按文本归并的限流器一条都合并不了
  （`rinit_core::ratelimit`）；
- **验收门禁**（`make gates`）：计划 §0.1 的体积预算和计划 §6 的覆盖率阈值，是测出来的
  而不是抄来的，[验收](#验收计划-6-的清单) 和 [测量](#测量计划-01-校准点) 里的数字
  就来自这里。它们刻意*不*属于 `make check`：`size-check` 需要 release 构建，
  `coverage-check` 要跑几分钟，而 `make check` 是每次提交的门禁。
  `scripts/coverage-check.sh` 除了结论还会打印它测到的计数，两个阈值都是参数，
  这样门禁自己的算术也能被测；
- **客户机系统测试**（`scripts/system-test.sh`，计划 §4.3）：在所选架构的 qemu 上启动
  真实的 initramfs，在控制台上输入以证明 root shell 可达，对运行中的系统驱动
  `rctl`，并断言 104 件事——包括层级按依赖顺序运行、target 从不被 exec、§7.4 的
  降权结果、§7.3 的限额从客户机自己的内核里读回、rinit 自己在启动时和 20 个服务下的
  RSS（从 `/proc/1/status` 读）、§4.3 的启动耗时（取自内核时钟）、qemu 真实挂载的
  设备上的 §7.5 看门狗、DHCP 客户端拿到的地址/路由/DNS、针对客户机自己的（可写的
  ramfs）单元目录执行的 §7.6 reload，以及 §7.10 的 forking 守护进程——通过 `PIDFile=`
  定位，然后作为单元的进程停止。退出码 `0` 通过、`1` 断言失败、`2` 缺少产物、
  `3` qemu 无法启动。日志落在 `build/logs/`（成功时删除，除非传了 `--keep-log`）。
挂载策略（计划 §1.3(9)）通过 `MountOps` trait 覆盖，因此致命/可降级规则不需要
`CAP_SYS_ADMIN` 就能测试——这就是计划 §4.2 要求"fake runtime"的形态。§7.4 的降权计划
是同一个关注点的另一种表达：用数据而不是 trait 表示。`Sandbox::steps()` 返回有序
步骤，于是"`setuid` 在环境能力提升之前"这条在 `rinit-core` 里就被断言，而不必只有在
真实系统上以 root 运行时才观察得到。

§7.3 两种手段都用上了，因为它包含两类内容。取值模型（指令 → 文件 → 内容，以及百分比
换算）是 `rinit-core` 里的数据，有单元测试。文件操作把 cgroup 目录作为*参数*，所以把
`--cgroup-root` 指向一个满是普通文件的目录时，整条真实路径——创建、写入、放置、读回、
清理——都能在宿主机的 `--dev` 下跑，不需要 root，也不需要 `/sys`。这个替身做不到的
是 `rmdir` 一个装满替身属性文件的目录，因此"删除 cgroup"和对活的 `cgroup.procs` 的
检查都在客户机上针对真实 cgroupfs 断言。

§7.5 形态相同，极性相反。看门狗的*协议*是描述符上的字节，所以宿主测试把
`--watchdog` 指向一个文件，再从文件里读回喂狗字节和 magic close——这也是一台自带
`/dev/watchdog` 的机器上唯一安全的测试方式。*ioctl* 和硬件只存在于客户机，因此
qemu 的 `i6300esb` 设备和带该驱动编译的内核是测试装置的一部分，而不是可选附加项：
没有它们，客户机只能走"没有看门狗硬件"那条路径。

`make coverage-check` 是这些数字的来源——`cargo llvm-cov --offline --workspace
--summary-only`，再把计划 §6 的阈值套到结果上——所以它们是在当前代码树上测出来的，
而不是从某次运行里抄来的：

| crate            | 行覆盖率      | 函数      |
| ---------------- | ------------- | --------- |
| `rinit-core`     | **97.76%**    | 576/587   |
| `rinit-runtime`  | 84.77%        | 282/326   |
| `rinit`          | 91.10%        | 620/666   |
| `rctl`           | 95.56%        | 32/38     |
| **总计**         | **92.32%**    | 1510/1617 |

计划清单要求总体 ≥80%、核心 ≥90%，所以回答它的是这些数字，而不是某种形状：

覆盖率最低的多数是*无法*在宿主机上运行的那些，这也是客户机测试存在的原因，而不是
遗漏：`syscall/reboot.rs`（0%——它会重启机器）、`syscall/mount.rs`（46.84%——需要
`CAP_SYS_ADMIN`）、`syscall/process.rs`（56.80%——子进程侧的降权路径，宿主运行按设计
会拒绝）、`src/main.rs`（46.67%——入口）和 `src/control.rs`（70.72%——socket 服务器，
客户机会驱动它，宿主机只能通过 `RINIT_CONTROL_DIR` 到达）。这个区间里唯一能在宿主机
上运行的文件是 `rinit-core/src/error.rs`（52.94%），它未覆盖的行是 `ParseError` 和
`TransitionError` 的 `From` 实现与 `Display` 分支。

未被覆盖的行有两类，值得区分：**仅客户机**的行（控制服务器、子进程侧的沙箱步骤、
`mount(2)`），系统测试会在真实内核上执行它们，但 `cargo llvm-cov` 看不到，因为它
只跑宿主套件；以及**真正没被走到**的行，这个数字正是为后者服务的。这次测量之后的
审计针对的就是第二类：它删掉了 `rinit-testservice`（M0 的 bring-up 辅助程序，覆盖率
0%，因为好几个月没人跑过它——`--test-service` 和真实的单元路径已经取代了它），并补上
同一份清单指向的两个测试，一个针对 `SIGINT`（有处理，且是四个关闭信号里唯一没有
测试的），一个针对每条 `ParseError` 消息。

```bash
cargo llvm-cov --offline --workspace --summary-only          # the table above
cargo llvm-cov --offline --workspace --html                  # target/llvm-cov/html
```

**已知缺口**，记录在案而不是藏起来：

- **日志*文件*是镜像，不是日志库。** 计划 §1.3(7) 的可选持久化已实现
  （`--log-file`，默认 `/var/log/rinit/log`，超过 1 MiB 截断），但它只是用普通
  `write_all` 把日志环里最新的若干行抄一份——没有按单元分文件，不能对文件做
  `journalctl` 式查询，也没有 syslog 协议。`rctl log` 读的是日志环，所以一行只在
  文件存在期间才在文件里；它能不能活过一次重启取决于文件系统（tmpfs 不能，磁盘能）。
- **`--dev` 无法切换到其他用户，也无法丢弃 bounding set。** 非特权 rinit 从
  `setgid`、`setgroups` 和 `PR_CAPBSET_DROP` 得到 `EPERM`，所以这些单元会失败，
  而不是跳过降权继续运行——这是有意的 fail-closed，也是为什么 §7.4 的*成功*路径只能
  在客户机（rinit 是 root）里证明。宿主测试证明的是拒绝行为。
- **关闭路径——`reboot(2)`、SysRq 兜底、看门狗的 magic close——没有被自动化测试
  覆盖。** 在宿主机上它不能运行（会重启机器），而系统测试是直接杀掉 qemu，而不是请
  客户机自己关机，所以"向客户机的 PID 1 发 `SIGTERM`"是手工检查项。其中唯一有自动化
  证据的是 magic close：在 `m2_lifecycle.rs` 里针对替身设备断言，文件在喂狗字节之后
  出现 `V`。在客户机里，内核在重启时也会停止设备（`watchdog_stop_on_reboot`），
  所以一次优雅关闭有两条独立理由不会复位机器。
- **看门狗*到底能干什么*没法在不卡死 rinit 的前提下测试。** 复位本身没有演练过：
  证明它意味着故意把 PID 1 卡住直到硬件触发，那会终结正在被验证的这次运行。已经证明
  的是它之前的一切——设备被打开并报告、超时传到真实驱动、监管循环在喂它（客户机活过
  了一个"不喂就活不过"的超时）、并且客户机只启动一次。
- **cgroup 限额只针对运行内核实际具备的控制器证明。** 客户机内核编译进了计划四条指令
  需要的全部四个控制器（Makefile 会检查），所以*跳过*分支——没有 `pids` 控制器、
  没有 `cpu.max`——在宿主机上针对一个干脆不带该属性的替身目录演练。内核*拒绝*的写入
  （除 `ENOENT` 以外的任何 errno）同理：用一个"该有属性文件的位置是目录"来替身，
  因为要让真实 cgroupfs 拒绝一条限额，需要一个本测试控制不了的值或内核。
- **一个从不脱离的 `forking` 单元会被 cgroup 清扫，所以没有 `Cgroup*=` 的单元仍可能
  留下守护进程。** `Type=forking` 单元如果主进程永不退出，会被 `TimeoutStartSec`
  判失败，清扫会杀掉进程组，然后杀掉该单元 cgroup 里的一切（计划 §7.3 的第二阶段）。
  如果守护进程*两者都*脱离了——双 fork 之后调用 `setsid`，而单元又没声明 cgroup——
  就没有任何东西再指向它。正常路径不需要 cgroup，因为 `PIDFile=` 指明了守护进程；
  这只发生在行为异常的单元的超时路径上。
- **单元文件不能指名模板实例**（计划 §7.7）：`Requires=worker@1.service` 在加载目录时
  会被拒绝，因为依赖图是由文件构建的，而实例不是文件之一。实例按名字启动，这同时也是
  创建它的方式；systemd 的 `[Install]`/`WantedBy=` 符号链接在 rinit 里没有对应物。
- `signals.rs` 的管道写满路径（handler 丢弃一个字节而不是阻塞）没有在测试中强制
  触发。描述符泄漏测试不再依赖 fd 号是否被复用：它们在 SignalSet 存活时记录管道的
  inode，结束后检查该 inode 已关闭，因此旁边运行的测试不会让它们偶发失败。
- **`miri`、`valgrind` 和 `cargo-fuzz` 在本环境里跑不了**，各自用什么替代值得说明，
  而不是暗示没有任何检查：`miri` 需要 *nightly* 工具链（这里只装了 stable，
  `cargo miri` 自己会这么说，而且环境离线），所以它要在 `rinit-core` 里找的东西——
  泄漏和 UB——由其他方式直接覆盖：`crates/rinit-core/tests/allocations.rs` 让每条
  拥有分配路径的代码都经过一个计数全局分配器，并断言两批相同操作之间存活字节不增长
  （10,000 次迭代会把一个泄漏字节放大成 10 KB，于是每次迭代的泄漏会高出 harness 自身
  噪声），而该 crate 是 `#![forbid(unsafe_code)]`，所以里面没有 `miri` 可找 UB 的
  unsafe 代码。`cargo-fuzz` 的解码器用属性测试（`proptest` 覆盖解析器、控制协议编解码
  和任务队列），运行时侧对应 `valgrind` 的是 soak harness 采样的 RSS/描述符/僵尸曲线。
## 验收：计划 §6 的清单

计划 §6 是"完成"的定义。它的每一行要么是仓库里的命令，要么是本 README 里的测量：

| 计划 §6 | 现状 |
| ------- | --------------- |
| 在 qemu（128M）里作为 PID 1 启动，内核无 panic | `make test-system`：aarch64 与 armv7 各 104 条断言，包括 "no rinit panic" 和四条内核日志检查 |
| 20 个服务并行，明显快于串行 | **19.2x**：`make parallel`——20 个各睡 1 s 的 `oneshot` 服务，并行总耗时 1065 ms，串行 20 424 ms |
| 崩溃重启、熔断、健康检查 | `make test-system`（"a crashed service is scheduled for restart"、"the breaker tripped on the failing unit"）和 `crates/rinit/tests/m2_lifecycle.rs` |
| `rctl` 能启动、停止、查询、读日志 | `make test-system` 的 M3 段和 `crates/rctl/tests/control.rs` |
| 优雅关闭、无僵尸、无 fd 泄漏 | `make soak` 在重启 churn 中采样描述符、线程和僵尸子进程，并断言各自回到基线 |
| 72 小时 soak，RSS 稳定 | **正在运行**，第八段，2026-10-22 09:17 启动，工作目录放在 `build/` 而不是 `/tmp`；这次由启用了 linger 的 systemd 用户服务托管，关闭 SSH 会话不会把它杀掉，窗口在 2026-10-25 09:17 结束，届时数字写进本行。日志是 `build/soak-charts/soak-72h-seg8.log`。第七段在 2026-10-22 03:10 被提前终止，共 570 个采样（9.5 小时），不是断言失败：它虽然用 `setsid nohup` 启动，但仍继承了 SSH 会话的 cgroup，连接关闭时 systemd 清理 session slice，向 `make` 发出了 SIGTERM；曲线和日志保留为 `soak-72h-seg7.log` 与 `soak-seg7.note`。第六段在 1 小时 50 分（109 个采样）后由人工停止，而不是被断言终止：在 `--interval 1` 下每个采样约 70 ms，循环在采样后又完整睡一个 interval，于是默认 60 秒门禁只采到 60 个槽位中的 56 个，失败于它自己的采样覆盖检查（在僵尸扫描合并为单次 awk 之前只有 21 个）；现在循环睡到下一个绝对槽位，`make soak` 能采满 60/60。此前的每一段都不是被断言结束的：**8.92 小时**（535 个采样、18 519 600 次重启），前后半段 RSS 峰值均为 5116 kB，描述符最低值 14 → 14，僵尸 0；**44.6 小时**（2587 个采样），RSS 在 3892 到 3960 kB 之间以约 16 kB 的台阶上下移动——后半段斜率 −1.28 kB/h——每小时描述符最低值都在 13-14，僵尸 0，被一次主机重启终止，曲线也随 tmpfs 上的工作目录一起丢失；**43 分钟**，被 harness 自己的剩余空间地板终止，原因是机器上别的进程填满了共享的 978 MB `/tmp`——这也是现在把约 100 KB 的工作目录放在仓库文件系统上的原因；以及 **13.6 小时**（796 个采样，2026-10-20 06:09 → 19:46），被重启终止，但这次曲线没有丢，因为工作目录已经搬走：RSS 在启动后两次下台阶，逐小时均值前 3 小时为 5068-5087 kB，随后 5 小时为 4381-4443 kB，最后 6 小时为 4202-4252 kB，最后一个采样 4296 kB，描述符 14-28，线程 1，僵尸 0。这些运行带来了四处改动：描述符检查改为比较每一半的*最低*值而不是峰值（运行足够久之后峰值就是掷硬币——8.92 小时那一段峰值读到 25 → 24，而最低值是 14 → 14）；rinit 对自己的控制台限流，而控制台正是磁盘被写满那次的出口；harness 打印的重启次数是限流后的下界——它统计的那行被限制为每个调用点每 5 秒 40 条，所以 `churn: N restarts` 读出来约 8/s，而真实 churn 用宿主 `/proc/stat` 的 fork 速率独立测得约 545/s；以及采样器改为睡到下一个绝对槽位——原来每次采样后再睡满一个 `interval`，使一次 60 秒运行只采到 60 个样本中的 56 个，失败于一个它本来已经满足的覆盖检查 |
| 二进制 ≤2 MB 动态 / ≤4 MB 静态 | [测量](#测量计划-01-校准点)：aarch64 静态 1.57 MiB、armv7 静态 1.26 MiB，由 `make size-check` 检查 |
| 单元测试覆盖率总体 ≥80%、核心 ≥90% | **92.32%** 和 **97.76%**（`make coverage-check`，见[上文](#测试)） |
| 文档：架构、单元格式、`rctl`、移植 | 本 README（架构、状态、测量）、[`docs/units.md`](docs/units.md)（每条指令、取值语法和默认值）、[`docs/porting.md`](docs/porting.md)（内核、用户空间、构建、与 systemd 的差异），以及 `rinit-开发计划.md` |

还有一个清单没要求、但计划 §0.1 的设计目标要求的数字：rinit 的 RSS，在*客户机里*从
`/proc/1/status` 测得，而不是在宿主机上——空闲 **1 632 kB**，跑 20 个服务时
**1 872 kB**，对应 §0.1 的 2 MB 和 6 MB。有意思的是增量：多二十个单元只多花不到
200 kB，因为一个单元是一份配置、一个状态和几个定时器，而不是一个线程。

## 运行：`make run`（arm64 默认，arm32 也支持）

这条一步到位目标会从清华镜像下载、构建并启动所选架构所需的一切
（Linux **6.6.158** + busybox stable）：

```bash
make ARCH=arm64 run      # default; aarch64
make ARCH=arm32 run      # ARMv7 hard-float, qemu virt / Cortex-A15
```

> 需要联网（下载源码包）和对应交叉工具链：arm64 用 `gcc-aarch64-linux-gnu`，
> arm32 用 `gcc-arm-linux-gnueabihf`，另需 `qemu-system-arm bc flex bison
> libssl-dev`。下载缓存在 `build/dl/`；arm32 构建树在 `build/arm32/`，
> 不会和 arm64 共用内核或 busybox 配置。

它按顺序做这些事（见 [Makefile](Makefile)）：

1. 按 ARCH 构建静态 rinit/rctl（arm64 为 `aarch64-unknown-linux-gnu`，arm32 为 `armv7-unknown-linux-gnueabihf`）。
2. 从 `mirrors.tuna.tsinghua.edu.cn/kernel/v6.x/` 取 `linux-6.6.158.tar.xz`，
   构建 `arch/arm64/boot/Image`（arm64）或 `arch/arm/boot/zImage`（arm32）。
3. 取 `busybox-1.36.1.tar.bz2`，用 initramfs 需要的 applet 构建一个**静态** busybox。
4. 组装 `build/out/initramfs-<target>.cpio.gz`（arm32 在 `build/arm32/out/`；
   busybox + rinit + rctl + `units/`，再加上登录 shell 需要的 `/etc` 与
   `/var` 骨架）。验收镜像 `...-demo.cpio.gz` 是同一套根，再加 `units/demo/`，
   也是 `make test-system` 启动的那个。
5. 用 `qemu-system-aarch64 -machine virt -cpu cortex-a57 -m 128M`（arm64）或
   `qemu-system-arm -machine virt,highmem=off -cpu cortex-a15 -m 128M`（arm32）
   启动，并加 `-device i6300esb`——rinit 武装的看门狗（计划 §7.5）——以及 qemu 的
   用户态网络（`-netdev user` 加 `virtio-net-device`），DHCP 客户端就跑在它上面。

没有需要配置的东西：`make run` 总是构建过期的部分并启动单元目录。重跑不会下载、
不会编译——下面的文件目标已经编码了这一点，所以没有"跳过构建"开关。一个例外恰好
证明这条规则：`kernel-config-check` 在每次 `make kernel` 时都跑，因为用一个缺少
§7.3 和 §7.5 所需符号的 config 构建出来的内核，会让客户机上的限额或看门狗悄悄不存在。

```bash
make ARCH=arm64 run # build if needed, boot → interactive root shell
make ARCH=arm32 run # the same for ARMv7 (qemu virt, Cortex-A15)
make ARCH=arm64 run-disk  # persistent ext4 root filesystem + device services
make disk-image     # force a destructive rebuild of that ext4 image
make kernel         # kernel only
make busybox        # busybox only
make initramfs      # rinit + initramfs only
make initramfs-demo # the same, plus units/demo/ (what the system test boots)
make qemu           # alias for `make run`
make check          # fmt + clippy + host tests (no cross toolchain)
make soak           # churn + memory/fd/zombie regression check (host, --dev)
make test           # check + the qemu system test
make MIRROR=https://... run   # different source mirror
make KERNEL_VERSION=6.6.158 BUSYBOX_VERSION=1.36.1 ...   # pin versions
```

只要 `rinit`、`rctl`、`busybox`、`units/` 里的某个文件或某个打包脚本变了，运行镜像
就会重打——改一下 `units/getty.service` 就会重新打包，因为单元目录正是客户机要启动的
东西。验收镜像还依赖 `units/demo/`，而且只额外依赖它：改一个 demo 单元只会重打验收
镜像，运行镜像不动。

### 在客户机里拿到 shell

`make run` 启动 `units/getty.service`，它在串口控制台上给出登录提示。输入 `root`，
密码是 `/etc/shadow` 里的默认值 `toor`。上面的记录就是那个提示符。登录后
`/etc/motd` 会打印常用命令（`rctl list`、`rctl status getty.service`、
`rctl log getty.service`、`poweroff -f`），`/etc/profile` 会设置 `PATH` 并应用
`/etc/hostname`。`network.service` 运行 DHCP 客户端：qemu 的用户态网络给客户机分配
`10.0.2.15/24`、网关 `10.0.2.2` 和 DNS `10.0.2.3`，所以接口无需手工配置。

这刻意不是一个安全边界：能碰到串口的人已经拥有这台机器。镜像里打包了
`/etc/shadow`，默认凭据是 `root`/`toor`，仅用于开发；部署前请改掉它，或者使用
只允许密钥的 `sshd.service`。

`units/shell.service` 是替代方案：它完全跳过 `login`，所以直接得到 `~ #` 提示符，
不用输用户名。两个单元都要 `ttyAMA0`，一个控制台上跑两个进程都会出问题，所以它们是
互斥替代而不是依赖——把 `multi-user.target` 里的两行 `Requires=` 换一下即可切换。

在 shell 里执行 `poweroff -f` 会关掉客户机；`exit` 回到启动循环。

退出 qemu：从 qemu monitor 够不到 PID 1，无法给它发 `SIGTERM`，所以用 `Ctrl-A X`
退出。

### 增量构建是怎么工作的

`make run` 天生是增量的：每一步都是一个文件目标，输出已存在且比输入新就跳过。
成功构建之后重跑不会下载、不会编译——它只负责启动。具体来说：

- `build/dl/` 里缓存的源码包会被复用（不重新下载）；
- `linux-6.6.158/arch/arm64/boot/Image` 存在 → 不重建内核；
- `busybox-1.36.1/busybox` 存在 → 不重建 busybox；
- `target/.../release/rinit` 比源码新 → 不重跑 `cargo build`；
- rinit、busybox、`units/` 里的某个文件（验收镜像还包括 `units/demo/`）、或某个
  打包脚本变化时，重新打包 initramfs。

既然这些已经成立，就不需要一个会被忘记传的 `SKIP_BUILD` 开关。

### 持久化设备镜像（`run-disk`）

`make run` 启动的是 initramfs，`/etc`、`/var` 和运行期写入的内容重启都会丢。
`run-disk` 改为构建真实 ext4 根文件系统：

```bash
make ARCH=arm64 run-disk
make ARCH=arm32 run-disk
```

第一次运行会生成 `build/.../out/rinit-<target>.ext4`（至少 64 MiB；默认 32 MiB
swap 文件时约 65 MiB），内容来自同一套 rootfs，加上 `units/device/`、dropbear
和 swap 文件，然后用
`root=/dev/vda rw rootfstype=ext4 init=/sbin/rinit -- --default device.target`
启动内核，不再使用 initramfs。**镜像存在时不会重建**，所以客户机里的修改能跨重启
保留；`make disk-image` 才是显式的破坏性重建。

`device.target` 会启动：

- `hostname.service` 和 `network-online.service`（必需；后者等 DHCP 租约，而不是
  只等 `udhcpc` 被 exec）；
- `syslog.service` / `klogd.service`（持久化 `/var/log/messages`）；
- `seedrng.service`（持久化 `/var/lib/seedrng`）；
- `time-sync.service`（busybox `ntpd`）；
- `mdev.service`（busybox 热插拔守护进程；启动时已有的节点仍由 devtmpfs 创建）；
- `swap.service`（启用镜像内的 `/swapfile`）；
- `sshd.service`（dropbear）。

在任何 unit 启动前，rinit 的第二波挂载会读取 `/etc/fstab`；镜像在这里把
`devpts` 挂到 `/dev/pts`。这里使用默认 devpts 实例，`/dev/ptmx` 仍是 devtmpfs
节点，不需要创建符号链接。

镜像还会安装 `/etc/mdev.conf`。mdev 在未配置时的默认权限是 `0660` root:root，
初始扫描后会把 `/dev/null` 等节点收窄；这份配置保证标准字符设备仍可供非 root
进程使用。

`make disk-image` 会创建 32 MiB 的 `/swapfile` 并执行 `mkswap`，`/etc/fstab`
用 `/swapfile none swap sw 0 0` 声明它。开机时由 `swap.service` 执行
`swapon -a`，也就是标准的 fstab 消费者；rinit 会解析这条记录，但不会把它当
挂载点处理。可以用 `make SWAP_SIZE_MB=64 disk-image` 修改大小；`run-disk` 复用的
旧镜像不会自动更新 swap 文件，必须等下一次破坏性重建。

Dropbear 只允许密钥登录（`-s`）。qemu 把宿主机 2222 转发到客户机 22
（可用 `DISK_HOSTFWD` 覆盖），配置好公钥后：

```bash
ssh -p 2222 root@localhost
```

`/root/.ssh/authorized_keys` 初始为空；请通过串口或首次启动前自行加入公钥，镜像
不提供默认凭据。主机密钥在第一次连接时生成到 `/etc/dropbear`，并随 ext4 镜像持久化。
dropbear 源码缓存在 `build/dl/dropbear-2026.94.tar.bz2`；离线环境把这个 tarball
放进去再跑 `make disk-image` 即可。`scripts/build-dropbear.sh` 会应用
`scripts/dropbear-pty.patch`，保证重新解压源码后仍保留 PTY 修复。

交互式 SSH 需要 PTY。镜像的 `/etc/fstab` 会在 unit 启动前把 `devpts` 挂到
`/dev/pts`；缺少这条配置时 Dropbear 会报 `PTY allocation request failed` 并关闭
会话。内核命令行上的 `nofstab` 会禁用整个第二波挂载，所以需要交互式 SSH 的设备
镜像不要使用它。`run-disk` 会复用已有 ext4 镜像，因此在这条 fstab 配置加入之前
构建的镜像，必须先执行 `make disk-image` 重建，再测试交互式 SSH。

镜像构建需要 `e2fsprogs`（`mke2fs -d`）和 `util-linux`（`mkswap`）；内核需要 `CONFIG_EXT4_FS`、
`CONFIG_VIRTIO_BLK`，并且 `CONFIG_DEVTMPFS_MOUNT` 必须关闭（由 rinit 自己挂载
`/dev`，否则真实 rootfs 上第一次挂载会 `EBUSY`）。当前还没有防火墙/nftables
服务：镜像里没有 `nft`/`iptables`，不要把设备直接暴露在不可信网络上。

## 在 qemu 中运行（仅主机 x86 验证）

`make run` 流水线覆盖 arm64 和 arm32。要在构建宿主机上不下载 ARM 内核就验证 rinit
作为 PID 1 的行为，可以构建 musl 二进制并启动 Debian 宿主机内核。这个镜像没有
NIC：`udhcpc` 以非 0 退出，状态机会把 `network.service` 留在 `failed`，但
`multi-user.target` 只用 `Wants=` 拉入它，所以其余启动不受影响：

```bash
cargo build --offline --release --target x86_64-unknown-linux-musl
scripts/build-initramfs.sh x86_64-unknown-linux-musl --busybox-src build/src/busybox-1.36.1 --out /tmp/ir.cpio.gz
scripts/run-qemu.sh --arch x86_64 \
    --kernel /boot/vmlinuz-6.12.111+deb13-amd64 \
    --initrd /tmp/ir.cpio.gz
```

### rinit 命令行

```
rinit [OPTIONS] [-- <SERVICE> [ARGS...]]

  -h, --help                 Show help
  -V, --version              Show version
      --dev                  Skip mounts; do not reboot (host/smoke testing)
      --units <DIR>          Unit directory (default: /etc/rinit/units)
      --default <TARGET>     Boot target (default: multi-user.target)
      --cgroup-root <DIR>    cgroup v2 hierarchy for unit limits
                             (default: /sys/fs/cgroup)
      --watchdog <DEVICE>    Hardware watchdog to arm and feed
                             (default: /dev/watchdog; not opened in --dev)
      --watchdog-timeout <S> Hardware timeout in seconds; fed every half
                             (default: 30)
      --log-file <PATH>      Mirror captured service output to this file
                             (default: /var/log/rinit/log; 'none' disables)
      --fstab <PATH>         Second-wave mount table (plan §1.3(9))
                             (default: /etc/fstab; 'none' disables)
      --test-service <CMD>   Launch this instead of the unit directory
      -- <SERVICE> [ARGS]    Service as an argv (kernel-cmdline form)
```

没有 `--test-service` 时，rinit 启动单元目录。`--test-service`（以及环境变量
`RINIT_TEST_SERVICE`）是绕过它的 bring-up 逃生门；客户机从不设置它。

`--cgroup-root` 是为把层次结构挂在别处的系统准备的，同时也是宿主测试用的接缝：
指向一个普通文件目录时，整条 §7.3 路径——创建、写入、放置、读回——不需要 root、
不需要 `CAP_SYS_ADMIN`、不需要 `/sys` 就能跑。`--dev` 不碰机器自己的层次结构：
挂载被跳过时限额也被跳过（会打一行日志），因为开发宿主机的 `/sys/fs/cgroup` 不归
rinit 划分。显式给出 `--cgroup-root` 时以它为准。

`--watchdog` 和 `--watchdog-timeout` 是 §7.5，`--dev` 拒绝默认设备的原因比"图方便"
更硬：打开 `/dev/watchdog` 会启动硬件定时器，而跑 `make check` 的宿主机不会去喂它。
显式指名的设备仍然会被采纳，宿主测试正是用这种方式对文件演练字节协议。在内核命令行上
这两个参数要用空格分隔的形式——内核会把 `-flag=value` 形式的 token 作为*环境变量*
交给 init，而不是参数（`init/main.c`），所以 `--watchdog-timeout 10` 能到 rinit，
`--watchdog-timeout=10` 不能。
### rctl

```
rctl <COMMAND> [ARGS]

  list [--format FMT]     List all known units (default: table)
  status [UNIT] [--tail N] [--format FMT]
                          Show unit status (default: systemd)
  start <UNIT>            Start a unit
  stop <UNIT>             Stop a unit
  restart <UNIT>          Restart a unit
  reset-failed <UNIT>     Clear a unit's failed/start-limit state
  log [UNIT] [--tail N]   Show buffered log
  reload                  Re-read the unit directory and apply the difference

Exit codes: 0 success  1 error  2 usage  3 no such unit  4 operation conflict
```

`rctl status` 默认输出 systemd 风格的面板。`--plain` 保留稳定的
`unit:`/`state:`/`target:` key-value 形式，供脚本使用；`--short` 是单行；
`--json` 输出机器可读 JSON；`--card` 输出带边框的交互式视图。详情中包含主进程的
内存、累计 CPU 时间和任务数（单元有自己的 cgroup 时读 cgroup，否则读
`/proc/<pid>`）：`rctl status --plain sshd.service` 会显示 `memory:`、`cpu:` 和
`tasks:`。

`rctl list` 默认输出带 `MEM` 和 `CPU` 列的对齐表格。`--plain` 保留旧的
`unit: state pid` 行，`--short` 为紧凑单行，`--json` 输出对象数组。

默认的 systemd 视图类似：

```text
● sshd.service - Dropbear SSH server
     Type: service (simple)
   Active: active (pid 123)
   Target: started
 Main PID: 123
   Memory: 1.1 MiB
      CPU: 290.0 ms
    Tasks: 1
 Restarts: 0
  Restart: on-failure (2s)
     User: root
  Command: /usr/sbin/dropbear -F -R -s -p 22
```

rinit 对**自己的**控制台输出限流，按调用点：每 5 秒 40 行，超出后统计丢弃了多少，
并以下一行报告 `N message(s) suppressed at <file>:<line> (rate limit)`——在下一个窗口，
或者如果洪峰先停了，就在关闭路径上报告。崩溃循环的服务每秒重启几百次，反复打印同样
几条只有 pid 不同的消息，所以限流器按调用点而不是按文本做键；最大的合法突发（一个
启动层启动它的 20 个单元，同一个调用点）远在预算之内。服务输出不受影响——日志环和
落盘镜像已经按单元、按大小给它设了界。

`--tail N` 返回日志环里最后 N 行，`0` 表示缓冲区里全部。日志环为每个单元保留有界的
字节数（`LINE_OVERHEAD` 也计入这份账，这样单元名不会让预算说谎），协议帧有 64 KiB
上限（`MAX_FRAME_BYTES`），正是它阻止任一侧的长度前缀变成无界分配。

`reload`（计划 §7.6）是唯一一个针对目录而不是单个单元的命令，所以它不接受参数，
回复是一份报告：注册了什么、什么立即采用了新定义、什么在等下次重启、什么被停止并
删除，以及各项计数。rinit 无法解析的目录——解析错误、循环依赖、`Requires=` 指向
不存在的单元——会得到一条说明"运行中的配置未改变"的错误，而且确实没变：在新的调度
整体成型之前，什么都不碰（见 `crates/rinit/src/reload.rs`）。

## 设计决策与对计划的有意偏离

有二十处与书面计划不同，或把计划留白的地方钉死。都记录在这里，以免被误认为漂移：

1. **信号用 self-pipe，不用 `tokio::signal`。** 计划 §2.1④ 要求统一到
   `tokio::signal`。但在验证环境里，tokio 的全局信号驱动从不会唤醒 reactor，而
   `sigaction` handler 写入非阻塞 `pipe2` + `AsyncFd` 则稳定可用。self-pipe 是
   init 系统的经典模式，也把每一行 `unsafe` 都留在经过审计的 `rinit-runtime` 层里。
   见 `crates/rinit-runtime/src/signals.rs`。
2. **挂载 `devtmpfs` 之前先 `mkdir -p` `/dev`。** 计划 §1.3(9) 允许跳过，但
   `mount(2)` 要求目标存在，所以创建它严格来说更稳。
3. **挂载序列用 `MountOps` trait 表达。** 计划 §1.3(9) 描述了顺序和致命/可降级的
   划分，但没说怎么测。直接对着 `mount(2)` 写，会让整套策略在没有 `CAP_SYS_ADMIN`
   时无法测试；放到 trait 后面，`first_wave_mounts_with` 就能在宿主机上对着会记录的
   假实现运行。这与计划 §4.2 要求的形态一致。
4. **不用 `initgroups(3)`。** 计划 §7.4 提到了它，但它会走 glibc 的 NSS，在运行时
   加载模块——这正是 §7.4 在别处对 `getpwnam`/`getgrnam` 路径明令禁止的，在静态
   二进制里尤其糟。附加组改为在 `rinit_core::users` 里从 `/etc/group` 计算，再用
   `setgroups(2)` 应用。结果相同，没有动态加载器，而且可测。
5. **capability 步骤围绕 `setuid` 拆开。** 计划 §7.4 列出的是一次 `capset()`，
   在 `setgid` 之前同时设置 bounding 和 ambient 集合。照字面用不了：
   `security/commoncap.c` 里的 `cap_emulate_setxuid` 会在每次 root→非 root 的
   `setuid` 时清空 ambient 集合，无论 `PR_SET_KEEPCAPS` 与否，所以在那里提升的
   ambient capability 到 `exec` 时已经没了。bounding 那一半仍然提前运行，这正是
   §7.4 给出的理由（"`setuid` 之后可能已经没有 `CAP_SETPCAP`"）所针对的；ambient
   提升放在 uid 切换之后。顺序是数据（`SandboxStep`），所以可以用单元测试覆盖，
   而不是只有真实系统上的 root 才能检查。
6. **空的 `CapabilityBoundingSet=` 是一个步骤，不是"没有这一步"。** 它是 §7.4
   提供的最严格设置，实现为"丢弃每一个 capability"；对保持 root 的服务来说，这会让
   它什么都不剩（`handle_privileged_root` 把 root 进程 `exec` 后的集合构造为
   `bset | inheritable`）。
7. **未知名字一律让单元失败。** 未知的 `User=`、`Group=` 或 capability 名字：
   不启动，没有默认值。另一种选择——以 root 运行，或带着悄悄缩小的 capability
   集合运行——会把一个拼写错误变成安全变更，而且要到很久以后、离单元文件很远的地方
   才暴露。
8. **initramfs 镜像的根属主是 root 且可穿行。** 归档用 `cpio --owner=0:0` 构建，
   根目录项是 `0755`，因为那个条目会变成镜像的 `/` 权限。第一版是从一个属主为构建
   用户、权限 `0700` 的工作目录手工打包的，结果客户机里每次 `exec` 都 `EACCES`——
   一个宿主测试看不见的权限 bug。
9. **`--dev` 无法施加降权，所以它让单元失败。** 非特权 rinit 从 `setgid`、
   `setgroups`、`PR_CAPBSET_DROP` 得到 `EPERM`；跳过降权继续跑恰恰是 §7.4 要防止的
   失败。这也是 §7.4 的成功路径只能在客户机里证明、以及那些测试会打印它们所断言的
   日志的原因。
10. **子进程自己把自己放进 cgroup，而不是父进程。** 计划 §7.3 说"先 fork，父进程
    （仍是 root）把子进程 PID 写入 `cgroup.procs`，子进程再降权"。父进程无法在正确的
    时机做这件事：`spawn_sandboxed` 要等子进程 `exec` 之后才返回（子进程里失败的
    `setuid` 正是通过报告管道回传的），所以父进程侧的写入会落在服务已经开始运行之后
    ——而在这段窗口里 fork 的服务会把一个进程放进 rinit 的 cgroup，落在自己的限额
    之外。子进程把自己的 pid 作为第一个动作写入（在 `setsid`、描述符整理和 `chdir`
    之后，任何 §7.4 降权之前），此时它仍是 root，仍拥有 `cgroup.procs`。顺序与计划
    相同，只是换了一个进程；见 `cgroup::join_from_child`。
11. **`HealthCheck=` 探针、`ExecStop=` 命令以及 `ExecStartPre=`/`ExecStartPost=`
    在单元的 cgroup 之外运行。** 它们是 rinit 按 pid 跟踪的独立进程，让它们加入会让
    单元 cgroup 在它们运行期间看起来是"有人"的——而 §7.3 的 `populated=0` 规则正是
    把它读作"保留这个目录"。
12. **内核无法表达的限额被跳过，而不是致命。** §7.3 列了一种降级情形（完全没有
    层次结构）。另外两种是同一类：内核没编译进去的控制器（`CONFIG_CGROUP_PIDS` 关闭，
    因此没有 `pids.max`），以及控制器存在但属性缺失——`cpu.max` 只在
    `CONFIG_CFS_BANDWIDTH` 下构建，内核会对它返回 `ENOENT`。两者都只警告、跳过该
    限额、让单元继续运行：为某个内核写的单元文件必须能在另一个内核上继续工作。内核
    *拒绝*的写入——任何其他 errno，意味着属性在但值不被接受——仍然让启动失败，因为
    那是单元文件声称了一条服务实际没有的界限。
13. **内核配置是构建的一部分。** §7.3 把这件事交给 M5（"否则 M5 需检查内核
    config"）：Makefile 的 `.config` 规则在 `defconfig` 之后打开
    `CONFIG_CFS_BANDWIDTH`，然后*检查*四条指令依赖的四个符号（`CGROUP_SCHED`、
    `CFS_BANDWIDTH`、`MEMCG`、`CGROUP_PIDS`）。必须检查，因为 `olddefconfig` 会
    静默丢掉依赖不满足的符号，否则缺失的 `cpu.max` 会表现为一次"限额不存在"的客户机
    启动。
14. **OOM kill 是被检测出来的，不是推断的，且 `Restart=` 仍然说了算。** §7.3 的
    "检测到 OOM kill 后进入重启策略"被理解为*诊断*加上既有策略，而不是一套独立策略：
    rinit 读 `memory.events` 的 `oom_kill` 计数，指出 `SIGKILL` 背后的内存限额，
    然后单元的 `Restart=` 做它该做的事。对每个被 cgroup OOM killer 杀掉的单元强制
    重启，会覆盖 `Restart=no`，而那是单元作者的明确决定。计数是与服务还不可能分配
    任何内存之前读到的值比较的，所以复用保留下来的 cgroup 目录的重启不会被误读成一次
    新的 kill。
15. **不支持 `PermissionsStartOnly=` 以及 `+`/`-`/`!` 命令前缀**，§7.4 把这列为
    "要记录的决定"而不是"要补的缺口"。每条 `Exec*` 命令——`ExecStart`、
    `ExecStartPre`、`ExecStartPost`、`ExecStop`、`HealthCheck`——都以
    `User=`/`Group=` 运行，所以准备步骤无法悄悄以 root 运行：`run_aux_command`
    解析的是服务得到的同一个沙箱。真正需要 root 的工作应放进系统 oneshot
    （`rinit-init.service`），而不是 `ExecStartPre=`。
16. **看门狗用命令行标志配置，不用 `/etc/rinit.conf`。** §7.5 提供"编译期或
    `/etc/rinit.conf` 全局开关"，并展示了一个 `[Global]` 段。rinit 没有全局配置
    文件，其他每个启动旋钮都是标志，所以 `--watchdog`/`--watchdog-timeout` 通过
    已有机制携带同样两个值（`WatchdogDevice=`、`WatchdogTimeoutSec=`）。配置文件会
    多出一个解析器、第二种启动期失败模式，以及找同一个设置的第二处地方。
17. **看门狗在监管循环启动时武装，而不是在启动流程最前面。** §7.5 说"启动时打开"，
    随后要求喂狗发生在主循环的健康检查点。更早武装意味着启动序列也要喂狗，而一个合法
    地超过超时的层级——带很长 `TimeoutStartSec=` 的服务、慢文件系统——会让一台正在
    正确启动的机器复位。留下的未覆盖窗口（挂载、加载单元、第一次启动）由启动超时界定；
    之后的循环才是 init 可能悄悄卡死的地方。`--test-service` 启动是 bring-up 路径而
    不是系统启动，不武装任何东西。
18. **客户机的看门狗是*给*它的，因为目标 qemu 机器本身没有。** `scripts/run-qemu.sh`
    里的 `-device i6300esb`，加上 Makefile 构建的内核里的 `CONFIG_I6300ESB_WDT=y`，
    让 `/dev/watchdog` 成为真实驱动后面的真实设备，于是客户机从
    `/sys/class/watchdog/watchdog0/timeout` 读回的超时是驱动对 rinit ioctl 的回答，
    而不是替身文件的。`CONFIG_WATCHDOG_SYSFS` 同理：没有它就没有可读取答案的属性。
    x86_64 qemu 路径启动宿主机的 `vmlinuz`，其看门狗驱动是个没进那个 initramfs 的
    模块，所以它没有设备，rinit 什么都不报告——行为正确，但不是证据。
19. **reload 不级联。** §7.6 说被删除的单元要停止并移除，但没说依赖它的单元怎么办
    ——所以一次 reload 如果删掉了另一个单元仍 `Requires=` 的单元，会被*拒绝*
    （引用悬空，且引用完整性在任何应用之前就检查，计划 §1.3(1)），而不是留下一个
    失去依赖却在运行的依赖者。因此删除单元意味着在同一次 reload 里删掉对它的引用，
    这是运维者唯一需要记住的事；错误会点名那个单元。"removed"的另一半是墓碑而不是
    删除：它请求的停止还在途中时运行时必须留着（`SIGKILL` 截止时间和回收需要它），
    所以它立即从所有协议回复里隐藏，由下一次 reload 发现它仍然缺失且已经停止后再丢弃。
20. **`forking` 单元的 `PIDFile=` 只读一次，且守护进程必须在该单元的 cgroup 里
    ——或者已经在里面。** 计划说"主进程退出（码 0）且能定位到守护进程 → 进入
    `Active`"，rinit 照字面执行：主进程被回收的那一刻读文件，之后再写文件的守护进程
    会让启动失败。systemd 会轮询几秒；这里不轮询，所以一个在父进程退出时还没写 pid
    的传统守护进程需要一个替它写的 wrapper。计划给出的顺序被保留——先文件，cgroup
    作为兜底——这也是为什么没有 `PIDFile=` 的 `forking` 单元需要 `Cgroup*=` 指令才能
    被定位，并且会 fail closed（"no PIDFile= and no cgroup of its own"）而不是从进程
    表里猜。还有两个较小的后果记在这里，因为代码就是这么做的：文件指名的 pid 如果
    *不在*单元 cgroup 里，会被移进去（这就是计划里的"写入对应 cgroup"，也正是它让
    单元的限额、停止信号和清理作用到守护进程），以及停止路径先向单元的进程*组*发信号、
    再向 pid 本身发信号——守护进程从 fork 它的进程继承了进程组，所以按组发信号可能
    指向一个已经不存在的组，而守护进程还活着。

## 测量（计划 §0.1 校准点）

去符号的 `release` 二进制（`panic="abort"`、`lto=true`、`codegen-units=1`），
字节数就是 `ls -l` 报出的数：

| 目标                                 | 二进制              | 大小                    | 在镜像里 |
| ------------------------------------ | ------------------- | ----------------------- | ----------- |
| `aarch64-unknown-linux-gnu`（静态）  | `rinit`             | 1.57 MiB (1 649 744 B)  | 是         |
| `armv7-unknown-linux-gnueabihf`（静态） | `rinit`          | 1.26 MiB (1 317 252 B)  | 是         |
| `x86_64-unknown-linux-musl`（静态）  | `rinit`             | 1.05 MiB (1 098 688 B)  | 是         |

`rctl` 在 aarch64 上是 994 152 B，在 armv7 上是 727 256 B，在 musl 上是
406 200 B；都在 initramfs 里。`make size-check` 是门禁，它按计划 §0.1 的 4 MiB
静态预算检查所选 ARCH 的两个二进制。
musl 的数字来自手工构建（`cargo build --release --target
x86_64-unknown-linux-musl`），这也解释了那个目标为什么坏了一段时间却没人发现：
`ioctl` 的请求参数在 glibc 里是 `unsigned long`、在 musl 里是 `int`，所以四个调用点
在那里编译不过，而本表的 musl 行比这次损坏还旧。要把第二个目标纳入门禁，需要构建
宿主机可能没有的工具链。

M0 的 bring-up 辅助程序 `rinit-testservice` 已经删除：没有东西再运行它，它也不再随
镜像发布，所以 §7.9 的审计把它删掉，而不是让每次构建都多背一兆字节的死二进制。
那个步骤现在的替代是 `--test-service`，它通过与单元相同的 spawn 路径运行命令。

打包后的镜像（`build/out/initramfs-aarch64-unknown-linux-gnu.cpio.gz` 和
`…-demo.cpio.gz`：静态 busybox + rinit + rctl + 单元目录 + `/etc`/`/var` 骨架）
gzip 后分别是 2.31 MiB（2 419 117 B）和 2.31 MiB（2 423 284 B）；arm32 的两个镜像
分别是 2.04 MiB（2 136 598 B）和 2.04 MiB（2 143 555 B）。`run-disk` 的镜像是
至少 64 MiB 的 ext4（首次启动前实际使用约 38-40 MiB，其中默认 swap 文件占
32 MiB），不放进 initramfs。差值就是 `units/demo/`
里的十四个文件，只有第二个镜像带它们。打包是可复现的——`scripts/mkinitramfs.sh` 把归档
本来会记录的 mtime 和 inode 号归零，所以同样的二进制重复打包得到同样的字节，这才让
这个数字可核对，而不只是"重新测了一遍"。例外是*强制*重建 busybox（`make -B`）：
busybox 自己的构建不是字节级可复现的，镜像会随之变动几个字节。

两个二进制都在计划 §0.1 的预算内：目标是每个可执行文件 ≤2 MB，静态构建最多允许
4 MB。

计划 §0.1 的内存列由 `make test-system` 在客户机里从 `/proc/1/status` 测量：
128M 机器上启动时 **1 632 kB**、跑 20 个服务时 **1 872 kB**，断言阈值为 ≤2 MB 和
≤6 MB。soak 监视的宿主进程读数更高——约 5 MB——因为它是同一份代码未优化的 `--dev`
构建。

## 下一步

计划的扩展清单已经完成——§7.7 的模板、§7.8 的 socket 依赖、§7.9 的激活和 §7.11 的
终端指令是最后四项——计划 §6 的清单在[验收](#验收计划-6-的清单)一节逐条回答。

`rinit-开发计划.md` 是冻结的规格，刻意不再编辑：它的复选框保持原样，本 README 是
唯一的状态来源。读计划之前有一件事值得知道：它的 §2.2 范围表仍把 socket 激活和模板
单元标为 ❌"不做"，那是计划最初的边界——两者都在扩展清单里（§7.9、§7.7）且都已
实现。那张表记录的是项目从哪里出发，而不是到哪里结束；后者记录在本 README 里。

剩下的不是功能：72 小时 soak（部署期跑一次 `make soak`），以及随着代码树变化重新
测量覆盖率和体积数字，而不是只测一次——现在 `make gates` 就在做这件事。
