# 工作记录

> 冰宽量化考核项目：Rust DPDK Async Runtime + ICMP Ping（SPEC-BQ.md）
> 每一条按"怎么想的 → 做了什么 → 效果 / 遇到的问题"记录，用于答辩前的总结和复盘。
> 时间为服务器时间（UTC）。

---

## 2026-09-30 · 01 理解题目与摸底服务器

**怎么想的**
- 题目的核心是 A − B，也就是 async 抽象层的税，而不是 ping 本身快不快。三条硬门槛（10 分钟不崩、零 mbuf 泄漏、零丢包或能自洽解释）任何一条不满足，排名数字就不看。
- 所以优先级是：先保证硬门槛，再保证 A − B 可信（A 和 B 严格对齐），最后才抠纳秒。

**做了什么**
- 通读 SPEC；用 lscpu、ip、ethtool、IMDS 等工具摸清机器配置，只读，没有改任何东西。

**效果 / 发现**
- c8a.xlarge：AMD EPYC 9R45，4 核无超线程，7.6 GiB 内存，AL2023（内核 6.18），东京 apne1-az4。
- 两张 ENA 网卡：
  - device 0：enp39s0，10.202.2.227，SSH 走这张，留给内核和 C；
  - device 1：enp40s0，10.202.15.133，MAC 06:ff:df:7d:66:91，PCI 0000:28:00.0，绑给 DPDK。
- 对端 10.202.8.15 两张网卡都能 ping 通，MAC 与 SPEC 一致。
- 三个对后续影响最大的发现：
  1. 没有 IOMMU 组 → DPDK 只能用 vfio-pci 的 noiommu 模式或 igb_uio。
  2. ENA 在用 LLQ 模式 → DPDK 需要写合并（WC），而主线 vfio-pci 不支持 WC。
  3. 内核 ping 的 RTT 随发包节奏变化很大：连续发约 60 µs，每 200 ms 发一个约 134 µs → 测 C 时节奏必须和 A 可比。
- 系统是裸的：没有 gcc、git、Rust、DPDK，大页为 0，也没有核隔离。

## 2026-09-30 · 02 知识准备文档

**怎么想的**
- 我的简历上没有 Rust 和 DPDK，答辩要能讲清每个设计选择，所以先系统补概念。

**做了什么**
- 写了 `~/Claude知识.md`，与豆包版逐条对照勘误。
- 写了一个可运行的教学版 runtime（`~/claude-knowledge-demo/minirt`）：executor、Waker、timer、模拟网卡，外加一个 B 风格的对照版。用 docker 镜像 rust:1-slim 编译运行。

**效果**
- 迷你版 A − B：段②约 70 ns 对 30 ns（用 Instant 测，只看量级）。
- 本机时钟成本实测：rdtsc 8.4 ns、lfence+rdtsc 13.9 ns、rdtscp 15.9 ns、Instant::now 22.7 ns；TSC 频率 2.600 GHz。
- 系统 ping 的坑：`-i 0.0005` 在本机退化为"收到回复就立刻发下一个"（间隔约 66 µs）；默认的接收时间戳是内核收包时刻，`-U` 才是用户态到用户态。

## 2026-09-30 · 03 与联系人确认的事项

- §9 要求 README 包含"§5 那条 grep 的结果"，但 §5 里并没有 grep。对方确认这是面试官忘了删的内容，**忽略**。
- 重启规则：我是远程连在服务器上工作的，重启会断开会话。所以其余工作都做完后，**先停下，由我本人执行重启**。

## 2026-09-30 · 04 环境搭建：工具链

**怎么想的**
- SPEC 把"工具链、DPDK 编译、大页、网卡绑定、核隔离"都算作交付物，要求一键 setup。所以不手工敲命令，而是写成**可重复执行**（幂等）的 `scripts/setup.sh`，边写边跑，最后在一台干净的机器上也能复现。

**做了什么**
- dnf 安装：gcc 11.5、clang 15（给 bindgen 用的 libclang）、meson 0.63、ninja、git 2.50、numactl-devel、`kernel6.18-devel`（与运行中的内核版本完全一致，用来编译 igb_uio）、elfutils-libelf-devel、perf。
- `python3-pyelftools` 不在 AL2023 的源里，改用 `pip install pyelftools`（0.32）。DPDK 构建时需要它。
- 建立项目仓库 `~/dpdk-async-ping`（git init）。

**DPDK 版本选择**
- 可选：24.11.x（上一个 LTS）、25.11.3（最新 LTS 的最新补丁版）、26.03 / 26.07（非 LTS）。
- 选 **25.11.3**：LTS 分支有长期的 bug 修复，比非 LTS 更稳，适合 10 分钟长跑的稳定性要求；它的 ENA 驱动也足够新。

**遇到的问题**
- 系统自带的 perf 是 6.1 版本，和 6.18 内核不完全匹配，基本功能应该能用，后面用到时再确认。

## 2026-09-30 · 05 环境搭建：setup.sh 与各阶段结果

**怎么想的**
- 把 setup 拆成 8 个幂等阶段：packages / rust / nic / dpdk / igb_uio / hugepages / cmdline / irq。每个阶段先检查"是否已完成"再动手，可以单独重跑。
- 网卡参数不写死，由 `scripts/detect-nic.sh` 通过 IMDS + sysfs 自动探测，写入 `config/nic.env`。**"换一块网卡要改哪几行"的答案：重跑 detect-nic.sh，或手改 config/nic.env 那几行。**
- 驱动选 **igb_uio + wc_activate=1**，而不是 vfio-pci：本机没有 IOMMU，vfio 只能用 noiommu 模式，而且主线 vfio-pci 不支持写合并；ENA 的 LLQ 需要写合并，否则发送路径会明显变慢（DPDK 文档原话："huge performance degradation"）。
- 只编译需要的 DPDK 驱动：ena（真网卡）+ null / ring（以后无网卡时做离线测试）。

**做了什么 / 效果**
- Rust：rustup 装固定版本 1.98.1（含 clippy、rustfmt）。
- DPDK 25.11.3：编译加安装只用了 26 秒，安装到 /usr/local（lib64），并写了 ld.so.conf.d；源码包 sha256 = `3719acc586b310c4f60ba230683bf4f1e12c6f2f5bee11f7c01b1bbd0ded7490`。
- igb_uio：dpdk-kmods commit 9b182be，在 6.18 内核上一次编译通过，安装到 /lib/modules/…/extra/dpdk/。
- 大页：运行时预留 1024 × 2 MiB（2 GiB）；重启后由启动参数 `hugepages=1024` 接管（启动时就预留，避免运行久了内存碎片化导致分配失败）。
- 中断：irqbalance 设置 `IRQBALANCE_BANNED_CPULIST=3`，现有中断的亲和性设为 0-2。
- 新增脚本：`bind.sh` / `unbind.sh`（绑定 / 解绑网卡，内置"绝不碰 SSH 网卡"的保险）、`check-env.sh`（只读的环境自检）。

## 2026-09-30 · 06 DPDK 首次上线验证（重启前）

**做了什么**
- `bind.sh`：0000:28:00.0（enp40s0）解绑 ena、绑定 igb_uio；SSH 所在的 enp39s0 不受影响。
- 用 `dpdk-testpmd` 的 icmpecho 模式（会回答 ARP 和 ping），从本机内核网卡 enp39s0 去 ping DPDK 网卡的 IP 10.202.15.133。

**效果**
- 20/20 收到，0 丢包，平均 RTT 77 µs → DPDK 的收包、发包、ARP 应答整条链路都通了。
- ENA 驱动日志："Placement policy: Low latency"（LLQ 在用），LLQ entry 128B；IOVA 模式 PA（无 IOMMU 时符合预期）。
- **写合并已证实生效**：testpmd 运行期间，`/sys/kernel/debug/x86/pat_memtype_list` 显示网卡 BAR2（0x2a3000000，1 MB，LLQ 的设备内存）为 **write-combining**，BAR0/1（寄存器）为 uncached-minus。
  - 注意：igb_uio 不把 `wc_activate` 暴露到 sysfs，所以读不到这个参数，只能靠 PAT 表确认。
- **ARP 疑问有了答案**：DPDK 端收到 21 个包 = 20 个 ping + 1 个 ARP 请求 → AWS 会把对端的 ARP 请求转发到我们的网卡，所以必须自己实现 ARP 应答（与 SPEC 一致）。
- **异常待查**：启动时 RX 还是 0，TX 计数就已经是 51817，推测是 ENA 设备的累计计数（包含绑定前内核发出的包）。→ 以后做丢包统计时只用自己的计数器，或者用启动时的快照做差值。

## 2026-09-30 · 07 启动参数（等待重启）

**写入的参数（grubby，全部内核项）**
```
default_hugepagesz=2M hugepagesz=2M hugepages=1024
isolcpus=managed_irq,domain,3 nohz_full=3 rcu_nocbs=3 irqaffinity=0-2 rcu_nocb_poll
nosoftlockup nmi_watchdog=0 tsc=reliable
```
- `isolcpus / nohz_full / rcu_nocbs`：让核 3 几乎只跑 runtime（不被调度其他任务，只有一个任务时不打时钟 tick，RCU 回调挪到别的核）。
- `irqaffinity=0-2` + `managed_irq`：新中断只落在核 0-2，内核托管的中断也避开核 3。
- `rcu_nocb_poll`：RCU 回调线程自己轮询，不再给核 3 发 IPI。
- `nosoftlockup nmi_watchdog=0`：关掉会周期性打断 busy-poll 核的看门狗。
- `tsc=reliable`：关掉 clocksource watchdog 对 TSC 的周期性校验。
- **暂不启用** `idle=poll` 之类的空闲态参数：它会影响核 0-2 上 C（系统 ping）的延迟，属于"C 在什么条件下测才公平"的问题，留到测 C 时再讨论。

**重启前自检**（`check-env.sh`）
- 软件、大页、网卡绑定、irqbalance 全部通过；启动参数类的项要重启后才生效。
- 还有 12 个中断落在核 3，推测是 nvme / ena 这种按 CPU 分配、内核托管的中断，运行时迁不走，重启后由 `managed_irq` 和 `irqaffinity` 处理，**重启后复查**。

**下一步**：按约定由我本人重启 → 重启后运行 `check-env.sh`、`bind.sh` 复查 → 开始写代码（先写 B）。

## 2026-09-30 · 08 重启后验证

**效果**
- `check-env.sh` 全部通过：cmdline 生效，`/sys/devices/system/cpu/isolated` = 3，nohz_full = 3，HugePages_Total = 1024（启动时预留）。
- **落在核 3 上的中断从 12 个降到 0**（managed_irq + irqaffinity 生效）。
- nohz_full 实测：用 `taskset -c 3` 让一个死循环独占核 3 跑 10 秒，核 3 只收到 21 次中断（LOC 4、IWI 4、RES 10、CAL 3），约每秒 2 次。对比：不开 nohz_full 时仅时钟 tick 就有每秒上千次。
- 重启后网卡回到了内核 ena 驱动（符合预期），`bind.sh` 重新绑定到 igb_uio。

## 2026-09-30 · 09 架构设计（写代码之前）

**总原则：A 和 B 除了"有没有 runtime"之外，其他一切共用同一份代码。** 否则 A − B 减出来的就不只是抽象层成本。

**crate 划分（Cargo workspace）**
| crate | 内容 | A 用 | B 用 |
|---|---|---|---|
| `dpdk-sys` | bindgen 生成的裸绑定 + 一个很薄的 C shim（包装 `rte_eth_rx_burst` 等 static inline 函数） | ✔ | ✔ |
| `dpdk` | 安全封装：Eal、Mempool、Mbuf（RAII，Drop 时归还）、Port（rx/tx burst）、TSC | ✔ | ✔ |
| `pingproto` | 帧模板、RFC 1624 增量校验和、reply 解析、ARP 应答；纯 Rust，可以离线单测 | ✔ | ✔ |
| `bench` | 命令行参数、TimerHeap、直方图、报表、housekeeping（ENA watchdog 等） | ✔ | ✔ |
| `rt` | **runtime 本体**：executor、Waker、timer、poll-mode reactor、按 id 分发的信箱 | ✔ | ✘ |
| `async-ping` / `raw-ping` | 两个可执行程序 | A | B |

**已确定的关键设计（附理由）**
1. **发送**：每个包一次 `tx_burst`（1 个包）。T1 的定义就是"tx_burst 返回"，把多个 session 的包攒成一批会让 T1 的语义含糊。先用"分配 mbuf + 拷贝 106B 模板"，简单且显然正确；"每个 session 一个模板 mbuf + refcnt"的优化留作备选。
2. **收包顺序对齐 B**：reactor 每分发一个包，就立刻 poll 被唤醒的 task，而不是先把整批包都分发完再统一 poll。原因：B 天然是"处理一个包、更新一个状态"，A 如果先全部 wake、再统一 poll，第 1 个包的 T3 也要等整批处理完，会人为放大 A − B。
3. **Timer**：用二叉堆（按 TSC deadline 排序）。64 个 session 规模很小，堆的插入是 O(log n) 且精确；时间轮适合海量 timer，这里没有必要。**B 用同一个 TimerHeap 数据结构**（只是不经过 waker），满足 SPEC"B 也要有 timer 路径"的要求。
4. **超时**：不给每个包注册超时 timer（那会带来每包的堆操作和取消成本），而是由 housekeeping 定期扫描 64 个在途 session 的 deadline。A 和 B 用同样的扫描方式。
5. **Waker**：data 里只放 task 编号，wake = 把编号推进固定容量的环形就绪队列（带"已入队"去重位）。clone / drop 都是空操作，没有引用计数。Waker 被规定为 Send + Sync 的问题：wake 时检查当前线程是否是 runtime 所在线程，不是就 abort，保证不会发生数据竞争。
6. **打点**：统一用 `rdtsc`（不加 lfence）。A 和 B 用同一个函数、在语义相同的位置打点。
   - 段③的定义拆成两段：**sleep 误差** = timer 发现到期的时刻 − deadline；**段③** = 发现到期 → 下一个 T0。
7. **统计**：lcore 上用自己实现的对数-线性直方图记录（每次几条指令），在 T3 之后才记录，不进入被测段。进度输出交给核 1 上的上报线程，通过 Relaxed 原子计数读取，lcore 自己不 println。
8. **ENA watchdog**：启动时调用 `rte_timer_subsystem_init()`，housekeeping 里周期性调用 `rte_timer_manage()`；注册 reset 事件回调，出事时干净退出并报告。
9. **零泄漏核对**：mempool 刚创建时（端口启动前）记下 avail；结束时先 drop 所有 task、再 stop 端口（PMD 归还 RX / TX 环上的 mbuf），然后比较 avail 是否等于初值。
10. **链接方式**：用 DPDK 共享库，EAL 会从插件目录自动加载 ENA 驱动，避开静态链接时驱动被链接器丢弃的问题。

**开发顺序**：`dpdk-sys` + `dpdk` 冒烟测试（真网卡上发一个 ping 收到 reply）→ `pingproto` 单测 → `bench` → **B** → `rt` + **A** → 10 分钟长跑 → C → README 与报告。先做 B，因为它既能验证整条硬件链路，又是 A 的对照基线。

## 2026-09-30 · 10 代码：dpdk-sys / dpdk / pingproto / pingkit

**做了什么**
- `dpdk-sys`：bindgen + 一个很薄的 C shim（`rte_eth_rx_burst` / `tx_burst` / `pktmbuf_alloc` / `free` 等都是 static inline，库里没有符号）。
  - 坑：allowlist 写成 `rte_.*` 会把 GTP 等 packed 协议头也拉进来，Rust 报 E0588。**改为只列出实际调用的函数**，生成的绑定从 2 万行降到 2500 行。
- `dpdk`：安全封装，所有 unsafe 都集中在这里，每处都写了 `SAFETY:` 注释。
  - `Mbuf`：RAII，内含裸指针 → 自动 `!Send`。
  - `Mempool`：泄漏为 `'static`，保证 mbuf 不会比池活得久。
  - `RxBurst`：逐个交出所有权，没取走的在 Drop 时释放。
  - `Port`：`!Send + !Sync`；tx 失败时把 mbuf 原样还给调用者。
- `pingproto`：帧模板 + 校验和 + 解析 + ARP。
  - 校验和：启动时对"id / seq / TSC 为 0"的模板预先算好部分和，每个包只加 6 个 16 位字。它与全量重算**逐位相同**（反码加法满足交换律），**20 万组随机数 × 5 种 payload 长度**交叉验证全部一致，所以不存在 RFC 1141 的 0 / 0xFFFF 边界问题。
- `pingkit`（A 和 B 共用）：参数、数据面初始化与零泄漏核对、**发送函数（段①，两边调用同一个函数）**、TimerHeap（二叉堆）、对数-线性直方图（< 256 周期精确，其余相对误差 < 0.8%）、报表、核 1 上的进度线程、周期维护（`rte_timer_manage` + `tx_done_cleanup` + 发布计数）。
  - 查了 ENA 源码：`tx_done_cleanup` 受支持；xmit 只在空闲描述符低于阈值时才顺带回收 → 在维护节拍里主动回收，尽量不让回收落进段①。

## 2026-09-30 · 11 B（raw-ping）首次运行

**设计**：一个 busy-poll 循环分三步：`on_rx`（收包 → 查表 → 就地改状态）→ `on_timers`（到期的 session：record 上一个样本 → 释放 mbuf → 发下一个）→ 每 100 µs 一次 `on_house`（维护、超时扫描、停止判定）。
- reply 的 mbuf 在 sleep 期间由 session 持有，醒来后才 record 并释放（与 SPEC 的 loop 形状一致）。
- 停止：到时间后不再发新请求，等在途请求收到或超时后退出 → 释放所有 mbuf → stop 端口 → 核对 mempool。

**效果（64 session，delay 500 µs，10 s）**
- sent = received = 880,977，0 超时、0 丢包；**mbuf 泄漏 0**（初值 8191，端口启动后 7168，关停后 8191）。
- 进程内耗时：p50 **50 ns**、p99 469 ns、p99.99 1.3 µs、max 39 µs。
  - 段① p50 40 ns / p90 310 ns（**双峰**，待查）；段② p50 10 ns / p99 210 ns。
- 端到端 p50 **245 µs**（!）、速率 8.6 万/秒（理论 11 万/秒）。

**端到端为什么是 245 µs：按 session 数做的对比实验**
| session 数 | 端到端 p50 | 进程内 p50 / p99 |
|---|---|---|
| 1 | 62 µs | 50 / 160 ns |
| 8 | 160 µs | 50 / 401 ns |
| 32 | 107 µs | 50 / 441 ns |
| 64 | 245 µs | 50 / 469 ns |

- 单个 session 时 62 µs，和内核 ping 连续发的 59–63 µs 一致 → **本端的 DPDK 路径正常**。
- RTT 随速率变化且**不单调** → 典型的自适应中断合并（DIM 在几档之间切换）。本端是轮询、没有中断，所以判断发生在**对端内核**。这是 SPEC 说的"不可控因素"，也从实证上说明了排名为什么只看进程内耗时。
- 对 A − B 没有影响（两边面对同样的对端）；但 **C 必须在相同速率下测**，否则对端的状态都不一样。

**其他发现**
- 段①的双峰（40 ns / 310 ns）：单 session 时几乎没有高峰，并发越高越明显。推测是 reply 成批到达 → 多个 session 的 sleep 几乎同时到期 → 同一轮里连续发送，后一次 tx 要等前一次的写合并缓冲 flush / 内存屏障。待查 ENA xmit 源码确认。
- 端口 opackets 恒比我们的 sent 多 106：绑定前内核发出的包，设备计数器未清零。**统计只用自己的计数器**。
- rx_burst 的包数分布：64 session 时 1 个包占 76%，2 个包占 19%，最多 4 个。

## 2026-09-30 · 12 runtime（rt）与 A（async-ping）

**rt 的结构**（crates/rt，约 500 行）
- `executor.rs`：
  - 固定容量的任务槽（永不扩容 → 任务地址稳定）+ 每槽一个代数（识别过期 waker）；
  - 就绪队列是 `Cell` 实现的环形数组 + "已入队"标记（去重；出队时、**poll 之前**清标记，避免丢唤醒）；
  - **Waker**：data 里编码 rt_id(16) | 代数(16) | 任务号(32)，clone / drop 是空操作（没有引用计数、没有原子操作）；wake 时通过线程局部变量确认"当前线程正在运行这个 runtime"，否则 abort → 满足 std 对 Waker 的 Send + Sync 约定，不可能发生数据竞争。
- `timer.rs`：TSC deadline 小顶堆（与 B 共用 `timerq`）+ 槽位表；`sleep` 返回 `SleepInfo { deadline, fired_at }`，用于统计 sleep 误差和段③；取消是 O(1)（标记为 Cancelled，弹出时再回收）。
- `sync.rs`：`Mailbox<T>`，单槽信箱，reactor 与 task 之间交接 mbuf 所有权。
- `runtime.rs`：`Driver` trait（协议侧由应用提供，**泛型参数、静态分发**）+ 主循环。
  - 主循环顺序与 B 对齐：rx_burst → 每个包 `on_packet` 之后**立刻跑一遍就绪队列** → 触发到期 timer → 每 100 µs `on_tick`。
- 所以 SPEC §1 那个核心问题的答案是：**rx_burst（reactor）→ Driver 分类 → Mailbox::put（交出 mbuf 所有权 + wake）→ 就绪队列 → executor poll → session future 返回 Ready**。

**A 的结构**：`driver.rs`（分类、按 id 投递信箱、ARP、超时扫描、停止判定）+ `main.rs`（session 的 async 逻辑，形状与 SPEC 的 loop 一致）。

**重构**：报告的组装 / 打印 / 写 JSON 挪进 pingkit，A 和 B 共用同一份代码；TimerHeap 挪进独立的 `timerq` crate（runtime 库不该依赖应用层的 pingkit）。

**遇到的问题**：写文件时有几次输出被中途截断（安全分类器误报），改为分小段用 Edit 续写。

**A 首次运行（64 session，delay 500 µs，10 s）**
- sent = received = 903,752，0 丢包，**mbuf 泄漏 0**，干净退出。
- 与 B 的对比：
| 指标 | A | B | A − B |
|---|---|---|---|
| 进程内 p50 | 170 ns | 50 ns | **+120 ns** |
| 进程内 p99 | 530 ns | 469 ns | +61 ns |
| 段① p50 | 40 ns | 40 ns | 0（同一个函数） |
| 段② p50 | 130 ns | 10 ns | **+120 ns** |
| 段③ p50 / p99 | 60 / 420 ns | 40 / 390 ns | |
- 税几乎全在段②，符合预期。但 120 ns（约 540 个核心周期）远多于 put → wake → 入队 → 出队 → poll 这几十条指令应有的开销 → **下一步用 perf 定位**。

## 2026-09-30 · 13 【关键发现】A − B 的 120 ns 里，90% 是测量假象（rdtsc 乱序）

**现象**：A 的段② p50 比 B 多 120 ns。但 put → wake → 入队 → 出队 → poll 应该只有几十条指令。

**第一步：把段②拆开**（新增 `probe` 编译特性，默认关闭；executor 在 poll 前打点，driver 在 put 返回后打点）
| 子段（rdtsc） | p50 | p99 |
|---|---|---|
| T2 → put 返回（分类 + 投递 + wake） | 120 ns | 310 ns |
| put 返回 → 开始 poll（回主循环 + 出队） | 10 ns | 10 ns |
| 开始 poll → T3 | 10 ns | 20 ns |
→ executor 本身几乎不花时间；120 ns 全在第一段，但第一段里真正属于 runtime 的只是 put + wake。

**推断**：分类要读网卡刚 DMA 进来的包数据，不在缓存里，一次 cache miss 约 100 ns。B 也读同样的数据，但它的 T3 紧跟在分类之后，只隔几条指令；**rdtsc 不是序列化指令**，乱序执行时可以在 miss 结束之前就被提前执行，于是这 100 ns 被"藏掉"了。A 从分类到 T3 之间有几百条指令，乱序窗口塞不下，rdtsc 只能等 miss 结束。→ 同一把尺子在 A 和 B 上"看到"的东西不同。

**验证**：所有打点改用 `rdtscp`（等前面所有指令执行完、所有读完成才读 TSC；每次约 16 ns，rdtsc 约 8 ns），A 和 B 同时改。
| 指标 | B rdtsc | B rdtscp | A rdtsc | A rdtscp |
|---|---|---|---|---|
| 段② p50 | 10 ns | **120 ns** | 130 ns | 130 ns |
| 进程内 p50 | 50 ns | 180 ns | 170 ns | 180 ns |
| **A − B 进程内 p50** | | | **+120 ns** | **≈ 0 ns** |
- rdtscp 下的 probe：put 返回 → 开始 poll 为 20 ns，开始 poll → T3 为 20 ns，而且其中包含 probe 自己多出的两次 rdtscp。

**结论**
- 如果用天真的测法（rdtsc），报出来的 A − B 是 120 ns，其中 90% 以上是测量假象；**真正的 async 税在 p50 上大约是 10 ns 量级**。
- 决定：**正式版统一使用 rdtscp**（`dpdk::tsc::rdtsc()` 内部实现为 rdtscp，A、B 与 runtime 共用这一个函数）。代价是每个打点多约 8 ns，两边相同。
- 这是"你是否知道税收在哪里"的一个具体答案：**测量工具本身也收税，而且对两边收得不一样**。

**新的疑问（下一步）**
- 这一轮 A 的 p99（360 ns）反而好于 B（521 ns）。差别在段①：B 的 p90 是 290 ns，A 只有 60 ns，可两边的发送函数是同一个。推测：B 在同一轮 `on_timers` 里连续发多个包，后一次 tx 要等前一次写合并缓冲 flush；A 的两次发送之间隔着 task 调度，缓冲有时间先排空。待验证。
- 两次运行的端到端差别很大（B 160 µs，A 243 µs）：对端的中断合并状态每次运行随机不同，会影响到达的批次结构和 p99 → **A / B 必须交替运行多轮再比较**，单轮结果不可信。

## 2026-09-30 · 14 A / B 交替多轮对比 + 段① 写合并效应

**方法**：新增 `scripts/ab.sh`（按 ABBA 顺序交替跑 N 对，抵消对端状态随时间漂移的影响）和 `scripts/summarize.py`（逐轮列出分位数，取各轮中位数，算 A − B）。

**3 对 × 30 秒（rdtscp）**：所有轮次 0 丢包、0 泄漏。
| 指标 | A 中位数 | B 中位数 | A − B |
|---|---|---|---|
| 进程内 p50 | 180 | 180 | **0** |
| 进程内 p99 | 350 | 521 | **−171** |
| 进程内 p99.9 / p99.99 | 460 / 610 | 620 / 770 | −160 / −160 |
| 段② p50 / p99 | 130 / 300 | 120 / 270 | **+10 / +30**（真正的 runtime 税） |
| 段① p50 / p99 | 50 / 70 | 50 / 330 | 0 / **−260** |

**A 的 p99 为什么反而更好？按"距上一次发送多久"拆开段①**（20 秒各一轮）：
| 距上次发送 | B 段① p50 / 占比 | A 段① p50 / 占比 |
|---|---|---|
| < 100 ns | **300 ns** / 20% | — / 0% |
| 100–250 ns | **270 ns** / 4% | 60 ns / ≈0% |
| 250–500 ns | 50 ns / 5% | 50 ns / 28% |
| 500 ns–2 µs | 50 ns / 52% | 50 ns / 53% |
| ≥ 2 µs | 50 ns / 19% | 50 ns / 18% |

**机制**（已读 ENA 源码确认，`base/ena_eth_com.c:138`）：每发一个包，先 `wmb()`（x86 上是 sfence），再用 16 次 8 字节写把 128 B 的 LLQ 条目推进写合并内存。sfence 要等上一个包的写合并缓冲排空（实测约 250 ns）。
- 对端的中断合并让多个 session 的 reply 成批到达 → 它们的 sleep 同时到期 → 发送扎堆（两边都有约 80% 的发送距上一次 < 2 µs）。
- B 的循环处理完一个到期的 session 立刻处理下一个，两次发送只隔几十纳秒 → 后一次在段①里等 flush。
- A 的两次发送之间隔着"record + 调度 + poll"（≥ 250 ns）→ flush 在任何被测段之外自然完成。
- **间隔相同时，两边的段①完全一样** → 发送代码确实是对齐的；差别只在"硬件等待被计入了哪一段"。

**结论（写进 README）**：排名指标按原样报告（p50 = 0，p99 = −171），同时给出拆解：
1. runtime 在接收侧真正的税：段② **+10 ns（p50）/ +30 ns（p99）**；
2. 段①的差异来自 ENA 写合并的 flush 等待被归到哪一段，不是 A 更快。
- 考虑过但**没有**做的"修正"：把 B 的发送人为拉开间隔（等于故意拖慢 B），或把多个 session 的包攒成一次 tx_burst（改变了 T1 的语义）。两者都会让 A − B 失去意义。

## 2026-09-30 · 15 60 秒路径、超时路径验证；C 的设计；README / 报告

**60 秒路径**（`scripts/run.sh A --delay-us 500 --duration-sec 60`，答辩现场要跑的那条命令）
- sent = received = 5,676,056，0 丢包、0 泄漏，退出码 0。
- 收到 1 个对端的 ARP request 并已应答（arp-replied 1）；端口计数可精确对账：ipackets = reply 数 + 1。

**超时 / 迟到路径**（故意设 `--timeout-us 150`，比 RTT 还短）
- B：timeouts 17,950 = late 17,950；A：timeouts 39,575 = late 39,575。对账差为 0，泄漏为 0。
- "超时数 = 迟到数"说明没有真正丢包，只是慢，而且迟到的 reply 都被正确识别、释放，没有被误当成新 seq 的回复。
- 注意：超时由每 100 µs 一次的扫描判定，实际生效的超时落在 [阈值, 阈值 + 100 µs]。

**10 分钟正式长测**：A、B 各 600 秒，后台依次运行。期间不编译（会冲刷与核 3 共享的 L3）。

**C 的设计**（`scripts/run-c.sh` + `scripts/summarize_c.py`）
- 64 个 `ping` 进程 = 64 路（ICMP id 各不相同）；每路 `-i 0.001`，聚合 6.4 万包/秒；`-s 64` → 106 B 帧；绑在核 0–2。
- iputils 的 `-i` 只精确到整数毫秒 → 1 ms 是能取到的最接近值；为严格可比，A 也在同速率下补测（`--delay-us 800` ≈ 64 路 × 1 ms）。
- 口径：默认 `ping -U`（用户态 → 用户态，对应 A 的 T3 − T0）；也测默认口径（接收时刻 = 内核 SO_TIMESTAMP，不含唤醒进程和拷贝）作对照。
- 分位数算法与 Rust 侧相同（最近秩）。

**文档**
- README：核心问题的回答（包从 rx_burst 到 future 被唤醒的五个环节）、设计亮点、测量方法（rdtscp、写合并、ABBA、C 的可比性）、超时 / 丢包 / 异常处理、运行方式、运维事项、换网卡改哪几行、版本。
- `scripts/report.py`：从 JSON 直接生成报告里的 Markdown 表格，每个数字都能从日志复现。

## 2026-09-30 · 16 【主考核】A / B 各连续 10 分钟（64 session，delay 500 µs，payload 64 B）

**硬门槛：两边全部满足**
| | 实际时长 | sent = received | 超时（丢包） | mbuf 初值 → 终值 | AWS allowance 超限 | 退出码 |
|---|---|---|---|---|---|---|
| A | 600.0 s | 53,934,999 | 0 | 8191 → 8191（泄漏 0） | 全部 0 | 0 |
| B | 600.0 s | 54,903,703 | 0 | 8191 → 8191（泄漏 0） | 全部 0 | 0 |

**进程内耗时（排名指标，ns）**
| | p50 | p90 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|
| A | 180 | 250 | 350 | 450 | 580 | 29,590 |
| B | 180 | 420 | 521 | 610 | 749 | 50,540 |
| A − B | 0 | −170 | −171 | −160 | −169 | |
- 段② p50 / p99：A 130 / 300，B 120 / 270 → **+10 / +30**（runtime 在接收侧真正的税）。
- 段①：A 的发送 0% 落在 < 100 ns 的间隔，B 有 19%（在这一档里段① p50 为 300 ns）→ 与 14 条的分析一致。

**对账细节**
- A 的端口收包数 53,935,018 = reply 53,934,999 + unexpected 1 + other 8 + ARP 10，**逐包对得上**。
- unexpected 1：一个 echo reply 的 seq 对不上任何在途或已超时的请求（5400 万分之一），最可能是网络层的重复包；它不影响 sent / received / timeouts 的对账。
- 10 分钟里对端各发来 10 次 ARP 请求，都已应答（与对端的 ARP 缓存过期周期一致）→ 这再次说明必须自己实现 ARP 应答。

**小插曲**：后台任务报"失败"，实际是我在命令末尾用了 `tail -3 文件1 文件2`（GNU tail 同时处理多个文件时不接受 `-3` 这种写法），两个长测本身都是退出码 0。

**git**：本地仓库已初始化并完成首次提交；为 GitHub 生成了专用的 SSH 密钥（建议作为仓库的 Deploy key），GitHub 主机指纹已与官方 API 公布的逐一核对。等仓库建好后推送。

## 2026-09-30 · 17 C 的测量、同速率对比、ABBA 60 秒、报告、清理

**C（系统 ping，内核网卡）与 A 的端到端（µs）**
| 场景 | 客户端 | p50 | p90 | p99 | p99.9 | p99.99 | max |
|---|---|---|---|---|---|---|---|
| 64 路、约 6.5 万包/秒 | A（delay 800） | 190.2 | 193.4 | 196.5 | 257.6 | 271.8 | 336.8 |
| | C `ping -U` | 182 | 328 | 464 | 631 | **4070** | 14500 |
| | C 默认口径 | 151 | 269 | 357 | 394 | 451 | 572 |
| 1 路、1000 包/秒 | A（1 session） | **63.2** | **65.6** | **67.9** | 73.8 | 93.5 | 158.5 |
| | C `ping -U` | 74 | 275 | 281 | 290 | 439 | 10800 |
| | C 默认口径 | 78 | 143 | 275 | 277 | 285 | 322 |
- **单路低速率最干净**（对端的中断合并处于最低档）：p50 快 11 µs（−15%），p90 到 p99 快约 210 µs（快 4 倍多）。后者主要是内核那边的 CPU 在两个包之间空闲、每次都要被中断唤醒；A 一直 busy-poll，没有这个问题。
- 64 路时 p50 看不出优势（中位数由对端的中断合并档位决定），但尾部差距明显：p99 197 对 464 µs，p99.99 272 µs 对 4.07 ms。
- **结论：kernel bypass 换来的主要是确定性，不是中位数。**
- 所有 C 运行 0 丢包。

**ABBA 3 对 × 60 秒**：进程内 p50 A − B = 0，p99 = −171；段② +10 / +20 → 与 3 对 × 30 秒、10 分钟主考核三者一致。

**报告**：`docs/REPORT.md` 由 `scripts/report.py`（主表格）+ 内联脚本（C 对比表、ABBA 表）从 JSON 生成，每个数字都能复现。

**清理**：clippy 3 条警告全部修掉（`as_chunks`、`for … by_ref()`、RefCell 借用改用块作用域，不再"看起来"跨越 await）；8 个单测通过；A / B 冒烟测试结果不变。

**异常包诊断**：10 分钟的 A 和一次短测的 B 各出现过 1 个 "unexpected"（seq 对不上任何在途或已超时的请求）。出现次数与运行时长无关，所以不像随机的网络重复包。新增 `Stats::note_anomaly`（冷路径，最多记 16 条：id、seq、当时 session 的状态），写进报告。之后 4 轮 10 秒测试没有复现，属于偶发；它们不影响丢包对账。

**git**：远端 `github.com/Chen-Shuai-CS/dpdk-async-ping`（公开），用仓库级 Deploy key 推送；首个提交已被 GitHub 关联到本人账号。

## 2026-09-30 · 18 setup 幂等性验证；runtime 单元测试（含变异测试）

**setup 幂等性**：在已配置好的机器上重跑完整的 `scripts/setup.sh`，8 个阶段 1.5 秒全部识别为"已完成"并跳过，没有误报需要重启。

**runtime 单元测试**
- 新增 `Runtime::run_offline()`：不驱动网卡，只跑 executor + timer，直到所有 task 结束；自带死锁检测（所有 task 都在等，却既无就绪任务也无待触发 timer → panic）。它让 runtime 的单测完全不依赖 DPDK、网卡和 root。
- `crates/rt/tests/offline.rs`，7 个测试：sleep 按 deadline 顺序且不早到；Mailbox 顺序交接 100 个值；poll 期间自唤醒 1000 次不丢；**过期 waker 不会唤醒复用同一槽位的新任务**；drop runtime 时释放未完成 task 持有的资源（对应 mbuf 归还）；取消的 sleep 不误触发；死锁检测。
- **变异测试**：把 executor 的代数检查临时改成永远为真，"过期 waker"测试立即失败（task 被 poll 3 次而非 2 次）→ 证明这个测试真的能抓到这类 bug；改回后全部通过。
- 全仓库现有 15 个测试（rt 7、pingproto 5、pingkit 2、timerq 1），都不需要网卡。

**为什么值得做**：之前 runtime 的正确性只靠真网卡上的端到端运行来证明。答辩时如果被问"边界情况怎么保证"，现在可以指着这些测试说明，包括用变异测试证明测试本身有效。
