# dpdk-async-ping

用 Rust 写的**单核、kernel-bypass 的 async runtime**（`crates/rt`），以及跑在它上面的 ICMP ping 客户端。
自研 executor / Waker / TSC timer / DPDK poll-mode reactor，不依赖任何现成 runtime。

| 标签 | 程序 | 说明 |
|---|---|---|
| **A** | `async-ping` | 跑在 `rt` 上，64 个 session 各是一个独立的 async task |
| **B** | `raw-ping` | 行为完全相同，手写 busy-poll 循环 + 状态表，不经过 runtime |
| **C** | 系统 `ping` | 走内核网卡，作为"不做 kernel bypass"的参照 |

**结果速览**（64 session、delay 500 µs、各连续 10 分钟；完整报告见 [`docs/REPORT.md`](docs/REPORT.md)，答辩提纲见 [`docs/DEFENSE.md`](docs/DEFENSE.md)）：

| | 样本 | 丢包 | mbuf 泄漏 | 进程内 p50 | p99 | p99.9 | 段②（接收侧）p50 / p99 |
|---|---|---|---|---|---|---|---|
| A async-ping | 5496 万 | 0 | 0 | 190 ns | 370 ns | 481 ns | 140 / 310 ns |
| B raw-ping | 5237 万 | 0 | 0 | 180 ns | 481 ns | 589 ns | 120 / 280 ns |
| **A − B**（插值，见下） | | | | **+11.0 ns** | **−112.6 ns** | −112.8 ns | **+11.5 / +25.1 ns** |

- **抽象税是正的，约每个请求 30 ~ 40 ns**：把每个请求在我们自己代码里花的时间（① + ② + ③）按平均值加起来，A 比 B 多 37 ns；
  A / B 交替 10 对，逐对差值的平均是 +39.5 ns（95% 区间 +32 ~ +47，10 对全部为正）。这是最稳的一个数：重启前后、各个时段都在 +29 ~ +41 之间。
- **p99 的 A − B 为负不代表 runtime 更快**：有一段约 250 ns 的 CPU 停顿（等上一次发送最后那次写网卡寄存器的操作完成），
  在 B 那边落在计分的段①里，在 A 那边落在不计分的段③里。用三个实验定位了它（§4.3）；把它统一移到 T0 之前再比，
  **A − B 在每个分位数上都是正的：p50 +20 ns，p99 +41 ns，平均 +25 ns**，此时两边的段①相差不到 1 ns。
- **这些数字有多可信**：这台机器的时间戳分辨率是 10 ns（表里的差值用插值分位数给出）；主考核同时导出了全部原始样本，用分块自助法给出 95% 置信区间；
  另外 A / B 交替跑了 10 对来看不同运行之间的波动：p50 的差值 10 对全为正（平均 +13.5 ns），p99 的差值 10 对全为负（平均 −107 ns）。
- **p99 的 A − B 不是一个稳定的数**：把机器重启后复测并连续监测 1 小时（200 次运行），10 分钟主考核原样复现（p50 +14.0，p99 −102），
  但有约 20 分钟 A 的尾部变重，p99 的差值变成 **+52**，之后回到 −60 ~ −80；同一时段 B 不受影响，A 的三段总和也没变——时间只是在三段之间挪动（报告 §3.4）。
  稳定的是 p50 的差值（+11 ~ +20 ns）和每请求总账。
- **可靠性**：除 10 分钟主考核外，A、B 各连续跑了 30 分钟（零泄漏；A 1.64 亿个请求零丢包；B 1.59 亿个请求里有 1 个超时，
  网卡计数器表明它不是在本机丢的，见报告 §8）；故障注入 20 个场景 × A / B 共 40 项全部干净收场
  （超时与回复抢跑、对端消失、信号、进程被冻结 2 秒、mbuf 耗尽、边界参数、重复启动……每项都核对零泄漏、对账为 0）。
- kernel bypass 的收益（A 对系统 ping）：单路 p99 68 µs 对 148 µs；64 路 p99.99 0.33 ms 对 1.51 ms，max 0.45 ms 对 16.8 ms。

开发过程中的每个决定、实验和走过的弯路见 [`docs/WORKLOG.md`](docs/WORKLOG.md)。

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
while !sh.stopping.get() {
    let stamp = send(&sh, id, seq).await;         // T0 → T1，段①（T0 = send() 的第一行）
    let reply = wait_reply(&sh, id, seq).await;   // T2 → T3，段②；超时也从这里返回 Err(Timeout)
    let t3 = rdtsc();                             // T3：本 task 从 wait_reply().await 恢复执行
    woke = sleep_until(t3 + sh.delay).await;      // 样本之外；期间 reply（及其 mbuf）被本 task 持有
    record(&sh, id, seq, stamp, reply, t3);       // 记录样本；reply 在这里 Drop → mbuf 归还
    seq = seq.wrapping_add(1);
}
```

（为突出形状省略了两行统计代码；与 SPEC §5 的区别只有：`send` 内部自己重试而不返回错误，
超时不用 `?` 退出循环——超时的请求计为丢失，session 照常继续。）

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
  campaign.sh  一键重测报告用到的全部数据（约 2 小时）
  session.sh   重启后 / 另一天的独立复测会话     drift.sh   A/B 每 20 秒交替的长时间监测
  ci.py        由原始样本算置信区间、按批内位置拆分段②
  fault.py     故障注入矩阵（20 个场景 × A / B）
  check-compliance.sh   把 SPEC 的硬性要求逐条机器核对
  summarize.py / summarize_c.py / report.py / make_report.py / plots.py   汇总结果，生成 docs/REPORT.md 的全部表格和图
config/nic.env 自动探测生成的网卡参数
docs/          REPORT.md（结果）、DEFENSE.md（答辩准备）、WORKLOG.md（开发记录）、img/（图）
logs/          运行日志与 JSON 报告
  final/       主考核（10 分钟 A / B）、置信区间 ci.json、同速率与单路的辅助对比
  ab-*/        A / B 交替各轮                diag/     诊断口径（T0 前 sfence / mfence / N 次写入）
  fault/       故障注入每个场景的日志与汇总     soak/     30 分钟连续运行
  C-*/         系统 ping 的汇总               probe/    probe 构建的诊断结果
  sessions/    重启后的复测会话（主考核、交替、1 小时监测）
  setup/       环境搭建与首次上线验证的日志     history/  早期版本的运行（保留作对比）
```

**A 与 B 共用除调度以外的全部代码**（数据面、发送函数、协议解析、TimerHeap、直方图、维护动作、报表），
这样 A − B 减出来的才是抽象层成本。

## 3. 设计要点与亮点

- **unsafe 有明确的边界**：共 57 处，每处都有 `// SAFETY:` 说明（`scripts/check-compliance.sh` 会清点并核对）。
  - 45 处在 `dpdk`：对 DPDK 的 FFI 调用，以及 `rdtscp` 等指令。上层拿到的是下面这些安全的类型。
  - 6 处在 `rt`：自己实现 Waker 绕不开 unsafe（`RawWaker` 的 vtable 是 4 个 `unsafe fn`，但它们只把指针当整数用、从不解引用）；另有 2 处解引用"当前线程正在运行的 runtime"的线程局部指针。
  - 4 处在 `pingkit`（注册信号处理函数、绑核、读内核单调时钟、诊断开关里的 volatile 写），A、B 的 `main` 各 1 处（最后调用 `eal.cleanup()`）。
  - 协议解析（`pingproto`）、timer 堆（`timerq`）、session 逻辑、统计与报表不含 unsafe。
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
- **测量方法本身经过验证**（详见 §4）：发现并修正了 rdtsc 乱序执行造成的系统性偏差、启动阶段对 sleep 误差的污染；
  标定出时间戳的分辨率只有 10 ns 并用插值分位数应对；给 A − B 配了置信区间和多轮交替对比；
  主循环自带"被外部打断"的检测，最大值尖刺可以直接归因。
- **收到的包先验明正身**：echo reply 必须来自对端 IP 才会交给 session；record 时再核对它带回的时间戳。

### 相位设计（64 个 session 的相位关系）

- **初始相位**：64 个 session 均匀错开在一个 delay 周期内（相邻 7.8 µs），不让它们同时发、同时回。
- **稳态相位不由我们决定**：这是闭环（收到 reply 再 sleep 再发），每个 session 的周期 = RTT + delay。
  对端网卡的中断合并会把相近时刻到达的请求攒成一批处理、一批回复，于是这些 session 的 T3 相近、sleep 同时到期、下一次发送也挤在一起。
  实测两边都有约 80% 的发送距上一次发送不到 2 µs；非空 rx_burst 里 1 个包占 77%、2 个占 18%、3 个占 4%、4 个占 0.6%。
- **没有做的事**：给 delay 加随机抖动来打散相位。它会改变 `--delay-us N` 的语义，而且对端仍会重新把它们捏成批次。
- **为什么要在意**：批次越大，同一批里靠后的包等得越久（段②的 p99，报告 §5 按批内位置做了拆分），连续发送也越多（段①的停顿，见 §4.3）。
  runtime 的应对是"每分发一个包就立刻 poll 它的 task"，让靠前的包不被整批拖慢。

### 取舍：时间花在哪、没花在哪

| 投入了 | 理由 |
|---|---|
| A 与 B 共用除调度之外的全部代码 | 否则 A − B 不是抽象层成本 |
| 验证测量工具本身（rdtsc → rdtscp、起点延后、停顿检测） | 这三处任何一处不处理，报出来的数字都是错的 |
| 把每个异常数字追到原因（段①尾巴 → CPU 写入队列被设备写入堵住；sleep 误差 max → 启动假象；外来回复 → 源 IP 检查） | SPEC 问的是"你是否知道它收在哪里" |
| 给数字配不确定度（原始样本、分块自助、交替 10 对）；主动做故障注入 | 一个没有区间的差值没法判断是不是噪声；"没出事"不等于"出了事也能收场" |
| 对账与零泄漏的可核对性（发送对账、收包对账、mbuf 记账） | 硬门槛，且要能"自洽解释" |

| 没有投入 | 理由 |
|---|---|
| 进一步压榨段②（RefCell → Cell、泛型任务存储省 vtable、缩短 T3 之后的收尾） | 接收侧的税在 p50 上只有一格（约 10 ns），接近测量分辨率；方向已在报告 §5 里量化 |
| 每个 session 一个预构建模板 mbuf + refcnt（省掉分配和 106 B 拷贝） | A、B 同样受益，不改变 A − B；且要处理"网卡还在读时不能改模板" |
| 攒批发送、或人为拉开 B 的发送间隔 | 前者改变 T1 的语义，后者是故意拖慢 B，都会让 A − B 失真 |
| 时间轮、每包一个超时 timer | 只有 64 个 timer；超时用 100 µs 一次的扫描即可，没有每包的堆操作 |

### 关于"语言必须是 Rust"

全部逻辑都是 Rust。仓库里唯一的 C 是 `crates/dpdk-sys/src/shim.c`（约 20 行）：DPDK 的 `rte_eth_rx_burst` / `tx_burst` / `rte_pktmbuf_alloc` / `free`
在头文件里是 `static inline`，库里没有符号，bindgen 无法直接绑定，只能包一层普通函数（bindgen 自己的 `wrap_static_fns` 也是生成同样的 C）。
`scripts/` 下的 bash / Python 只是环境搭建与结果汇总工具，不参与运行。

依赖树里没有任何现成 runtime：

```bash
$ grep -ciE '^name = "(tokio|async-std|smol|glommio|monoio|futures|futures-util|futures-executor|async-executor)"' Cargo.lock
0
```

### 测试与诊断工具

```bash
cargo test --release --workspace        # 34 个测试，不需要网卡 / root
scripts/fault.py                        # 故障注入：20 个场景 × A / B，需要网卡
scripts/check-compliance.sh             # 把 SPEC 的硬性要求逐条机器核对
```

| crate | 测试内容 |
|---|---|
| `rt`（10 个） | 用 `Runtime::run_offline()`（只跑 executor + timer，不驱动网卡）：sleep 按 deadline 顺序醒来且不早到；Mailbox 顺序交接；poll 期间自唤醒 1000 次不丢；**过期 waker 不会唤醒复用同一槽位的新任务**（做过变异测试：去掉代数检查后该测试失败）；drop runtime 时释放未完成 task 持有的资源（对应 mbuf 归还）；取消的 sleep 不误触发；死锁检测；**在别的线程上、或在另一个 runtime 里调用 Waker 会 abort**（测试程序把自己再启动一遍，在子进程里真的触发，父进程核对它是被 SIGABRT 终止的）；runtime 结束后迟到的 wake 被无害地忽略 |
| `pingproto`（6 个） | 增量校验和与全量重算在 100 万组随机数据上逐位一致；帧布局与 IPv4 头校验和；reply 解析；**别的主机发来的 echo reply 即使 id / seq 合法也不会交给 session**；ARP 原地应答 |
| `pingkit`（15 个） | 直方图分桶边界与分位数精度；**插值分位数**（模拟"时间戳只能取 10 ns 整数倍"的时钟，核对它能还原一格以内的差别，并如实写明在分布硬边界处误差可到半格）；原始样本的打包、写满即停、文件布局；时间戳核对不符的回复被计数并排除；命令行参数的边界值 |
| `timerq` / `dpdk`（3 个） | timer 堆顺序；主循环停顿检测器；时钟标定 |

故障注入（`scripts/fault.py`，结果见报告 §8）覆盖单元测试够不着的部分：超时与回复抢跑、对端消失、SIGINT / SIGTERM、进程被冻结 2 秒、
mbuf 池耗尽、最小 / 最大载荷、1 个 / 256 个 session、delay 为 0、重复启动、各种非法参数。

- `cargo build --release -p async-ping -p raw-ping --features probe`（诊断用，默认关闭，会多几次时钟读取）：
  把段②拆成"分类+投递+wake / 回到 executor+出队 / poll 到恢复"，把段①拆成"取 mbuf / 写包 / tx_burst"，并按发送队列位置统计慢 `tx_burst`。
- `--samples <文件>`（默认关闭）：把每个样本的段①、段②、T2 原样导出，供 `scripts/ci.py` 算置信区间。缓冲区启动时一次分配好并预先触发缺页，运行中不分配。
- `--diag-pre-t0 sfence|mfence|stores`（默认关闭，打开后报告标明"不参与排名"）：读 T0 之前多做一件事，用来定位段①尾巴的来源（§4.3）。
- 每份 JSON 报告都带 `env` 字段：构建时的 git 提交、编译器、DPDK 版本、内核启动参数、网卡驱动、时钟标定结果。任何一个数字都能追溯到确切的代码版本。
- `scripts/make_report.py` / `scripts/plots.py`：从 `logs/` 下的 JSON 重新生成 `docs/REPORT.md` 里的全部表格和图。

## 4. 测量方法

### 4.1 四个时刻（A 与 B 语义相同、调用同一个打点函数）

| | A | B |
|---|---|---|
| **T0** | `send()` 的第一行（早于向 runtime 查找端口） | 循环里 `record(上一个 reply)` 之后、调用发送函数之前 |
| **T1** | `tx_burst` 返回（同一函数内） | 同左 |
| **T2** | `rx_burst` 返回（runtime 主循环） | `rx_burst` 返回（B 的循环）；同一批包共用一个 T2 |
| **T3** | task 从 `mailbox.recv().await` 恢复后的第一行 | 状态机拿到 reply、可以开始算延迟的那一刻 |

- **进程内耗时 = (T1 − T0) + (T3 − T2)**（排名指标）；端到端 = T3 − T0。
- **sleep 误差** = timer 发现到期的时刻 − deadline；**段③** = 发现到期 → 下一个 T0。
  SPEC 的"段③ = sleep 到期 → 下一个 T0"里，"到期"也可以理解成 deadline 本身，所以另报一行 **deadline → 下一个 T0**（= sleep 误差 + 段③），两种读法都覆盖。
- 按 SPEC 的 loop 形状，样本在 sleep **之后**的 `record(reply)` 处记录，reply 的 mbuf 在 sleep 期间一直被持有。
- **B 的 T0 为什么不取在"timer 判定到期"的那一刻**：A 的顺序是 sleep 返回 → `record(reply)` → `send()` 入口（T0），
  record 不在 A 的段①里。B 若把 T0 提前到判定到期的那一刻，它的段①就会多包含 record 的开销，两边不再对齐。
  所以两边的 T0 都紧挨在"发送这件事"之前，record 都落在段③里。
- **运行起点定在启动后 1 ms**：创建 runtime、分配直方图、spawn 任务要花约 100 µs。早期版本把起点取在这些准备工作之前，
  前几个 session 的初始 deadline 在主循环开始转之前就已过期，被记成"sleep 迟到 80–190 µs"，污染了 sleep 误差的最大值。

### 4.2 为什么用 `rdtscp` 而不是 `rdtsc`（关键发现）

最初用 `rdtsc` 打点，A − B 的段② p50 是 **+120 ns**。把段②拆开后发现：executor 的出队 + poll 只有约 20 ns，
大头在"分类"这一步——读网卡刚 DMA 进来的包数据，是一次 cache miss（约 100 ns）。
B 从分类到 T3 只隔几条指令，而 **rdtsc 不是序列化指令**：乱序执行可以在 miss 结束前就把 rdtsc 执行掉，
这 100 ns 就从 B 的段②里"消失"了；A 从分类到 T3 之间有几百条指令，乱序窗口塞不下，只能等 miss 结束。
改用 `rdtscp`（等前面所有指令完成、所有读都落地才读 TSC）后，B 的段② p50 从 10 ns 变成 120 ns，
**A − B 的段② p50 从 120 ns 变成约 10 ns**。所以正式版的所有打点都用 `rdtscp`（每次约 18 ns，两边相同；程序启动时会标定并写进报告）。

### 4.3 段①的尾巴不来自 runtime：三个实验

两边的段①执行的是同一个函数，尾巴却不同。用 `probe` 构建把段①拆成三步后（A、B 相同）：
"取 mbuf"和"写包"直到 p99.99 都是平的，**尾巴全部在 `tx_burst` 里**。它有两个来源：

1. **紧跟在上一次发送之后的发送**。对端的中断合并让 reply 成批到达，多个 session 的 sleep 同时到期、发送扎堆。
   B 有约 5% 的发送距上一次发送不到 100 ns，这些发送的段①约 310 ns（正常是 50 ns）；A 相邻两次发送的间隔从不低于 250 ns，没有这样的发送。
   这就是 p99 的 A − B 为负的全部来源。用诊断开关 `--diag-pre-t0`（在读 T0 **之前**多做一件事）做了三个实验来定位它：

   | 读 T0 之前多做的事 | 结果 |
   |---|---|
   | 一条 `sfence` | **没有任何变化**（否定了最初的猜测"卡在驱动的 sfence 上"） |
   | 一条 `mfence`（等此前所有写入真正完成） | B 的慢发送消失，停顿移到段③；A − B 在每个分位数上都变成正的 |
   | N 次普通的内存写入，不带任何栅栏指令 | N ≤ 40：没有变化；N ≥ 48：效果与 `mfence` 相同 |

   三个实验指向同一个机制：发送的最后一步是往网卡寄存器写"门铃"，这次写入约 250 ~ 300 ns 才真正完成；CPU 不等它，
   后面的写入在 CPU 的写入队列（store queue）里排队，门铃没写完它们都出不去；**队列被填满时，CPU 才停下来等**。
   B 两次发送之间做的事少、写入少，队列在下一次发送的中途填满（落在段①）；A 多了调度，写入更多，在走到 T0 之前就填满了（落在段③）。

   示意（1 个字符 = 10 ns；pkt1 / pkt2 是连续要发的两个包）：

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

   两边的第二个包都在约 400 ns 时交给网卡，A 并没有更快；这段停顿在 B 那边算进段①（计分），在 A 那边算进段③（不计分）。
   按平均值算总账，A 每个请求比 B 多 37 ns；把停顿统一移到 T0 之前（两边都加 `mfence`）再比，A − B 是 p50 +20 ns、p99 +41 ns（报告 §4）。
   需要如实说明：这台虚拟机没有开放硬件性能计数器，"写入队列被堵住"是与三个实验都吻合的解释，不是直接观测到的。
2. **发送队列的位置**。即使间隔足够，仍有一小部分 `tx_burst` 要 150 ~ 270 ns。按"发送计数 % 32"统计，
   慢的集中在余数为 3、7、11、…、31 的位置：每 4 个条目（512 B 设备内存）一次，并以 32 个条目（一个 4 KB 页）为周期起伏。
   A 和 B 的规律相同——这是网卡 / PCIe 一侧的行为，软件控制不了。

**对读数的影响**：第 2 类慢发送的占比在不同的运行之间变化很大（A 实测 0.4% ~ 4%）。段①的分布是两段式的，
所以它的 p99 落在哪一段只看这个占比在 1% 的哪一边：低于 1% 读数是 70 ~ 80 ns，高于 1% 就跳到 130 ns 以上。
程序因此把这个占比直接报出来（"段① ≥ 125 ns 的样本占 x%"），它比 p99 本身更稳定、更有解释力。

### 4.4 这些数字有多可信：分辨率、置信区间、交替多轮

- **时间戳分辨率是 10 ns。** 程序启动时会标定时钟：这台机器的 TSC 读数每 10 ns 才跳一步（一步 26 个周期），所以任何时间差都是 10 ns 的整数倍，
  两个分位数相减也只能得到 10 ns 的整数倍。单个样本有 ±10 ns 的量化误差，但平均值没有偏差。
  为了看清一格以内的差别，程序另外给出**插值分位数**（把同一格的样本看成均匀分布在格内，再线性插值；JSON 里的 `p50_interp` 等），
  单元测试用模拟的量化时钟验证了它。
- **置信区间。** 运行时加 `--samples` 会把全部原始样本导出；`scripts/ci.py` 用分块自助法算 95% 区间：先分别求 A、B 的分位数再相减；
  以 20 秒的时间块为单位重抽（样本并不独立，把 1 亿个样本当成独立的，区间会窄几十倍）。
- **交替多轮。** 对端的状态每次运行都不同（同样 64 路，往返 p50 有时 110 µs、有时 245 µs），会改变 reply 的到达批次，从而影响尾部。
  这部分波动单次运行的区间看不到，所以用 `scripts/ab.sh` 让 A、B 交替各跑 10 轮，对逐对的差值做统计，并核对先后顺序没有影响。
- **换一次开机、换一个时段。** `scripts/session.sh` 在重启后把主考核和交替 10 对重测一遍（结果单独存放）；`scripts/drift.sh` 让 A、B 每 20 秒交替一次连续跑 1 小时。
  结论：p50 的差值和每请求总账稳定；尾部分位数的差值会随时段大幅变化，连符号都会变，不宜单独引用。

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
- **外来回复**：echo reply 必须"目的 IP 是我、源 IP 是对端"才会进入分发逻辑。实测遇到过别的主机发来的 echo reply（id=16509）；
  这类包计为 `foreign` 并记下源 IP，**绝不交给 session**（否则只要 id / seq 碰巧合法，就会产生一个错误的延迟样本）。
- **回复真伪核对**：`record` 时检查回复带回的发送时间戳是否等于我们发这个请求时写入的 T0，不符计为 `tsc-mismatch`（在被测段之外），
  并且**不进入延迟分布**——它不是这个请求的应答，用它算出来的延迟没有意义。报告会核对"样本数 = received − tsc-mismatch"。
- **收包对账**：收到的每个包必须恰好落入一类，报告打印 `rx − (received + late + unexpected + foreign + other + arp)`，必须为 0。
- **最大值尖刺的归因**：主循环自带停顿检测——一轮什么都没干的空轮询超过 1 µs、或收到包那一轮在取包之前超过 10 µs，
  就记为一次"被外部打断"。A 和 B 的次数几乎相同（约每秒 215 次、累计占 0.07%，平均 3 µs），sleep 误差与进程内耗时的最大值都能对上其中最长的一次，
  说明这些尖刺来自虚拟机宿主机而不是程序本身。
- **对账**：报告里打印 `sent − received − timeouts − in-flight-at-end`，必须为 0；丢包 = timeouts，其中多少最终迟到收到也一并给出。
  用 `--timeout-us 150`（比 RTT 还短）做过压力验证：两边都是 timeouts = late、对账为 0、零泄漏。
- **AWS 静默丢包的证据**：报告给出 ENA 的 `bw/pps/conntrack/linklocal_allowance_exceeded` 在本次运行中的增量。
- **零 mbuf 泄漏核对**：mempool 刚创建（端口未启动）时记下 avail 作为初值；结束时先 drop 所有 task / 信箱 / 持有的 reply，
  再 `tx_done_cleanup` + `stop` 端口（PMD 归还 RX / TX 环上的 mbuf），然后比较 avail。泄漏非 0 时进程退出码为 3。
- **网卡 reset**：注册了 `RTE_ETH_EVENT_INTR_RESET` 回调；ENA watchdog（保活超时、TX 完成丢失）触发时，
  程序停止并照常输出统计、核对 mbuf，退出原因写在报告里。
- **SIGINT / SIGTERM**：不依赖它们停止；收到时走与到时相同的收尾流程（停止发送 → 等在途请求 → 统计 → 核对）。
- **TX 环满 / mempool 取不到 mbuf**：计数后 1 µs 再重试（A 与 B 相同），不丢弃 session。
- **同一张网卡只允许一个进程驱动**：igb_uio 不阻止第二个进程打开同一个设备，两个进程同时驱动会互相破坏收发队列。
  程序启动时先拿一把文件锁（`/run/bqping-<PCI>.lock`），拿不到就报错退出；锁随进程结束自动释放，崩溃后也不需要手工清理。
- **以上每一条都有对应的故障注入场景**（`scripts/fault.py`，报告 §8）：每个场景都核对没有崩溃、零泄漏、两条对账为 0。

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
scripts/ab.sh 10 60                                    # A/B 交替 10 对，每轮 60 秒
scripts/fault.py                                       # 故障注入矩阵（约 4 分钟）
scripts/check-compliance.sh                            # 逐条核对 SPEC 的硬性要求
scripts/campaign.sh                                    # 一键重测报告里的全部数据并重新生成报告（约 2 小时）
```

- 也可以不经过脚本，在仓库根目录直接运行可执行文件（网卡参数自动从 `config/nic.env` 读取；网卡需已 `scripts/bind.sh`）：
  `sudo target/release/async-ping --delay-us 500 --duration-sec 60`
- `run.sh` 会自动：重新绑定网卡（重启后网卡会回到内核）、增量编译、以 root 运行（无 IOMMU 时 DPDK 需要读物理地址）、
  把输出写入 `logs/<A|B>-<时间>.log`，报告写入同名 `.json`。
- A / B 的参数：`--delay-us`、`--duration-sec`（必填），`--sessions`（默认 64）、`--payload`（默认 64）、
  `--timeout-us`、`--progress-sec` 等，见 `--help`。

## 7. 运维事项

- **CPU 规划**（4 核无超线程）：核 3 由 `isolcpus/nohz_full/rcu_nocbs` 隔离，runtime 独占；核 0–2 给 OS、SSH、C 和进度上报线程（核 1）。
  实测核 3 在 busy loop 下，来自本机内核的中断每秒只有约 2 次；另有虚拟机宿主机造成的短暂停顿每秒约 215 次（主循环的停顿检测，累计占 0.07%），这部分隔离不掉。
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

公开仓库：<https://github.com/Chen-Shuai-CS/dpdk-async-ping>（也可以打包成 zip 交给联系人，内容相同）。
