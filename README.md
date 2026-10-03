# dpdk-async-ping

用 Rust 写的**单核、kernel-bypass 的 async runtime**（`crates/rt`），以及跑在它上面的 ICMP ping 客户端。
自研 executor / Waker / TSC timer / DPDK poll-mode reactor，不依赖任何现成 runtime。

| 标签 | 程序 | 说明 |
|---|---|---|
| **A** | `async-ping` | 跑在 `rt` 上，64 个 session 各是一个独立的 async task |
| **B** | `raw-ping` | 行为完全相同，手写 busy-poll 循环 + 状态表，不经过 runtime。A 与 B 共用除调度以外的全部代码，所以 A − B 就是抽象层的成本 |
| **C** | 系统 `ping` | 走内核网卡，作为"不做 kernel bypass"的参照 |

## 结果速览

正式成绩：SPEC 规定的 600 秒主考核（64 session、delay 500 µs、payload 64 B），代码版本 v4。完整报告见 [`docs/REPORT.md`](docs/REPORT.md)。

| | 样本 | 丢包 | mbuf 泄漏 | 进程内 p50 | p99 | p99.9 | 段②（接收侧）p50 / p99 |
|---|---|---|---|---|---|---|---|
| A async-ping | 6514 万 | 0 | 0 | 180 ns | 330 ns | 429 ns | 120 / 270 ns |
| B raw-ping | 6777 万 | 0 | 0 | 170 ns | 429 ns | 540 ns | 110 / 260 ns |
| **A − B**（插值） | | | | **+9.6 ns** | **−98.9 ns** | −111.1 ns | **+10.8 / +17.9 ns** |

- **排名指标**：p50 的 A − B 是 +9.6 ns（一次运行内部的 95% 区间 +9.1 ~ +10.2），p99 是 −98.9 ns。时间戳的分辨率是 10 ns，所以差值用插值分位数给出（报告 §3.1）。
  A / B 交替 10 对（p50 +10.9，10 对全为正；p99 −104.7，10 对全为负）和重启两次后的复测（方向全部一致）说明结论不依赖于挑了哪一次运行。
  但同一份二进制换一种内存布局，A 的发送段就能差 5 ns，所以 **p50 的差值应当读作 +10 ~ +17 ns**（报告 §3.5）。
- **抽象税是正的，每个请求约 20 ~ 30 ns**（三个被测段平均值之和的差：主考核 +32.2，交替 10 对 +20.9）。其中接收路径本身约 9 ns，批内排队约 2 ns，其余在不计分的发送侧调度里（§2.2）。
- **p99 为负不代表 runtime 更快**：发送最后那次写网卡门铃约 250 ns 才完成，CPU 的写入队列被它堵住；这段停顿在 B 落进计分的段①，在 A 落进不计分的段③（§2.1）。
  把停顿统一移到 T0 之前再比，A − B 在每个分位数上都是正的：p50 +10.4、p99 +14.7 ns。这个机制与三个实验吻合，但没有硬件计数器可以直接观测，是推断。
- **硬门槛与可靠性**：10 分钟零崩溃、零丢包、零泄漏；另各跑了 30 分钟（A 2.05 亿、B 2.00 亿个请求，零丢包）；故障注入 25 个场景 × A / B 共 50 项全部干净收场。
  重启后的两个复测会话共 13.6 亿个请求丢了 1 个，本机没有任何丢弃计数（报告 §8）。
- **kernel bypass 的收益**（A 对系统 ping，实测速率对齐）：64 路 p50 115 对 249 µs、p99 126 对 582 µs、max 0.47 对 18.1 ms；单路 p99 39 对 47 µs。换来的主要是确定性。

## 交付物在哪（对应 SPEC §9）

| SPEC §9 的要求 | 在哪 | 是什么 |
|---|---|---|
| 1. Git 仓库 + README，一键 setup、一键 run | 本仓库；`scripts/setup.sh`、`scripts/run.sh` | 环境搭建（工具链、DPDK、驱动、大页、核隔离）与运行，见 §3 |
| 2. **运行日志** | [`logs/`](logs/) | 当前代码版本 v4 最新的一整套测量的原始输出：每次运行一个 `.log`（程序打印的完整报告）+ 一个同名 `.json`（机器可读，另含构建版本与环境）。明细见下表 |
| 2. **延迟报告** | [`docs/REPORT.md`](docs/REPORT.md) | 全部表格由 `scripts/make_report.py` 从 `logs/` 生成，图由 `scripts/plots.py` 生成；表格之外的文字是对数据的解读 |
| 3. 答辩，现场跑一次 `--duration-sec 60` | [`docs/DEFENSE.md`](docs/DEFENSE.md)、本文 §3.1 | 答辩提纲；考察机上现成的运行方法和屏幕上该看哪几行 |

**运行日志的明细**（正式数据是 2026-10-02 在同一次开机、同一个网络环境里由 `scripts/campaign.sh` 连续测的；A / B 来自同一个干净的提交）：

| 目录 | 内容 | 报告里对应 |
|---|---|---|
| `logs/final/A-600.*`、`B-600.*`、`ci.json` | **主考核**：A、B 各连续 10 分钟；由原始样本算出的置信区间（原始样本每份约 500 MB，不进仓库） | §1、§3.2 |
| `logs/ab-20261002-124231/` | A / B 交替 10 对 × 60 秒 | §3.3 |
| `logs/versions-ab/v3-vs-v4/`、`logs/r1-bisect/` | 上一个版本与当前版本的同场对比；v4 第一版为什么让慢发送变多（五个版本同场轮流，附补丁和说明） | §3.4 |
| `logs/sessions/v4-reboot1/`、`v4-reboot2/` | 重启之后的两个复测会话（各 200 次运行），以及 `v4-reboot2/placement/`（"发送段慢 5 ns"的追查）、`v4-reboot2/otherlog/`（那 1 个丢包的追查） | §3.5、§8 |
| `logs/diag/` | 诊断口径：读 T0 之前加 `sfence` / `mfence` / N 次普通写入 | §4 |
| `logs/c/`、`logs/final/A-vsC.*`、`B-vsC.*`、`A-1flow.*` | 系统 ping（C）与速率对齐的 A / B | §6 |
| `logs/probe/` | `probe` 构建：段①、段②的子步骤 | §7 |
| `logs/fault/20261002-143203/`、`logs/soak/` | 故障注入（25 个场景 × A / B）；A、B 各连续 30 分钟 | §8 |
| `logs/timer-batch/` | timer"批量唤醒、统一 poll"与"触发一个、poll 一个"的同场对照（6 轮，附补丁和说明） | `DESIGN.md` §1.1 |
| `logs/setup/`、`logs/compliance.txt` | 环境搭建与首次上线验证的日志；合规检查的输出 | |

仓库里只有当前版本最新的一套。此前三个代码版本（v1 ~ v3）的日志和当时的报告保存在考察机本地的 `~/bq-archive/`，不在仓库里。

**其余文档**

| 文档 | 内容 |
|---|---|
| [`docs/DESIGN.md`](docs/DESIGN.md) | 设计与实现说明：抽象税的逐项来源与"还值不值得优化"、仓库结构、设计要点与取舍、测试与诊断工具、异常处理细则、遇到的异常汇总 |
| [`docs/REVIEW.md`](docs/REVIEW.md) | 外部代码审查的处理记录：两轮（R1 ~ R11）以及后来的几条补充意见 |
| [`docs/HISTORY.md`](docs/HISTORY.md) | v1 ~ v3 的测量与结论（冻结的历史摘要）：跨重启 / 跨日期的复测、两次由数据引出的纠正、两次云平台侧事件 |
| [`docs/CODE_GUIDE.md`](docs/CODE_GUIDE.md) | 代码解读：每块代码做什么、每项功能在哪个文件哪一行 |
| [`docs/WORKLOG.md`](docs/WORKLOG.md) | 开发记录：每个决定、实验和走过的弯路 |

---

## 1. 架构：raw frame 从 `rx_burst` 进来，到 64 个 session 的 future 被唤醒，中间 runtime 提供了什么

```text
 NIC ──DMA──▶ RX 环 ──rx_burst──▶ reactor ──Driver::on_packet──▶ Mailbox::put ──wake──▶ 就绪队列
                        (T2)      rt 主循环      应用提供的分类器       rt 原语          rt executor
                                                                                          │
 session task ◀── future 返回 Ready ◀── executor 出队并 poll ◀────────────────────────────┘
    (T3)
```

1. **poll-mode reactor**（`rt::Runtime::run`）：主循环 busy-poll 调用 `rx_burst`，不睡眠、不用中断。对每个包调用应用提供的 `Driver::on_packet`（泛型参数，静态分发，可内联）。
2. **Driver（协议侧，应用提供）**：解析 Ethernet / IPv4 / ICMP，用 ICMP `id` 找到对应 session 的 `Flow`，核对 `seq` 和回带的发送时间戳都是它正在等的那个，
   然后把 **mbuf 的所有权**连同 T2 一起放进它的 `Mailbox`。
3. **Mailbox**（`rt::sync`）：单槽信箱。`put` 存入值并调用登记在里面的 `Waker::wake`。
4. **Waker**（`rt::executor`）：data 里编码 `rt_id | 代数 | 任务号`，wake = 把任务号推进就绪队列（带去重位）。clone / drop 都是空操作：没有引用计数，也就没有原子操作。
5. **executor**：reactor 每分发**一个**包就立刻跑一遍就绪队列：出队 → poll 该 task → 它的 future 从 `mailbox.recv().await` 返回 `Ready(reply)` → session 代码继续执行（T3）。

全程没有锁、没有原子操作、没有堆分配。另外两块：**timer**（TSC deadline 小顶堆，`sleep` 精确到 TSC 读数，sleep 误差 p50 10 ns）；**超时**由每 100 µs 一次的维护节拍扫描，不给每个包注册 timer。

session 的代码就是 SPEC 要求的形状（`crates/async-ping/src/main.rs`）：

```rust
while !sh.stopping.get() {
    let Some(stamp) = send(&sh, id, seq).await else { break };   // T0 → T1，段①（T0 = send() 的第一行）
    let reply = wait_reply(&sh, id, seq).await;                    // T2 → T3，段②；超时也从这里返回 Err(Timeout)
    let t3 = rdtsc();                                              // T3：本 task 从 wait_reply().await 恢复执行
    woke = sleep_until(t3 + sh.delay).await;                       // 样本之外；期间 reply（及其 mbuf）被本 task 持有
    record(&sh, id, seq, stamp, reply, t3);                        // 记录样本；reply 在这里 Drop → mbuf 归还
    seq = seq.wrapping_add(1);
}
```

（为突出形状省略了两行统计代码；与 SPEC §5 的区别只有：`send` 内部自己重试、停止时返回 `None`；超时不用 `?` 退出循环——超时的请求计为丢失，session 照常继续。）

| crate | 作用 |
|---|---|
| `dpdk-sys` / `dpdk` | bindgen 裸绑定 + 安全封装：`Eal`、`Mempool`（`'static`）、`Mbuf`（RAII，`!Send`）、`Port`（`!Send + !Sync`）、`RxBurst`、TSC。59 处 unsafe 有 47 处在这里 |
| `pingproto` / `timerq` | 帧模板与增量校验和、reply 解析、ARP 应答；TSC deadline 最小堆。都不含 unsafe |
| `pingkit` | A、B 共用：参数、数据面初始化 / 关停 / 泄漏核对、发送函数（段①）、回复身份判定、直方图、报表 |
| **`rt`** | **runtime**：executor、Waker、timer、reactor、Mailbox |
| `async-ping` / `raw-ping` | A / B |

## 2. 亮点

- **A − B 测的确实是抽象层**：A 与 B 共用除调度以外的全部代码（发送函数、协议解析、回复身份判定、直方图、维护动作），四个时间戳用同一个函数、打在语义相同的位置（§4）。
- **runtime 的设计**：Waker 零原子操作（只编码任务号和代数，跨线程调用直接 abort，过期 Waker 靠代数失效）；任务槽、就绪队列固定容量，热路径零分配；
  每分发一个包就立刻 poll 它的 task，同一批里靠前的包不被整批拖慢；mbuf 的所有权从网卡一路移交到 session，Drop 即归还。unsafe 的边界与每处的理由见 [`docs/DESIGN.md`](docs/DESIGN.md) §3。
- **测量方法经过验证**：发现并修正了 `rdtsc` 乱序执行造成的系统性偏差（它曾让段②的 A − B 被报成 +120 ns）；标定出时间戳分辨率只有 10 ns 并用插值分位数应对；
  分块自助法给置信区间；A / B 交替多对、新旧版本同场配对、重启后复测；主循环自带停顿检测，最大值尖刺可以直接归因。
- **每个异常数字都追到了原因，追不到的写明卡在哪里**：19 项的汇总表在 [`docs/DESIGN.md`](docs/DESIGN.md) §6。下面两项是主线。
- **经得起审查**：两轮外部代码审查的 11 条意见逐条核实、处理，附证据（[`docs/REVIEW.md`](docs/REVIEW.md)）；其中两条是真实的行为缺陷，修复前先复现，修复后有测试和故障场景。

### 2.1 p99 为什么是负的

```text
ns          0    50   100  150  200  250  300  350  400
            |----|----|----|----|----|----|----|----|
doorbell        [== pkt1's doorbell write in flight ==]

B  pkt1     [###][..]
B  pkt2             [#~~~~~~~~ stalled ~~~~~~~~~~~][##]
                    ^T0                               ^T1
                    |<-------- seg1 = 300 ns -------->|   <- ranked

A  pkt1     [###]
A  sched        [..... scheduling ....~~ stalled ~]       <- seg3, NOT ranked
A  pkt2                                           [###]
                                                  ^T0 ^T1
                                                  |<->|   <- seg1 = 50 ns, ranked

[###] tx_burst    [...] our own code    [~~~] CPU stalled: store queue full behind the doorbell write
```

两边的第二个包都在约 400 ns 时交给网卡，A 并没有更快。三个实验（报告 §4.3）：T0 之前加 `sfence` 没有变化；加 `mfence`，B 的慢发送消失；
不加任何栅栏指令、只多做 N 次普通的内存写入，N ≤ 36 没有变化，N ≥ 44 效果与 `mfence` 相同。反方向的证据：两次发送之间少做一次函数调用，慢发送就变多几倍（报告 §3.4）。
能带走的结论是：**这段停顿躲不掉，但落在哪里可以安排**——A 的结构让它自然落在非关键路径上，但 B 在 T0 前加一条指令也能做到（计分路径 170 / 320，不比 A 的 180 / 330 高），所以这不是 runtime 的优势。

### 2.2 抽象税收在哪，还值不值得优化

| 来源（每个请求，A − B） | 主考核 | 说明 |
|---|---|---|
| ① 接收路径本身：`put → wake → 就绪队列 → 出队 → poll → 恢复` | +8.9 ns | 批内第 1 个包的段②差值，约一格 |
| ② 同一批里排在后面的包多等 | +1.8 ns | A 处理完一个包后的收尾比 B 长；v1 里是 +5.5，去掉一次多余的读时钟后（v2）降到现在 |
| ③ 发送侧调度：timer 触发 → wake → poll → 走到 `send()` | +20.8 ns | 落在不计分的段③；运行之间波动最大，一部分是那段停顿落在了哪里 |
| **合计** | **+32.2 ns** | 三个被测段的平均值之和，不是完整的 CPU 成本（T3 之后的收尾、主循环空转不在内） |

**还值不值得优化**：明显多余的一步（登记 sleep 时多读一次时钟）已在 v2 去掉，同场配对 36 轮全部下降。剩下的指令级优化（`RefCell` 换 `Cell`、省一次虚调用）估计各 1 ~ 2 ns，低于时钟分辨率，不值得。
timer 一侧改成和收包一侧一样"触发一个、马上 poll 一个"实测过：段③ p99 平均低约 115 ns，但排名指标和每请求总账（抽象税）都没有可测出的变化——它减少的是尾部抖动，不是平均成本——所以没有并入（`logs/timer-batch/`）。
更大的波动来自"状态"——那段停顿落在哪一段、内存布局——而不是多执行了几条指令。
逐项对应的代码、量法、三次开机的范围，以及完整的"还值不值得优化"表，见 [`docs/DESIGN.md`](docs/DESIGN.md) §1。

## 3. 怎么跑

### 3.1 在考察机上：已经部署好，登录就能跑

环境、代码、可执行文件都是现成的：仓库在 `~/dpdk-async-ping`，A / B 已编译（release）；Rust、DPDK 25.11.3、`igb_uio` 已装好；核 3 隔离、1024 × 2 MiB 大页已写入引导项；
网卡参数在 `config/nic.env`（自动探测生成，只有 device-number 1 的那张交给 DPDK）。重启后唯一会丢的是"网卡绑定到 DPDK"，`run.sh` 每次运行都会自动补上。

```bash
cd ~/dpdk-async-ping
```

```bash
scripts/check-env.sh
```

```bash
scripts/run.sh A --delay-us 500 --duration-sec 60
```

```bash
scripts/run.sh B --delay-us 500 --duration-sec 60
```

`check-env.sh` 是只读的环境自检（不到 1 秒）。每次 A / B 运行约 65 ~ 70 秒（60 秒测量 + 启动与收尾），结束时打印一份报告，看这几行：

```text
对账：sent − received − timeouts − in-flight = 0（应为 0）；丢包 = timeouts = 0（其中 0 个迟到收到）
收包对账：rx 6910950 − (received + late + unexpected + foreign + tsc-mismatch + other + arp) = 0（应为 0）
metric (ns)                       count      min     mean      p50      p90      p99    p99.9   p99.99        max
in-process ①+②  [排名指标]          6910949       60      186      180      220      320      401      521      16820
seg① send  T1−T0                6910949       40       54       50       60       70      160      250       5710
seg② recv  T3−T2                6910949       20      132      120      170      270      340      450      16760
mbuf：pool 8191，初值 avail 8191（端口启动后 7168），关停后 avail 8191 → 泄漏 0 ✔
```

（2026-10-02 在考察机上用 v4 按这条命令原样跑出来的 A。紧接着跑的 B：`in-process` 一行是 p50 170 / p99 429。）
**零丢包**看对账那一行；**零泄漏**看最后的 `泄漏 0 ✔`；**排名指标**是 `in-process ①+②` 一行，A 与 B 的同一行相减就是 A − B（p50 约 +10，p99 约 −100）。
运行过程中每 5 秒打印一行进度。输出同时写入 `logs/<A|B>-<时间>.log`，同名 `.json` 是机器可读的报告（随手运行产生的这些文件不进仓库）。

接着可以跑的：

```bash
scripts/run.sh A --delay-us 500 --duration-sec 600
```

```bash
scripts/run.sh C --duration-sec 60
```

```bash
scripts/ab.sh 3 60
```

```bash
scripts/fault.py
```

```bash
scripts/check-compliance.sh
```

依次是：10 分钟主考核（B 同理）；C（系统 ping，走内核网卡，不占 DPDK 网卡）；A / B 交替 3 对、每轮 60 秒，结束时打印逐对差值（约 7 分钟）；
故障注入 25 个场景 × A / B（约 5 分钟）；逐条核对 SPEC 的硬性要求（34 项，约 20 秒，含编译、clippy 与 50 个单元测试）。

- **一次只能跑一个 A 或 B**：它们独占同一张网卡。第二个实例会在几秒内被单实例锁拒绝，不会干扰正在跑的那个。
- **随时可以 Ctrl-C**：程序会停止发送、等在途请求收尾、照常打印报告并核对 mbuf 泄漏。
- **长时间测量期间不要在这台机器上编译**（会污染尾延迟）。`run.sh` 自带的增量编译在代码没改时是空操作。

### 3.2 在一台新机器上从零开始

```bash
git clone https://github.com/Chen-Shuai-CS/dpdk-async-ping ~/dpdk-async-ping && cd ~/dpdk-async-ping
```

```bash
scripts/setup.sh
```

```bash
sudo reboot
```

`setup.sh` 一键搭环境（工具链、Rust、DPDK、igb_uio、大页、核隔离、中断），幂等、分阶段（`scripts/setup.sh <阶段>` 可单独重跑某一步）；
网卡参数由 `scripts/detect-nic.sh` 自动探测并写入 `config/nic.env`。只有第一次需要重启（核隔离与大页的启动参数）。重启后与 §3.1 相同；
`scripts/campaign.sh` 一键重测报告里的全部数据并重新生成报告（约 2 小时 15 分钟）。

### 3.3 参数

- A / B 的参数：`--delay-us`、`--duration-sec`（必填），`--sessions`（默认 64）、`--payload`（默认 64）、`--timeout-us`（默认 10000）、`--progress-sec` 等，见 `--help`。
- 也可以不经过脚本直接运行可执行文件（网卡参数自动从 `config/nic.env` 读取；网卡需已 `scripts/bind.sh`）：`sudo target/release/async-ping --delay-us 500 --duration-sec 60`。
  `run.sh` 额外做的是：重新绑定网卡、增量编译、以 root 运行（无 IOMMU 时 DPDK 需要读物理地址）、保存输出。

## 4. 测量口径

**四个时刻**（A 与 B 语义相同、调用同一个打点函数 `dpdk::tsc::rdtsc()`，内部是 `rdtscp`）：

| | A | B |
|---|---|---|
| **T0** | `send()` 的第一行（早于向 runtime 查找端口） | 循环里 `record(上一个 reply)` 之后、调用发送函数之前 |
| **T1** | `tx_burst` 返回（同一个发送函数内） | 同左 |
| **T2** | `rx_burst` 返回（runtime 主循环）；同一批包共用一个 T2 | `rx_burst` 返回（B 的循环）；同左 |
| **T3** | task 从 `mailbox.recv().await` 恢复后的第一行 | 状态机拿到 reply、可以开始算延迟的那一刻 |

**进程内耗时 = (T1 − T0) + (T3 − T2)**（排名指标）；端到端 = T3 − T0；sleep 误差 = timer 发现到期 − deadline；段③ = 发现到期 → 下一个 T0（另报一行 deadline → 下一个 T0）。
每一项都输出 p50 / p90 / p99 / p99.9 / p99.99 / max。为什么 B 的 T0 取在 record 之后、为什么运行起点延后 1 ms、为什么用 `rdtscp` 见 [`docs/DESIGN.md`](docs/DESIGN.md) §4。

**超时与丢包**（SPEC §7.1）：reply 超过 **10 ms**（`--timeout-us 10000`，远大于约 53 µs 的往返时间）没回来，就算**丢失**，计入 `timeouts`，不进入延迟分布；session 照常继续。
超时之后才到的回复计为 `late`，释放 mbuf，绝不会被当成后面请求的回复。报告里两条对账必须为 0：`sent − received − timeouts − in-flight-at-end`（每个请求都有下落）、
`rx − (received + late + unexpected + foreign + tsc-mismatch + other + arp)`（每个收到的包都恰好归入一类）。丢了包怎么"自洽解释"：对账 → 本机计数器（网卡 `imissed` / `ierrors` / `rx_nombuf`、AWS 限额计数）→ 改变一个变量复测（报告 §8）。
其余异常处理（外来回复、回复身份核对、网卡 reset、信号、TX 环满、单实例锁）见 [`docs/DESIGN.md`](docs/DESIGN.md) §5。

**C 怎么测、为什么能和 A 相减**（SPEC §6.3）：

- **同一路径**：同一对端、同一子网，只是本端换成内核网卡（device-number 0）；**同样的帧**：`ping -s 64` → 106 B，与 A 相同。
- **同样的并发，速率实测并对齐**：64 个 `ping -i 0.001` 进程 = 64 路（ICMP id 各不相同；iputils 的 `-i` 只精确到整数毫秒，实测 `-i 0.0005` 会退化为"收到即发"）。
  名义 6.4 万包/秒，实测约 6.2 万：每个 ping 结束时会报告自己实际跑了多久，`scripts/summarize_c.py` 用它算实际速率；再给 A / B 选 delay 使实测速率对齐（这次 A 61,601、C 62,059 包/秒，差 0.7%）。
- **同样的口径**：默认 `ping -U`（用户态到用户态），对应 A 的 T3 − T0；ping 默认的"用户态发送 → 内核收包时间戳"不含唤醒进程与拷贝，两种都给出。另测一组单路低速率。
- **为什么速率必须对齐**：对端网卡驱动的自适应中断合并让往返时间随速率非单调变化，速率不同，对端状态就不同。
- **没有对齐的是负载的形状**：A 是闭环（收到回复后等 delay 再发），ping 是开环（按固定间隔发）。所以 A 与 C 的差值是"这个环境、这种负载下"的读数，不能全部归因于绕过内核。

## 5. 运维事项

- **CPU 规划**（4 核无超线程）：核 3 由 `isolcpus/nohz_full/rcu_nocbs` 隔离，runtime 独占；核 0–2 给 OS、SSH、C 和进度上报线程（核 1）。
  核 3 上来自本机内核的中断每秒约 2 次；另有虚拟机宿主机造成的短暂停顿每秒约 200 次（主循环的停顿检测，累计约 0.06%），这部分隔离不掉。
- **网卡**：device-number 0（SSH 所在）永远留给内核；`bind.sh` 内置保险，拒绝绑定 SSH 所在的网卡；`unbind.sh` 把 DPDK 网卡还给内核。
- **大页**：启动参数预留 1024 × 2 MiB；EAL 使用 `--in-memory`，进程退出后不在 `/dev/hugepages` 留文件。
- **长测期间不要在本机编译**：编译会占满核 0–2 并冲刷与核 3 共享的 L3，污染尾延迟。一组要互相比较的运行要用同一个二进制文件（`session.sh` 开始时编译一次；原因见报告 §3.5）。
- **日志**：直接运行 `run.sh` 会在 `logs/` 下留一对 `.log` + `.json`（已被 `.gitignore` 忽略）；要留档的运行由 `campaign.sh` / `session.sh` / `ab.sh` / `drift.sh` 写进各自的子目录。
- **图**：`scripts/plots.py` 生成 `docs/img/` 下的图，文字用中文（`setup.sh` 会安装 Noto Sans CJK SC 字体）；找不到中文字体时自动退回英文。

## 6. 换一块网卡要改哪几行

网卡参数不写死在代码里。`scripts/detect-nic.sh [device-number]` 通过 IMDS + sysfs 自动探测并生成 `config/nic.env`：

```bash
DPDK_PCI=0000:28:00.0      # ← 绑定到 DPDK 的 PCI 地址
DPDK_IFACE=enp40s0         # ← 它在内核里的名字
DPDK_MAC=06:ff:df:7d:66:91 # ← 源 MAC（程序实际使用端口报告的 MAC）
DPDK_IP=10.202.15.133      # ← 源 IP（AWS 源地址检查要求是该 ENI 的 IP）
KERNEL_IFACE=enp39s0       # ← C 使用的内核网卡
PEER_IP=10.202.8.15        # ← 对端
PEER_MAC=06:ff:fd:b6:f0:cd # ← 对端 MAC
```

换网卡 = 先 `scripts/unbind.sh`，再重跑 `scripts/detect-nic.sh <device-number>`（或直接改上面这几行），然后 `scripts/run.sh`。
换成非 ENA 的网卡时，还要在 `scripts/setup.sh` 的 `-Denable_drivers=` 里加上对应的 PMD（比如 `net/mlx5`），并视情况改用 vfio-pci。

## 7. 版本与环境

| | |
|---|---|
| 机器 | AWS c8a.xlarge（AMD EPYC 9R45，4 核无超线程，7.6 GiB），东京 apne1-az4 |
| OS | Amazon Linux 2023，内核 6.18.48 |
| DPDK | **25.11.3**（最新 LTS 分支；只编译 `net/ena`、`net/null`、`net/ring`） |
| 内核模块 | igb_uio（dpdk-kmods @ 9b182be），`wc_activate=1`：ENA 的低延迟发送队列依赖写合并，而这台机器没有 IOMMU，主线 vfio-pci 不支持写合并 |
| Rust | **1.98.1**（`rust-toolchain.toml` 固定），release：`lto=fat`、`codegen-units=1`、`panic=abort`、`target-cpu=native` |
| 依赖 | bindgen / cc / pkg-config（构建期）；clap、libc、serde（非热路径）。**没有任何带 executor / reactor 的 runtime**，也未使用 `futures-util`（`Cargo.lock` 里 grep 这些名字结果为 0） |
| 代码版本 | 标签 **`v4`**（报告里的全部数据都由它的代码构建；之后只改过注释）。v1 ~ v3 各自改了什么见 `docs/HISTORY.md` 开头的表 |
| 测量环境 | 正式数据：2026-10-02，同一次开机，12:22 ~ 14:36 UTC 连续测完；对端往返时间约 53 µs（64 路、delay 500 µs 时） |

## 8. 提交方式

公开仓库：<https://github.com/Chen-Shuai-CS/dpdk-async-ping>（也可以打包成 zip 交给联系人，内容相同）。
