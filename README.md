# dpdk-async-ping

用 Rust 写的**单核、kernel-bypass 的 async runtime**（`crates/rt`），以及跑在它上面的 ICMP ping 客户端。
自研 executor / Waker / TSC timer / DPDK poll-mode reactor，不依赖任何现成 runtime。

| 标签 | 程序 | 说明 |
|---|---|---|
| **A** | `async-ping` | 跑在 `rt` 上，64 个 session 各是一个独立的 async task |
| **B** | `raw-ping` | 行为完全相同，手写 busy-poll 循环 + 状态表，不经过 runtime |
| **C** | 系统 `ping` | 走内核网卡，作为"不做 kernel bypass"的参照 |

**结果速览**与完整报告见 [`docs/REPORT.md`](docs/REPORT.md)；开发过程中的每个决定与实验见 [`docs/WORKLOG.md`](docs/WORKLOG.md)。

---

## 1. 核心问题：raw frame 从 `rx_burst` 进来，到 64 个 session 的 future 被唤醒，中间 runtime 提供了什么

```text
 NIC ──DMA──▶ RX 环 ──rx_burst──▶ reactor ──Driver::on_packet──▶ Mailbox::put ──wake──▶ 就绪队列
                        (T2)      rt 主循环      应用提供的分类器       rt 原语          rt executor
                                                                                          │
 session task ◀── future 返回 Ready ◀── executor 出队并 poll ◀────────────────────────────┘
    (T3)
```

1. **poll-mode reactor**（`rt::Runtime::run`）：主循环 busy-poll 调用 `rx_burst`，不睡眠、不用中断。
   对每个包调用应用提供的 `Driver::on_packet`（泛型参数，静态分发，可内联）。
2. **Driver（协议侧，应用提供）**：解析 Ethernet / IPv4 / ICMP，用 ICMP `id` 找到对应 session 的 `Flow`，
   检查 `seq` 是否是它正在等的那个，然后把 **mbuf 的所有权**连同 T2 一起放进它的 `Mailbox`。
3. **Mailbox**（`rt::sync`）：单槽信箱。`put` 存入值并调用登记在里面的 `Waker::wake`。
4. **Waker**（`rt::executor`）：data 里编码 `rt_id | 代数 | 任务号`，wake = 把任务号推进就绪队列（带去重位）。
   clone / drop 都是空操作：没有引用计数，也就没有原子操作。
5. **executor**：reactor 每分发**一个**包就立刻跑一遍就绪队列：出队 → poll 该 task →
   它的 future 从 `mailbox.recv().await` 返回 `Ready(reply)` → session 代码继续执行（T3）。

session 的代码就是 SPEC 要求的形状（`crates/async-ping/src/main.rs`）：

```rust
loop {
    record(held.take());                          // 上一轮的 reply：sleep 之后才记录、释放 mbuf
    let stamp = send(&sh, id, seq).await;         // T0 → T1
    let r = flow.mailbox.recv().await;            // T2 → T3；超时时 driver 放进 Err(Timeout)
    let t3 = rdtsc();
    held = r.ok().map(|reply| (reply, stamp, t3));
    woke = sleep_until(t3 + delay).await;         // 期间 `held` 持有 reply 的 mbuf
    seq = seq.wrapping_add(1);
}
```

## 2. 仓库结构

```text
crates/
  dpdk-sys/    bindgen 裸绑定 + 很薄的 C shim（rx/tx_burst、mbuf alloc/free 等 static inline 函数）
  dpdk/        安全封装：Eal、Mempool('static)、Mbuf(RAII，!Send)、Port(!Send+!Sync)、RxBurst、TSC
  pingproto/   帧模板、增量校验和、reply 解析、ARP 应答（纯 Rust，可离线单测）
  timerq/      TSC deadline 最小堆（rt 的 timer 与 B 的 delay 共用）
  pingkit/     A、B 共用：参数、数据面初始化/关停/泄漏核对、发送函数（段①）、直方图、报表、上报线程
  rt/          ★ runtime：executor、Waker、timer、reactor、Mailbox
  async-ping/  A
  raw-ping/    B
scripts/
  setup.sh     一键环境搭建（幂等，分阶段）      run.sh     一键运行 A / B / C
  detect-nic.sh / bind.sh / unbind.sh / check-env.sh
  ab.sh        A/B 按 ABBA 顺序交替多轮对比     run-c.sh   C（系统 ping）
  summarize.py / summarize_c.py / report.py      汇总多轮结果、生成报告表格
config/nic.env 自动探测生成的网卡参数
docs/          REPORT.md（结果）、WORKLOG.md（开发记录）
logs/          运行日志与 JSON 报告
```

**A 与 B 共用除调度以外的全部代码**（数据面、发送函数、协议解析、TimerHeap、直方图、维护动作、报表），
这样 A − B 减出来的才是抽象层成本。

## 3. 设计要点与亮点

- **unsafe 全部收在 `dpdk` 一个 crate 里**，每处都有 `// SAFETY:`。上层（rt、A、B）是纯安全 Rust。
  - `Mbuf` 是唯一所有者，Drop 时归还 mempool；不实现 `Clone`；内含裸指针 → 自动 `!Send`。
  - `Mempool` 创建后泄漏为 `&'static`：从类型上保证 mbuf 不会比池活得久。
  - `Port` 刻意 `!Send + !Sync`：同一队列上的 rx/tx 不是线程安全的，交给编译器保证。
  - `RxBurst` 逐个交出所有权，没取走的在 Drop 时释放；`tx` 失败时把 mbuf 原样还给调用者。
- **Waker 的内存安全**：std 规定 `Waker: Send + Sync`，而本 runtime 是单线程的。
  wake 时通过线程局部变量确认"当前线程正在运行的就是这个 runtime"（比对 rt_id），否则 abort——
  绝不会在别的线程上碰非线程安全的就绪队列。任务槽带代数，过期 waker 唤醒不到复用了同一槽位的新任务。
- **固定容量、零热路径分配**：任务槽、就绪队列在创建时分配好，永不扩容；timer 槽位复用；
  每包只做一次 mempool 取 mbuf（cache 命中）。
- **timer 精确到 TSC**：二叉堆（64 个 timer，O(log n) 且无槽粒度误差）；`sleep` 返回
  `SleepInfo { deadline, fired_at }`，用于统计 sleep 误差与段③；取消是 O(1)（标记后在弹出时回收）。
- **ICMP 校验和**：对"id / seq / TSC 为 0"的模板预先算好反码部分和，每包只加 6 个 16 位字
  （RFC 1624 的思路）。由于反码加法满足交换律，结果与全量重算**逐位相同**，单测用 100 万组随机数据交叉验证。
- **ARP 应答**：原地改写收到的 request 再发回（实测对端确实会通过 ARP 问我们的 MAC）。
- **ENA 专项**：igb_uio + `wc_activate=1` 打开写合并（LLQ 依赖它；用 PAT 表确认了 BAR2 为 write-combining）；
  主循环周期性调用 `rte_timer_manage()`（ENA watchdog 依赖应用驱动 rte_timer）；注册 reset 事件回调；
  空闲时主动 `tx_done_cleanup`，尽量不让 TX 回收落进段①。
- **测量方法本身经过验证**（详见 §4）：发现并修正了 rdtsc 乱序执行造成的系统性偏差。

### 测试与诊断工具

```bash
cargo test --release --workspace        # 15 个测试，不需要网卡 / root
```

| crate | 测试内容 |
|---|---|
| `rt`（7 个） | 用 `Runtime::run_offline()`（只跑 executor + timer，不驱动网卡）：sleep 按 deadline 顺序醒来且不早到；Mailbox 顺序交接；poll 期间自唤醒 1000 次不丢；**过期 waker 不会唤醒复用同一槽位的新任务**（做过变异测试：去掉代数检查后该测试失败）；drop runtime 时释放未完成 task 持有的资源（对应 mbuf 归还）；取消的 sleep 不误触发；死锁检测 |
| `pingproto`（5 个） | 增量校验和与全量重算在 100 万组随机数据上逐位一致；帧布局与 IPv4 头校验和；reply 解析；ARP 原地应答 |
| `pingkit` / `timerq`（3 个） | 直方图分桶边界与分位数精度；timer 堆顺序 |

- `cargo build --release -p async-ping --features probe`：把段②拆成"分类+投递+wake / 回到 executor+出队 / poll 到恢复"三个子段打印（诊断用，默认关闭）。
- `scripts/report.py --a <A.json> --b <B.json> [--c <C.json>...]`：从运行结果生成 `docs/REPORT.md` 里的表格。

## 4. 测量方法

### 4.1 四个时刻（A 与 B 语义相同、调用同一个打点函数）

| | A | B |
|---|---|---|
| **T0** | `send()` 入口（共用的 `Sender::send` 第一行） | 循环判定"该发了"后调用同一个 `Sender::send` 的第一行 |
| **T1** | `tx_burst` 返回（同一函数内） | 同左 |
| **T2** | `rx_burst` 返回（runtime 主循环） | `rx_burst` 返回（B 的循环）；同一批包共用一个 T2 |
| **T3** | task 从 `mailbox.recv().await` 恢复后的第一行 | 状态机拿到 reply、可以开始算延迟的那一刻 |

- **进程内耗时 = (T1 − T0) + (T3 − T2)**（排名指标）；端到端 = T3 − T0。
- **sleep 误差** = timer 发现到期的时刻 − deadline；**段③** = 发现到期 → 下一个 T0。
- 按 SPEC 的 loop 形状，样本在 sleep **之后**的 `record(reply)` 处记录，reply 的 mbuf 在 sleep 期间一直被持有。

### 4.2 为什么用 `rdtscp` 而不是 `rdtsc`（关键发现）

最初用 `rdtsc` 打点，A − B 的段② p50 是 **+120 ns**。把段②拆开后发现：executor 的出队 + poll 只有约 20 ns，
大头在"分类"这一步——读网卡刚 DMA 进来的包数据，是一次 cache miss（约 100 ns）。
B 从分类到 T3 只隔几条指令，而 **rdtsc 不是序列化指令**：乱序执行可以在 miss 结束前就把 rdtsc 执行掉，
这 100 ns 就从 B 的段②里"消失"了；A 从分类到 T3 之间有几百条指令，乱序窗口塞不下，只能等 miss 结束。
改用 `rdtscp`（等前面所有指令完成、所有读都落地才读 TSC）后，B 的段② p50 从 10 ns 变成 120 ns，
**A − B 的段② p50 从 120 ns 变成约 10 ns**。所以正式版的所有打点都用 `rdtscp`（每次约 16 ns，两边相同）。

### 4.3 段①的写合并效应

ENA 每发一个包，先执行一次 sfence（`wmb()`），再把 128 B 的 LLQ 条目推进写合并内存；
sfence 要等上一个包的写合并缓冲排空（实测约 250 ns）。对端的中断合并让 reply 成批到达，
多个 session 的 sleep 同时到期、发送扎堆：
- B 的循环连续处理到期的 session，两次发送只隔几十纳秒 → 约 22% 的发送在段①里等 flush（约 300 ns）；
- A 的两次发送之间隔着"record + 调度 + poll"（≥ 250 ns）→ flush 在被测段之外自然完成。
- **在相同的发送间隔下，两边的段①完全相同**（报告里按间隔分档给出）。所以 A 在 p99 上"更快"并不是 runtime 更快，
  而是硬件等待被计入的段不同。报告同时给出排名指标原值与这个拆解。

### 4.4 A / B 交替多轮

对端内核的中断合并状态每次运行都不同（同样 64 路，端到端 p50 有时 160 µs、有时 245 µs），
会改变 reply 的到达批次，从而影响 p99。所以除了 SPEC 要求的 10 分钟单轮之外，
还用 `scripts/ab.sh` 按 ABBA 顺序交替跑多轮，报告各轮中位数与 A − B。

### 4.5 C 为什么能和 A 相减

- **同一路径**：同一对端、同一子网，只是本端换成内核网卡（device-number 0）；
- **同样的帧**：`ping -s 64` → 106 B，与 A 相同；
- **同样的并发、相近的速率**：64 个 ping 进程 = 64 路（ICMP id 各不相同），每路 1 ms 间隔，聚合约 6.4 万包/秒
  （iputils 的 `-i` 以整数毫秒计，实测 `-i 0.0005` 会退化为"收到即发"）。为严格可比，A 也在同速率下补测一轮（`--delay-us 800`）；
- **同样的口径**：默认用 `ping -U`（用户态到用户态），对应 A 的 T3 − T0。ping 默认的口径是"用户态发送 → 内核收包时间戳"，
  不含唤醒进程与拷贝到用户态，会低估内核路径的开销；两种口径都给出；
- **为什么速率必须对齐**：实测对端 RTT 随速率非单调变化（1 路 62 µs、8 路 160 µs、32 路 107 µs、64 路 245 µs），
  是对端网卡驱动的自适应中断合并。速率不同，对端状态就不同，相减就没有意义。

## 5. 超时、丢包与异常处理

- **超时**：默认 `--timeout-us 10000`（10 ms，约为正常 RTT 的 40–150 倍）。超时由维护节拍（每 100 µs）扫描，
  实际生效的超时落在 [10 ms, 10.1 ms]。**超时的请求计为丢失（timeouts）**，不进入延迟分布；session 照常 delay 后发下一个 seq。
- **迟到**：超时之后才到的 reply，用每个 session 最近 4 个超时 seq 识别，计为 `late`，释放 mbuf，
  绝不会被当成新 seq 的回复。其他对不上的 echo reply 计为 `unexpected`。
- **对账**：报告里打印 `sent − received − timeouts − in-flight-at-end`，必须为 0；丢包 = timeouts，其中多少最终迟到收到也一并给出。
  用 `--timeout-us 150`（比 RTT 还短）做过压力验证：两边都是 timeouts = late、对账为 0、零泄漏。
- **AWS 静默丢包的证据**：报告给出 ENA 的 `bw/pps/conntrack/linklocal_allowance_exceeded` 在本次运行中的增量。
- **零 mbuf 泄漏核对**：mempool 刚创建（端口未启动）时记下 avail 作为初值；结束时先 drop 所有 task / 信箱 / 持有的 reply，
  再 `tx_done_cleanup` + `stop` 端口（PMD 归还 RX / TX 环上的 mbuf），然后比较 avail。泄漏非 0 时进程退出码为 3。
- **网卡 reset**：注册了 `RTE_ETH_EVENT_INTR_RESET` 回调；ENA watchdog（保活超时、TX 完成丢失）触发时，
  程序停止并照常输出统计、核对 mbuf，退出原因写在报告里。
- **SIGINT / SIGTERM**：不依赖它们停止；收到时走与到时相同的收尾流程（停止发送 → 等在途请求 → 统计 → 核对）。
- **TX 环满 / mempool 取不到 mbuf**：计数后 1 µs 再重试（A 与 B 相同），不丢弃 session。

## 6. 怎么跑

```bash
git clone <repo> ~/dpdk-async-ping && cd ~/dpdk-async-ping
scripts/setup.sh              # 一键环境：工具链、Rust、DPDK、igb_uio、大页、核隔离、中断
sudo reboot                   # 仅第一次：核隔离与大页的启动参数需要重启生效
scripts/check-env.sh          # 重启后自检（只读）
scripts/run.sh A --delay-us 500 --duration-sec 60      # 答辩现场的 60 秒
scripts/run.sh A --delay-us 500 --duration-sec 600     # 10 分钟主考核
scripts/run.sh B --delay-us 500 --duration-sec 600
scripts/run.sh C --duration-sec 60                     # 系统 ping
scripts/ab.sh 3 60                                     # A/B 交替 3 对，每轮 60 秒
```

- `run.sh` 会自动：重新绑定网卡（重启后网卡会回到内核）、增量编译、以 root 运行（无 IOMMU 时 DPDK 需要读物理地址）、
  把输出写入 `logs/<A|B>-<时间>.log`，报告写入同名 `.json`。
- A / B 的参数：`--delay-us`、`--duration-sec`（必填），`--sessions`（默认 64）、`--payload`（默认 64）、
  `--timeout-us`、`--progress-sec` 等，见 `--help`。

## 7. 运维事项

- **CPU 规划**（4 核无超线程）：核 3 由 `isolcpus/nohz_full/rcu_nocbs` 隔离，runtime 独占；核 0–2 给 OS、SSH、C 和进度上报线程（核 1）。
  实测核 3 在 busy loop 下每秒只被打断约 2 次。
- **网卡**：device-number 0（SSH 所在）永远留给内核；`bind.sh` 内置保险，拒绝绑定 SSH 所在的网卡。
  `unbind.sh` 可把 DPDK 网卡还给内核。
- **大页**：启动参数预留 1024 × 2 MiB；EAL 使用 `--in-memory`，进程退出后不在 `/dev/hugepages` 留文件。
- **长测期间不要在本机编译**：编译会占满核 0–2 并冲刷与核 3 共享的 L3，污染尾延迟。
- **日志**：`logs/` 下每次运行一对 `.log` + `.json`；`logs/ab-*/summary.txt` 是交替对比的汇总。

## 8. 换一块网卡要改哪几行

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

## 9. 版本与环境

| | |
|---|---|
| 机器 | AWS c8a.xlarge（AMD EPYC 9R45，4 核无超线程，7.6 GiB），东京 apne1-az4 |
| OS | Amazon Linux 2023，内核 6.18.48 |
| DPDK | **25.11.3**（最新 LTS 分支；只编译 `net/ena`、`net/null`、`net/ring`） |
| 内核模块 | igb_uio（dpdk-kmods @ 9b182be），`wc_activate=1` |
| Rust | **1.98.1**（`rust-toolchain.toml` 固定），release：`lto=fat`、`codegen-units=1`、`panic=abort`、`target-cpu=native` |
| 依赖 | bindgen / cc / pkg-config（构建期）；clap、libc、serde（非热路径）。**没有任何带 executor / reactor 的 runtime**，也未使用 `futures-util` |

## 10. 提交方式

以 GitHub 仓库链接或 zip 包的形式交给联系人（二者内容相同）。
