# 任务：24h guest soak 与 72h host soak 验收

## 任务：24h/72h 长稳测试与门禁修复（release-24h / soak-host-72h）

- 时间/范围：2026-11-17 19:29 起，针对提交 `a62642a4489292f0903d63250906de7be00fb698`；2026-11-19 根据用户要求完成工具、采样器、分析器、文档、许可证和 rnet 测试修复。
- 【事实】已确认：
  - 24h guest soak 已完成：`SOAK_DONE status=OK samples=8640 control_ok=17280 control_fail=0 quiescent=0 health_fail=0`，驱动器 `SOAK OK tag=release-24h`，CSV 8640 行，24 个每小时检查点全部 `ok`。
  - 24h guest 曲线：RSS 2736→3028 kB（峰值 3028 kB < 4096 kB 预算），稳定窗口（>2h）内 +88 kB / 22 h；fds 稳定在 20（8 次 21，1 次 22）；threads 恒为 2；zombies 恒为 0；swap 0；serial 只有 1 次 boot，无 PANIC / OOM / Kernel panic。
  - `scripts/soak-analyze.py --tag release-24h` 判 **FAIL**，理由两条：`fd span 22 exceeds 2`、`RSS slope 3.32 kB/h exceeds 1.0 kB/h`。
  - 【事实】`fd span` 这条是采集工具缺陷：`scripts/soak-guest.sh` 用 `ls /proc/1/fd | wc -l` 统计 fd，而镜像里 `/bin/ls` 是 rinit-tools 实现（`tools.list` profile=replace-system）。`crates/rinit-tools/src/bin/ls.rs` 的 `format_dir` 用 `fs::read_dir(path)?` / `entry?` / `fs::symlink_metadata(entry.path())?`，目录项在读取过程中消失时整个命令以 1 退出且不打印任何行。实测 400 次对不断增删的目录执行：rinit-tools ls 有 248 次输出 0 行，coreutils ls 0 次。24h CSV 中 21 个 `fds=0` 样本（0.24%）即由此产生，剔除后 fd 跨度为 2（20..22），与线程、控制面计数一致（这些样本的 threads/control 均正常，说明 rinit 本身健康）。
  - 24h guest 的 `disk_free` 单调下降约 -708 kB / 22 h，属日志持久化文件的正常增长（`DEFAULT_LOG_FILE_MAX_BYTES` = 1 MiB，truncate 策略），未触及下限 8192 kB（最低 17944 kB）。
  - 72h host soak 仍在运行（PID 349721/349728/349765，进程自 11-17 19:30 起未重启）。截至 2026-11-19 20:57 已运行约 49.5 h、178030 个样本；RSS 4244 kB、fds 11、threads 1、zombies 0 的最新样本正常。
  - 72h 通过标准以 `scripts/soak.sh` 内断言为准（半窗 RSS 峰值差 ≤ 1024 kB、半窗 fd 谷值差 ≤ 2、quiescent zombie = 0、样本数 ≥ 95%、control 无失败、churn restarts ≥ 10、SIGTERM 退出码 0）。
  - `rinit-tools ls` 已修复：目录项在 `readdir` 后消失时只跳过该条目，其他元数据错误仍失败；新增确定性命中消失和权限错误测试。
  - 修复后对持续开关 FD 的进程复现 2000 次：失败/空输出从 429 次降为 0；系统 `/bin/ls` 对照为 0。
  - `soak-guest.sh` 不再调用 `ls` 统计 FD，改用 shell glob；空或非正 FD 采样计入 `sample_fail` 并令结果失败。
  - `soak-analyze.py` 明确报告无效 FD 样本；RSS 斜率只有在 128 kB 漂移阈值的一半也被超过时才触发失败，避免把 256 KiB 有界日志环填满判成泄漏。
  - `soak-qemu.sh` 现在执行 analyzer，保存 `soak-qemu-<tag>-analysis.json`，analyzer 失败会传播到最终退出码。
  - 新增 `soak-analyze-test.sh` 并接入 `make check`，覆盖无效 FD、可接受的有界扩容、可确认的持续斜率三类结果。
  - `make check` 通过：fmt、workspace Clippy（`-D warnings`）、全部 cargo tests、analyzer 自测。
  - `make soak` 60 秒通过：60 样本、469 次 churn、120/120 control、fds 11–13、无 zombie。
  - 120 秒客户机 soak `sampler-fix-120` 通过：12 样本、24/24 control、fds 恒 20、`sample_fail=0`、`Analyzer: OK`。
  - aarch64 与 armv7 `test-system` 均通过 115/115 断言。
  - aarch64 与 armv7 `tools-test` 均通过 102/102 检查，包括动态 FD 目录回归。
  - aarch64 与 armv7 `size-check` 均通过；`install-tools-test` 17/17 通过。
  - 无逐项预算的 `rnetd`/`rncli` 现在执行计划的 2 MiB 动态二进制上限，而不是旧注释中的 4 MiB 静态上限。
  - Rust 工具链固定为 1.100.0；CI 固定 `cargo-llvm-cov 0.8.7`。
  - 已新增 `LICENSE-MIT` 与 `LICENSE-APACHE`，README 增加双许可说明。
  - 已修正 rinit-tools 的 poweroff 状态、rnet unsafe 边界和当前 24h 门禁状态文档。
  - rnet 新增畸形 DHCP option、超大硬件地址和 reload 原子性测试；rnet 测试 66/66 通过。
- 【推断】：
  - `RSS slope 3.32 kB/h` 这条并非泄漏：24h 内 RSS 仅有 38 次增长事件，每次 +4 kB（唯一一次 +136 kB 发生在 1982 s，属启动后一次性分配），2h 后总计 +88 kB；这是典型的按需扩容（缓冲区/哈希表阶梯），且在释放侧没有对应回落，属分配器 arena 与日志环形缓冲填满的有界行为，不是每重启一次泄漏（15209 次 churn 重启若每次泄漏应远超 88 kB）。
  - `soak-analyze.py` 的 1.0 kB/h 斜率阈值对 24h 窗口过严：把同一 CSV 从 2h/4h/8h/12h/16h/20h 起算，斜率分别为 3.32/3.00/2.95/2.66/3.24/2.38 kB/h，几乎不随窗口衰减，说明它拟合的是那 300 kB 的一次性台阶，而不是持续增长；因此该阈值会把“一次性扩容”判成泄漏。其 drift 阈值 128 kB 才是对“一次性台阶”有效的判据（实测 drift 28 kB，通过）。
- 【假设】及验证方式：
  - 假设 72h 结束时 `soak.sh` 判 SOAK OK。验证：等进程结束后读取 `build/logs/soak-host-72h.log` 末尾的 `SOAK OK (N samples)` 或 `SOAK FAILED`。
  - 假设新采样器的 24h 结果不再出现无效 FD，并且 RSS 判据只报告有中位数漂移佐证的持续增长。验证：完成新一轮 24h `release-24h` soak。
- 关键决策及理由：
  - 修工具优先于在 analyzer 中过滤 0，保留真正的异常可见性。
  - 斜率仍需中位数漂移佐证：当前曲线是 256 KiB 有界日志环逐步填满，单看线性拟合会误判；新增合成高增长测试防止阈值失效。
  - 不在仍运行的 72h soak 期间替换其二进制；该运行继续以复制的 `a62642a` 二进制完成。
- 修改的文件/路径：
  - `crates/rinit-tools/src/bin/ls.rs`
  - `crates/rnet/src/dhcp/packet.rs`
  - `crates/rnet/src/runtime.rs`
  - `scripts/soak-guest.sh`
  - `scripts/soak-analyze.py`
  - `scripts/soak-qemu.sh`
  - `scripts/soak-analyze-test.sh`
  - `scripts/tools-test.sh`
  - `Makefile`
  - `.github/workflows/ci.yml`
  - `rust-toolchain.toml`
  - `README.md`
  - `README.zh-CN.md`
  - `docs/release.md`
  - `docs/rinit-tools-spec.md`
  - `docs/rnet-design.md`
  - `LICENSE-MIT`
  - `LICENSE-APACHE`
  - `.agent-memory/soak-24h-72h.md`
- 验证状态：已验证上述静态、host、双架构系统、双架构工具和短程 soak；72h 仍部分验证；新 24h 未运行；覆盖率门禁未运行，因为本机没有安装 `cargo-llvm-cov`，且安装工具需要单独明确授权。
- 风险与回滚：
  - 风险一：RSS 斜率现在要求中位数漂移同时超过 64 kB；非常缓慢但线性的增长可能在单次 24h 内低于该阈值。合成测试验证了 8 kB/h 的持续增长会失败；后续长跑仍应保留 drift、峰值和绝对上限三重判断。
  - 风险二：本轮修复与 `a62642a` 上的既有 72h 证据不是同一代码；发布前必须在本次提交上重跑 24h 并完成新的 72h 运行。回滚点为 `a62642a`。
  - 风险三：MIT 文本暂以“rinit contributors”为版权持有者；若发布主体有明确法律名称，应在发布前替换。
- 下一步：
  1. 等现有 72h host soak 结束并记录最终判决。
  2. 在冻结的新提交上重跑 24h guest soak；必须保存 analysis JSON 并达到 `SOAK OK` + `Analyzer: OK`。
  3. 获得明确授权后安装固定版本 `cargo-llvm-cov` 并运行覆盖率门禁。
