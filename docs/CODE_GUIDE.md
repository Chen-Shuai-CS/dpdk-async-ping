# 代码解读：dpdk-async-ping

这份文档回答两个问题：**项目里的每一块代码是用来做什么的**，以及**每一项重要功能对应的代码在哪里**。

- 仓库在 `~/dpdk-async-ping`。文中的路径都相对于仓库根目录；写成 `文件:行号` 的，行号以标签 `v2` 的代码为准（之后改动代码，行号会略有偏移，按函数名找即可）。
- 不解释 Rust / DPDK / async 的基础概念，那些在 `~/Claude知识.md` 里；这里只讲"本项目的代码"。
- 仓库里的副本是 `docs/CODE_GUIDE.md`，内容相同。

## 怎么读

| 你想做的事 | 读哪一章 |
|---|---|
| 先有个整体印象 | 第 1 章（地图）+ 第 2 章（一个请求的一生） |
| 找"某个功能在哪" | 第 1.3 节的速查表 |
| 弄懂 runtime 是怎么工作的 | 第 2 章 + 第 3.5 节 |
| 准备回答"T0 ~ T3 打在哪" | 第 4 章 |
| 准备回答"零泄漏 / 超时 / 安全是怎么保证的" | 第 5 章 |
| 想改点什么 | 第 9 章 |

## 目录

1. 地图：8 个 crate、谁依赖谁、功能速查表
2. 沿着一个请求走一遍：A 和 B 的代码对照
3. 逐个 crate 解读
4. 四个时间戳、三段在代码里的确切位置
5. 横切主题：某件事是怎么保证的
6. 测试：在哪、测什么
7. 脚本：每个脚本做什么，数据怎么流到报告里
8. 日志与数据的目录结构
9. 想改某样东西，该动哪里
10. 读代码时常见的"为什么"

---

## 第 1 章 地图

### 1.1 一句话说清每个 crate

代码分成 8 个 crate（Rust 的"包"），都在 `crates/` 下，一共约 4900 行（含测试）。

| crate | 行数 | 一句话 | 谁在用它 |
|---|---|---|---|
| `dpdk-sys` | 约 130（含 C） | DPDK 的"裸"接口：自动生成的函数声明 + 17 行 C | 只有 `dpdk` |
| `dpdk` | 约 700 | 把 DPDK 包成安全的 Rust 类型：`Eal`、`Mempool`、`Mbuf`、`Port`；还有读时钟 | 其余所有 |
| `pingproto` | 约 400 | 协议：造 echo request、认 echo reply、回 ARP。纯计算，不碰网卡 | `pingkit`、A、B |
| `timerq` | 64 | 一个按 deadline 排序的最小堆 | `rt`、B |
| `rt` | 约 1080（含测试） | **★ runtime 本体**：executor、Waker、timer、主循环、信箱 | 只有 A |
| `pingkit` | 约 1860 | A 和 B **共用**的一切：参数、初始化、发送函数、统计、报表 | A、B |
| `async-ping` | 420 | **A**：64 个 async task 跑在 `rt` 上 | — |
| `raw-ping` | 332 | **B**：手写的循环 + 状态表，不用 `rt` | — |

### 1.2 谁依赖谁

```text
                 async-ping (A)                    raw-ping (B)
                  │    │    │                       │      │
          ┌───────┘    │    └────────┐      ┌───────┘      │
          ▼            ▼             ▼      ▼              ▼
          rt ────▶  timerq        pingkit  ◀───────────  (pingkit, pingproto, dpdk)
          │                        │   │
          │                        │   └──▶ pingproto
          ▼                        ▼
         dpdk  ◀────────────────  dpdk
          │
          ▼
       dpdk-sys ──▶ DPDK 的 C 库（libdpdk，装在 /usr/local/lib64）
```

要点只有一个：**B 不依赖 `rt`**。A 和 B 的区别被限制在"有没有 runtime"这一件事上，其余（初始化、发送、协议、统计）都是 `pingkit` 里的同一份代码。
所以 A − B 测的就是 `rt` 的成本。

### 1.3 功能速查表：某件事的代码在哪

**runtime（题目的核心）**

| 功能 | 位置 | 备注 |
|---|---|---|
| 主循环：收包 → 跑就绪队列 → 触发 timer → 维护节拍 | `crates/rt/src/runtime.rs:162` `Runtime::run` | 整个 runtime 的心脏，约 50 行 |
| 就绪队列（环形数组 + 去重标记） | `crates/rt/src/executor.rs:13` `ReadyQueue` | `push` 在 31 行，`pop` 在 47 行 |
| 任务槽、代数 | `executor.rs:61` `TaskSlot` | 代数 `gen` 用来识别过期的 Waker |
| 把就绪队列跑空（出队 → poll） | `executor.rs:106` `run_ready` | |
| Waker：怎么造、wake 时做什么 | `executor.rs:170 ~ 208` | `make_waker` 173 行，`waker_wake` 183 行 |
| 在别的线程 wake → abort | `executor.rs:183 ~ 208` + `runtime.rs:57 ~ 82` | 靠线程局部变量 `CURRENT` 判断 |
| timer：登记、触发、取消 | `crates/rt/src/timer.rs` | `Timers::fire` 73 行，`Sleep::poll` 107 行，取消在 `Drop`（139 行） |
| 信箱：reactor 把回复交给 task | `crates/rt/src/sync.rs:17` `Mailbox` | `put` 34 行，`Recv::poll` 68 行 |
| 应用怎么接入 runtime | `runtime.rs:14` `trait Driver` | A 的实现在 `async-ping/src/driver.rs:136` |
| task 里怎么拿到网卡 | `runtime.rs:86` `with_port` | |
| 不带网卡的运行（给单元测试用） | `runtime.rs:220` `run_offline` | |

**A 和 B**

| 功能 | A（async-ping） | B（raw-ping） |
|---|---|---|
| 一个 session 的逻辑 | `main.rs:86` `session`（一个 async fn） | `main.rs:26` `State` 枚举 + `Raw` 的三个方法 |
| 发送 | `main.rs:31` `send` | `main.rs:157` `on_timers` 里 |
| 收到回复 | `driver.rs:144` `on_packet` → 信箱 → `main.rs:60` `wait_reply` | `main.rs:123` `on_reply` |
| 超时扫描 | `driver.rs:192` `on_tick` | `main.rs:205` `scan_timeouts` |
| 记录样本 | `main.rs:67` `record` | `on_timers` 里（163 行附近） |
| 主循环 | 在 `rt` 里（`Runtime::run`） | `main.rs:270` 的 `loop` |
| 启动与收尾 | `main.rs:113` `main` | `main.rs:224` `main` + `main.rs:304` `finish` |

**两边共用（都在 `crates/pingkit/src/`）**

| 功能 | 位置 |
|---|---|
| 命令行参数、合法性检查 | `args.rs:9` `Args`，检查在 `args.rs:190` `resolve` |
| 初始化 DPDK、建 mbuf 池、起端口 | `dataplane.rs:67` `Dataplane::open` |
| 关停、核对 mbuf 泄漏 | `dataplane.rs:96` `Dataplane::shutdown` |
| 单实例锁 | `dataplane.rs:33` `instance_lock` |
| **发送函数（段①的全部代码）** | `sender.rs:65` `Sender::send` |
| 直方图 | `hist.rs:15` `Hist`；插值分位数在 `hist.rs:107` |
| 计数器、各段的统计 | `stats.rs:14` `Counters`，`stats.rs:44` `Stats` |
| 记一个样本 | `stats.rs:130` `on_reply` |
| 记一次 sleep 的误差和段③ | `stats.rs:161` `on_wake` |
| 核对回复带回的时间戳 | `stats.rs:172` `verify_echo` |
| 最终报表（屏幕 + JSON） | `stats.rs:293` `Report`，打印在 `stats.rs:361`，写文件在 `stats.rs:554` |
| 周期性维护（网卡 watchdog、回收 TX） | `house.rs:39` `maintain` |
| 进度上报线程、信号处理 | `live.rs:63` `spawn_reporter`，`live.rs:39` `install_signal_handlers` |
| 原始样本导出（`--samples`） | `samples.rs:26` `SampleLog` |
| 环境信息、时钟标定 | `envinfo.rs:78` `EnvInfo::collect` |
| 诊断开关（`--diag-pre-t0`） | `args.rs:102` `Diag`，执行在 `args.rs:115` |

**更底层**

| 功能 | 位置 |
|---|---|
| 读时钟（所有时间戳都用它） | `crates/dpdk/src/tsc.rs:10` `rdtsc`（实际执行的是 `rdtscp` 指令） |
| 主循环停顿检测 | `tsc.rs:92` `StallWatch` |
| 时钟标定（读一次多少纳秒、步长多少） | `tsc.rs:62` `clock_read_cost` |
| mbuf 的所有权 | `crates/dpdk/src/mbuf.rs:9` `Mbuf`，归还在 `mbuf.rs:104` 的 `Drop` |
| 收一批包 / 发一个包 | `crates/dpdk/src/port.rs:119` `rx_burst`，`port.rs:131` `tx` |
| 造一个 echo request（含增量校验和） | `crates/pingproto/src/lib.rs:132` `write_request` |
| 认出一个包是什么 | `pingproto/src/lib.rs:172` `classify` |
| 回 ARP | `pingproto/src/lib.rs:224` `arp_reply_in_place` |

---

## 第 2 章 沿着一个请求走一遍

这一章按时间顺序，把 A 和 B 各走一遍。读完这一章，就知道程序运行时"现在执行到哪个文件的哪个函数"。

### 2.1 启动（A 和 B 几乎一样）

以 A 为例（`crates/async-ping/src/main.rs:113` 起）：

| 步骤 | 代码 | 做了什么 |
|---|---|---|
| 1 | `Args::load()`（`pingkit/src/args.rs:181`） | 解析命令行；网卡参数没给就读 `config/nic.env`；检查合法性（session 数、载荷大小、mbuf 池够不够） |
| 2 | `install_signal_handlers()`（`live.rs:39`） | Ctrl-C / kill 只置一个标志，主循环看到后走正常的收尾 |
| 3 | `Dataplane::open`（`dataplane.rs:67`） | 拿单实例锁 → 初始化 EAL → 建 mbuf 池并记下"初始可用数" → 配置并启动端口 → 等链路 up → 预先造好帧模板 |
| 4 | `EnvInfo::collect`（`envinfo.rs:78`） | 标定时钟，采集环境信息 |
| 5 | `SampleLog::with_capacity`（`samples.rs:43`） | 开了 `--samples` 才分配；否则容量为 0 |
| 6 | `spawn_reporter`（`live.rs:63`） | 在核 1 上起一个线程，每 5 秒打印一行进度 |
| 7 | 算出"起点"（`main.rs` 中 `let start = …`） | 起点 = 现在 + 1 ms，让下面的准备工作不被算进任何 session 的 sleep 误差 |
| 8 | `Runtime::new` + 64 次 `rt.spawn(session(…))` | 每个 session 是一个 task；初始相位均匀错开在一个 delay 周期内 |
| 9 | `rt.run(&dp.port, &driver)` | 进入主循环，直到所有 session 结束 |

B 的 1 ~ 7 步完全相同（`crates/raw-ping/src/main.rs:224` 起）。第 8 步换成"建一个 `Vec<Session>` 状态表，把每个 session 的初始 deadline 放进堆里"；第 9 步换成自己写的 `loop`（270 行）。

### 2.2 A 的一个请求：从 timer 到期到下一次 sleep

主循环在 `crates/rt/src/runtime.rs:162`。它的每一轮做三件事：**收包 → timer → 维护**。下面按一个请求的顺序走。

**第 ① 步：timer 到期，session 被唤醒（段③ 从这里开始）**

1. 主循环读一次时钟（`runtime.rs:188`，变量 `now`），调用 `Timers::fire(now)`（`timer.rs:73`）。
2. `fire` 从堆里弹出所有 deadline ≤ now 的 timer，把槽位标成 `Fired(now)`，调用登记在里面的 Waker。
3. `Waker::wake` → `waker_wake`（`executor.rs:183`）：通过线程局部变量找到当前 runtime，核对编号和代数，把任务号推进就绪队列（`ReadyQueue::push`，31 行）。
4. 回到主循环，`run_ready`（`executor.rs:106`）出队，调用这个 task 的 `poll`。
5. task 是 `session()` 这个 async fn（`async-ping/src/main.rs:86`）。它上次停在 `sleep_until(…).await`，这次 `Sleep::poll`（`timer.rs:107`）发现槽位是 `Fired`，返回 `SleepInfo { deadline, fired_at }`。
6. session 接着往下走：`record(…)`（67 行）把**上一个**请求的样本记进统计，并在这里释放上一个回复的 mbuf。

**第 ② 步：发送（段①：T0 → T1）**

7. `send(&sh, id, seq).await`（`main.rs:31`）：第一行读时钟，这就是 **T0**（36 行）。
8. `with_port(|p| sh.sender.send(p, t0, id, seq))`：向 runtime 取端口（`runtime.rs:86`），调用共用的发送函数。
9. `Sender::send`（`pingkit/src/sender.rs:65`）：从池里取一个 mbuf → 写入帧模板、id、seq、T0、校验和（`pingproto` 的 `write_request`）→ `port.tx(m)`（`dpdk/src/port.rs:131`）→ 读时钟，这就是 **T1**（`sender.rs:75`）。
10. 回到 `send`：登记"我在等 seq，超时时刻是 T0 + timeout"（`Flow::arm`，`driver.rs:49`），已发送数加一。
11. 回到 `session`：`on_wake(…)` 记下刚才那次 sleep 的误差和段③。

**第 ③ 步：等回复**

12. `wait_reply(&sh, id, seq).await`（`main.rs:60`）→ `flow.mailbox.recv().await`。信箱是空的，`Recv::poll`（`sync.rs:68`）把自己的 Waker 存进信箱，返回 `Pending`。
13. `session` 的 poll 返回，`run_ready` 继续处理就绪队列里的其他 task；队列空了就回到主循环。这个 session 现在"睡着了"，不占用任何 CPU。

**第 ④ 步：回复到达（段②：T2 → T3）**

14. 主循环调用 `port.rx_burst`（`runtime.rs:175` 附近），返回 n > 0。读时钟，这就是 **T2**（177 行），同一批的包共用它。
15. 对每个包调用 `driver.on_packet(m, t2, port)`（`async-ping/src/driver.rs:144`）：
    - `classify`（`pingproto/src/lib.rs:172`）认出它是对端发来的 echo reply，取出 id、seq、带回的时间戳；
    - 用 id 找到 `Flow`，检查 seq 正是它在等的那个；
    - `f.mailbox.put(Ok(Reply { mbuf, t2, tx_tsc }))`（`rt/src/sync.rs:34`）：把回复（包括 mbuf 的所有权）放进信箱，并调用存在里面的 Waker。
16. `waker_wake` 把这个 session 的任务号推进就绪队列（同第 3 条）。
17. **每处理一个包，主循环立刻 `run_ready`**（`runtime.rs` 的 `for m in burst.by_ref()` 循环里）：出队，poll 这个 session。
18. `Recv::poll` 这次从信箱里取到了回复，返回 `Ready`。`session` 从 `wait_reply().await` 之后继续执行，第一行读时钟，这就是 **T3**（`main.rs:94`）。

**第 ⑤ 步：收尾，再次 sleep**

19. 已收到数加一；`sleep_until(t3 + delay).await`（`main.rs:107`）：`Sleep::poll` 把 timer 登记进堆，返回 `Pending`。回复（和它的 mbuf）此时还被这个 task 持有着。
20. 回到第 ① 步，循环。

### 2.3 B 的同一个请求

B 没有 task、没有 Waker、没有就绪队列。每个 session 是状态表里的一行（`raw-ping/src/main.rs:46` `Session`），状态是一个枚举（26 行）：`Sleeping` / `Waiting` / `Done`。主循环（270 行）每一轮同样做三件事：

| A 的步骤 | B 对应的代码 | 区别 |
|---|---|---|
| ① timer 到期 → 唤醒 → poll → record | `on_timers`（157 行）：从堆里弹出到期的 session → 直接 `record` | 没有 wake、入队、出队、poll |
| ② T0 → 发送 → T1 | `on_timers` 里：`let t0 = rdtsc()`（181 行）→ `self.sender.send(…)` | **调用的是同一个 `Sender::send`** |
| ③ 等回复 | 把状态改成 `Waiting { seq, stamp, timeout_at }` | 不需要"挂起"，只是改一个字段 |
| ④ T2 → 认包 → T3 | `on_rx`（90 行）：`rx_burst` → T2（95 行）→ `classify` → `on_reply`（123 行）→ **T3**（131 行） | 认出包之后直接改状态，没有信箱、Waker、就绪队列、poll |
| ⑤ 收尾 | `on_reply` 里：把回复存进 `held`，状态改成 `Sleeping`，deadline 入堆 | |

**把两边放在一起看**，A 多出来的就是：信箱（`sync.rs`）、Waker 和就绪队列（`executor.rs`）、timer 的槽位表（`timer.rs`）、以及 async fn 被编译器变成的状态机。这些加起来就是"抽象税"，README §1.1 有逐项的量化。

### 2.4 每 100 µs 一次的维护节拍

两边都有，做的事相同：

- A：`runtime.rs` 主循环的第 3 部分调用 `driver.on_tick`（`async-ping/src/driver.rs:192`）；
- B：主循环里的 `if house.due(now) { … }`（`raw-ping/src/main.rs:278` 附近）。

内容：

1. `maintain`（`pingkit/src/house.rs:39`）：驱动网卡的 watchdog（`rte_timer_manage`）、回收已发送完成的 mbuf、把计数器发布给上报线程；
2. **超时扫描**：遍历 64 个 session，谁超时了就计为丢失。A 往它的信箱里放一个 `Err(Timeout)`，让 `wait_reply` 返回；B 直接改状态；
3. **该不该停**：网卡要求 reset？收到信号？到时间了？到了就置 `stopping`，session 发完手头这个就不再发；
4. 所有 session 都结束了 → 退出主循环。

### 2.5 收尾（核对泄漏的关键在顺序）

A 在 `main.rs` 的 `rt.run` 返回之后（约 160 行起），B 在 `finish`（304 行）：

1. 停掉上报线程；数一下结束时还在途的请求。
2. **先释放程序自己手里的所有 mbuf**：A 是 `drop(rt)`（丢弃所有未完成的 task，它们持有的回复随之释放）+ 清空每个信箱；B 是 `drop(raw.sessions)` + `drop(raw.burst)`。
3. `Dataplane::shutdown`（`dataplane.rs:96`）：回收 TX 环 → 停端口（驱动把收发环上的 mbuf 还回池里）→ **读池的可用数，与初始值比较** → 关端口。
4. `Report::new(…).emit(…)`：打印报表，写 JSON 和样本文件。
5. `eal.cleanup()`，然后以退出码 0（无泄漏）或 3（有泄漏）结束。

---

## 第 3 章 逐个 crate 解读

每一节的格式：它解决什么问题 → 关键的类型和函数 → 值得注意的设计。

### 3.1 `dpdk-sys`：DPDK 的裸接口

**解决什么问题**：Rust 不能直接调用 C 库，需要一份"函数声明"。这个 crate 在编译时用 bindgen 自动生成。

| 文件 | 作用 |
|---|---|
| `build.rs` | 编译时执行：用 pkg-config 找到 DPDK → 编译 `shim.c` → 用 bindgen 生成声明。`FUNCTIONS` 列表（3 行起）是本项目用到的全部 DPDK 函数，只生成这些 |
| `src/shim.c`（17 行） | **仓库里唯一的 C 代码**。DPDK 的 `rx_burst` / `tx_burst` / `alloc` / `free` 在头文件里是 `static inline`，库里没有符号，只能各包一层普通函数 |
| `src/lib.rs` | 把生成的声明包含进来；导出 DPDK 的版本号 |

这里的函数全是 `unsafe` 的。别的 crate 不直接用它，都通过 `dpdk`。

### 3.2 `dpdk`：安全封装

**解决什么问题**：把"容易用错的 C 接口"变成"用错了就编译不过的 Rust 类型"。项目里 57 处 unsafe 有 45 处在这里，上层拿到的是安全的类型。

| 类型 / 函数 | 位置 | 作用 | 设计要点 |
|---|---|---|---|
| `Eal` | `eal.rs:8` | "DPDK 已初始化"的凭证 | 全进程只能有一个（`init` 里用原子标志防重复）；`cleanup` 是 `unsafe` 的，因为调用后所有 DPDK 对象都失效 |
| `Mempool` | `mempool.rs:8` | mbuf 池 | 创建后故意"泄漏"成 `&'static`：从类型上保证池不会比 mbuf 先消失 |
| `Mempool::alloc` | `mempool.rs:40` | 取一个空 mbuf | 池空返回 `None`，不 panic |
| `Mempool::avail_count` | `mempool.rs:28` | 池里还有多少个可用 | 零泄漏核对用的就是它 |
| **`Mbuf`** | `mbuf.rs:9` | 一个包缓冲区的**唯一所有者** | 不能复制；`Drop`（104 行）时归还池子；内含裸指针所以不能跨线程 |
| `Mbuf::data` / `data_mut` / `set_len` | `mbuf.rs:64 / 75 / 87` | 读、写包内容，设长度 | 返回的切片借用 `Mbuf`，不会比它活得久 |
| `Mbuf::into_raw` | `mbuf.rs:23` | 放弃所有权（交给网卡时用） | 只有 `Port::tx` 调用它 |
| **`Port`** | `port.rs:17` | 一个网卡端口（固定用 0 号收发队列） | 故意做成不能跨线程（`PhantomData<*const ()>`） |
| `Port::configure` / `start` / `stop` / `close` | `port.rs:36 / 83 / 90 / 96` | 生命周期 | `close` 消耗 `self`，之后无法再用 |
| `Port::rx_burst` | `port.rs:119` | 收一批包 | 结果放进 `RxBurst`，逐个以 `Mbuf` 的形式交出 |
| `Port::tx` | `port.rs:131` | 发一个包 | 成功：所有权交给网卡；失败：**原样还给调用者** |
| `Port::tx_done_cleanup` | `port.rs:145` | 主动回收已发完的 mbuf | 在维护节拍里调用，不让回收落进发送路径 |
| `Port::stats` / `xstats` | `port.rs:151 / 166` | 网卡计数器 | `xstats` 里有 AWS 的限额计数 |
| `Port::reset_requested` | `port.rs:188` | 网卡是否要求 reset | 回调函数（194 行）只写一个原子变量 |
| `RxBurst` | `port.rs:201` | 一次 `rx_burst` 的结果 | 是个迭代器；没取走的包在下次收包或 `Drop` 时自动释放 |
| `tsc::rdtsc` | `tsc.rs:10` | 读时钟 | 名字叫 rdtsc，实际执行 `rdtscp`（原因见函数上方的注释） |
| `tsc::cycles_to_ns` / `ns_to_cycles` | `tsc.rs:24 / 29` | 周期与纳秒互换 | |
| `tsc::clock_read_cost` | `tsc.rs:62` | 标定读时钟的成本和步长 | 启动时调用一次 |
| `tsc::StallWatch` | `tsc.rs:92` | 停顿检测 | 两类：空轮询停顿（`tick`）、取包前停顿（`tick_rx`） |
| `tsc::sfence` / `mfence` | `tsc.rs:35 / 42` | 诊断开关用的两条指令 | |
| `Error` | `error.rs` | 错误类型：哪一步失败 + errno | |

### 3.3 `pingproto`：协议

**解决什么问题**：我们绕过了内核，就得自己造包、认包。这个 crate 只做计算，不依赖 DPDK，所以可以脱离网卡做单元测试。

| 函数 / 类型 | 位置 | 作用 |
|---|---|---|
| 帧布局（各字段的偏移） | `lib.rs:12 ~ 20` | 以太网 14 字节 + IPv4 20 字节 + ICMP 8 字节 + 载荷（前 8 字节放 T0） |
| `sum16` / `fold` / `checksum` | `lib.rs:43 / 54 / 61` | Internet 校验和 |
| `EchoTemplate::new` | `lib.rs:88` | 启动时造好一个"id、seq、时间戳都是 0"的完整帧，并预先算好不变部分的校验和 |
| **`EchoTemplate::write_request`** | `lib.rs:132` | 热路径：拷贝模板 → 填 id、seq、T0 → 校验和只做 6 次加法 |
| `Rx` | `lib.rs:155` | 一个帧的四种可能：对端的回复 / 别人发来的回复 / 问我的 ARP / 其他 |
| **`classify`** | `lib.rs:172` | 认包。**源 IP 必须是对端**才算我们的回复（197 行） |
| `arp_reply_in_place` | `lib.rs:224` | 把收到的 ARP 请求原地改写成应答 |
| `parse_mac` / `parse_ipv4` | `lib.rs:241 / 251` | 解析命令行里的地址 |

### 3.4 `timerq`：最小堆

64 行，一个类型 `TimerHeap`（`lib.rs:10`）：`push(deadline, payload)`、`pop_expired(now)`。
payload 是一个 32 位整数：在 A 里是 timer 槽位号，在 B 里是 session 号。A 的 timer 和 B 的 delay 用的是同一个堆的实现。

### 3.5 `rt`：runtime（最重要的一节）

**解决什么问题**：让应用可以把每个 session 写成一个顺序的 `async fn`（发 → 等 → 睡 → 记），而由 runtime 负责在 64 个 session 之间切换、盯着网卡、管理定时。

五个文件，建议按这个顺序读：

**① `sync.rs`（88 行）——信箱，最简单，先读它**

`Mailbox<T>`（17 行）里只有两样东西：一个可能有值的槽 `item`，一个可能有的 Waker。

- `put(v)`（34 行）：槽里已经有东西就原样退回；否则放进去，取出 Waker，**先释放借用再调用 wake**（避免 wake 引起的重入撞上借用检查）。
- `recv()` 返回一个 future `Recv`；它的 `poll`（68 行）：槽里有值就取走返回 `Ready`；没有就把自己的 Waker 存进去，返回 `Pending`。
- `Recv` 被丢弃时（83 行）会撤销登记，避免之后的 `put` 去叫醒一个已经不在等的 task。

**② `executor.rs`（208 行）——任务、就绪队列、Waker**

- `ReadyQueue`（13 行）：一个固定大小的环形数组 + 每个任务一个"已入队"标记。`push`（31 行）时标记已置就直接返回（同一个任务被唤醒多次只 poll 一次）；`pop`（47 行）时**先清标记再返回**，这样 poll 期间发生的 wake 还能让它重新入队。
- `TaskSlot`（61 行）：一个装 future 的格子 + 代数。格子的数组容量固定、永不扩容。
- `Executor::run_ready`（106 行）：`while let Some(idx) = ready.pop()` → 造 Waker → poll。future 返回 `Ready` 就清空格子、代数加一、归还格子。
- **Waker**（159 行起的注释 + 170 ~ 208 行）：
  - `make_waker`（173 行）：把 runtime 编号（16 位）、代数（16 位）、任务号（32 位）拼成一个整数，当作 Waker 的"数据指针"。它从不被解引用。
  - `waker_clone` / `waker_drop`（179 / 202 行）：什么都不做。没有引用计数。
  - `waker_wake`（183 行）：拆出三个数 → `try_with_core` 找当前线程上正在运行的 runtime → 编号对得上就 `exec.wake(gen, idx)`（145 行：代数对得上才入队）。
  - 对不上的三种情况（194 ~ 199 行）：当前线程上跑的是另一个 runtime → abort；当前线程上没有 runtime，但这个 runtime 刚在本线程结束（关停阶段的迟到 wake）→ 忽略；否则是别的线程 → abort。

**③ `timer.rs`（152 行）——sleep**

- `Timers`（35 行）：一个 `TimerHeap` + 一张槽位表。槽位有四种状态（27 行）：空闲 / 等待中（存着 Waker）/ 已触发（存着触发时刻）/ 已取消。
- `Timers::fire(now)`（73 行）：弹出所有到期的，标成"已触发"，wake。
- `sleep(cycles)` / `sleep_until(deadline)`（90 / 95 行）：创建一个 `Sleep`。
- `Sleep::poll`（107 行）：第一次 poll **只登记**（分配槽位、入堆），不读时钟；之后的 poll 看槽位——已触发就归还槽位、返回 `SleepInfo`。
  （v1 在登记前会多读一次时钟，v2 去掉了，见文件开头的说明和报告 §5。）
- `Drop for Sleep`（139 行）：sleep 在触发前被丢弃 → 把槽位标成"已取消"，等它从堆里弹出时再回收。取消是 O(1) 的，不用在堆里查找。

**④ `runtime.rs`（248 行）——把上面三样和网卡粘在一起**

- `trait Driver`（14 行）：应用要实现的三个函数——`on_burst`（统计用）、`on_packet`（每个包）、`on_tick`（每个维护节拍，返回 false 表示要求退出）。它是泛型参数，调用是静态分发的，可以内联。
- 线程局部变量（57 ~ 62 行）：`CURRENT` 指向"当前线程上正在运行的 runtime"，只在 `run` / `drop` 期间非空；`LAST_RT` 记着本线程最近跑过的 runtime 编号。
  `Enter`（96 行）是设置和恢复 `CURRENT` 的守卫。Waker 的安全性就靠这两个变量。
- `with_port`（86 行）：task 里用它拿到网卡。
- **`Runtime::run`**（162 行）：主循环。注释里写了每一轮的三步，对照 2.2 节读。
- `run_offline`（220 行）：不驱动网卡，只跑 executor 和 timer。单元测试用；发现"所有 task 都在等、却没有任何 timer"时 panic（死锁检测）。
- `Drop for Runtime`（242 行）：丢弃所有未完成的 task，释放它们持有的资源（mbuf、timer）。

**⑤ `probe.rs`（19 行）**：只在 `--features probe` 时编译，记录"executor 开始 poll 的时刻"，用来把段②拆成子段。

### 3.6 `pingkit`：A 和 B 共用的一切

**解决什么问题**：保证 A 和 B 除了调度之外执行同一份代码。

| 文件 | 行数 | 内容 |
|---|---|---|
| `args.rs` | 308 | 命令行参数（9 行起）；诊断开关（94 ~ 140 行）；`sample_capacity`（171 行）；合法性检查 `resolve`（190 行）；读 `nic.env`（231 行） |
| `dataplane.rs` | 113 | `Dataplane::open` / `shutdown`；单实例锁；`LeakReport` |
| `sender.rs` | 96 | **发送函数**。`Stamp`（7 行）是一次发送留下的记录：T0、T1、距上次发送多久 |
| `hist.rs` | 241 | 直方图。小于 256 的值精确记录，更大的值每个 2 的幂分 128 个桶。`record`（53 行）、`quantile`（82 行）、`quantile_interp`（107 行）、`fraction_at_or_above`（137 行） |
| `stats.rs` | 614 | 计数器、各段的直方图、报表。见下 |
| `house.rs` | 48 | 维护节拍：`House::due`（21 行）判断该不该做了；`maintain`（39 行）做什么；`START_LEAD_NS`（34 行）是起点延后的 1 ms |
| `live.rs` | 90 | 上报线程（只读几个原子计数器）；信号处理；绑核 |
| `samples.rs` | 134 | 原始样本：一个样本压成 8 字节，写进预先分配好的数组 |
| `envinfo.rs` | 162 | 采集环境信息；启动时标定时钟，结束时算时钟漂移 |
| `build.rs` | 28 | 编译时把 git 提交号、编译器版本写进程序 |

`stats.rs` 再细一点：

| 符号 | 位置 | 作用 |
|---|---|---|
| `Counters` | 14 行 | 所有计数：sent、received、timeouts、late、unexpected、foreign、tsc_mismatch、other_rx、arp_replies、tx_full、no_mbuf…… |
| `Stats` | 44 行 | 各段的直方图 + 计数器 + 异常明细 + 样本记录 |
| `on_reply` | 130 行 | 一个样本进来：记段①、段②、进程内、端到端；按"距上次发送多久"分档；写原始样本 |
| `on_wake` | 161 行 | 一次 sleep 结束：记 sleep 误差、段③、"deadline → 下一个 T0" |
| `verify_echo` | 172 行 | 回复带回的时间戳必须等于我们写进去的 T0；不等就计数并返回 false |
| `note_anomaly` | 202 行 | 记一条异常明细（最多 16 条） |
| `rows` | 216 行 | 把各直方图变成报表的行 |
| `Report::print` | 361 行 | 屏幕上看到的那份报告就是这里打印的，从上到下一一对应 |
| `port_summary` | 474 行 | 读网卡计数器，算 AWS 限额计数在本次运行里的增量 |
| `Report::new` / `emit` | 498 / 554 行 | 组装报告；打印 + 写 JSON + 写样本文件 |

### 3.7 `async-ping`：A

两个文件。

**`driver.rs`（218 行）——协议侧，session 与 reactor 共享的状态**

| 符号 | 位置 | 作用 |
|---|---|---|
| `Reply` | 14 行 | reactor 交给 session 的东西：mbuf 的所有权 + T2 + 带回的时间戳 |
| `Flow` | 28 行 | 一个 session 在协议侧的状态：在等哪个 seq、何时超时、最近 4 个超时的 seq、信箱 |
| `Flow::arm` | 49 行 | 发送成功后登记"我在等 seq" |
| `Shared` | 68 行 | 所有 session 和 driver 共享的东西：64 个 `Flow`、统计、发送器、各种时长、停止标志 |
| `IcmpDriver::on_packet` | 144 行 | 认包 → 找 Flow → 放进信箱；或者计为迟到 / 对不上号 / 外来 / ARP / 其他 |
| `IcmpDriver::on_tick` | 192 行 | 维护 → 超时扫描 → 停止判定 |

**`main.rs`（202 行）——session 和 main**

| 符号 | 位置 | 作用 |
|---|---|---|
| `send` | 31 行 | T0 → 发送 → 登记。TX 环满或取不到 mbuf 时睡 1 µs 重试 |
| `wait_reply` | 60 行 | 等信箱 |
| `record` | 67 行 | 核对时间戳 → 记样本；回复在函数结束时被丢弃 → mbuf 归还 |
| **`session`** | 86 行 | **整个 A 的业务逻辑就是这个循环，与题目给的形状逐行对应** |
| `main` | 113 行 | 见 2.1 和 2.5 |

### 3.8 `raw-ping`：B

一个文件 `main.rs`（332 行）。

| 符号 | 位置 | 作用 |
|---|---|---|
| `State` | 26 行 | session 的三种状态 |
| `Held` | 36 行 | sleep 期间持有的回复（mbuf + 发送记录 + T2、T3） |
| `Session` | 46 行 | 状态表的一行 |
| `Raw` | 71 行 | 整个程序的状态：数据面、发送器、状态表、堆、统计 |
| `Raw::on_rx` | 90 行 | 收包、认包、分发 |
| `Raw::on_reply` | 123 行 | 回复到了：打 T3、存回复、改状态、deadline 入堆 |
| `Raw::on_timers` | 157 行 | 到期的 session：记上一个样本 → 打 T0 → 发送 |
| `Raw::scan_timeouts` | 205 行 | 超时扫描 |
| `main` | 224 行 | 初始化 + 主循环（270 行） |
| `finish` | 304 行 | 收尾 |

---

## 第 4 章 四个时间戳、三段

| | A | B |
|---|---|---|
| **T0**（决定发送） | `async-ping/src/main.rs:36`，`send()` 里读时钟 | `raw-ping/src/main.rs:181`，`on_timers` 里调用发送函数之前 |
| **T1**（`tx_burst` 返回） | `pingkit/src/sender.rs:75` | 同一行（同一个函数） |
| **T2**（`rx_burst` 返回） | `rt/src/runtime.rs:177` | `raw-ping/src/main.rs:95` |
| **T3**（回复交到 session 手里） | `async-ping/src/main.rs:94`，`wait_reply().await` 之后 | `raw-ping/src/main.rs:131`，`on_reply` 里状态匹配成功之后 |
| "timer 发现到期"的时刻 | `rt/src/runtime.rs:188` 的 `now` | `raw-ping/src/main.rs:275` 的 `now` |

| 量 | 定义 | 在哪里算 |
|---|---|---|
| 段① | T1 − T0 | `stats.rs:130` `on_reply` |
| 段② | T3 − T2 | 同上 |
| 进程内耗时（排名指标） | 段① + 段② | 同上 |
| 端到端 | T3 − T0 | 同上 |
| sleep 误差 | 发现到期 − deadline | `stats.rs:161` `on_wake` |
| 段③ | 下一个 T0 − 发现到期 | 同上 |

所有读时钟都是 `dpdk::tsc::rdtsc()` 这一个函数。两边每个请求读时钟的次数相同：T0、T1、T2（一批共用）、T3，加上主循环每轮一次。

诊断开关 `--diag-pre-t0` 的执行点在读 T0 的前一行：A 是 `main.rs:33 ~ 35`，B 是 `main.rs:178 ~ 180`。

---

## 第 5 章 横切主题：某件事是怎么保证的

### 5.1 零 mbuf 泄漏

| 环节 | 代码 |
|---|---|
| mbuf 只有一个所有者，丢弃即归还 | `dpdk/src/mbuf.rs:104` `Drop for Mbuf` |
| 发送失败时 mbuf 原样退回 | `dpdk/src/port.rs:131` `tx` 的返回类型 `Result<(), Mbuf>` |
| 一批包里没取走的自动释放 | `port.rs:239` `Drop for RxBurst` |
| 记下初始可用数 | `pingkit/src/dataplane.rs` `open` 里的 `avail_initial` |
| 收尾时先丢弃程序手里的 mbuf | A：`drop(rt)` + 清信箱；B：`drop(raw.sessions)` |
| 停端口后比较可用数 | `dataplane.rs:96` `shutdown` |
| 有泄漏则退出码为 3 | 两个 `main` 的最后一行 |

### 5.2 超时、迟到、对不上号、外来回复

| 情况 | 怎么判定 | A | B |
|---|---|---|---|
| 超时 | 维护节拍里 `now ≥ timeout_at` | `driver.rs:192` `on_tick`，往信箱放 `Err(Timeout)` | `main.rs:205` `scan_timeouts` |
| 迟到（超时之后回复才到） | seq 在"最近 4 个超时的 seq"里 | `driver.rs:62` `timed_out_before` | `main.rs:139` 附近 |
| 对不上号 | 是对端的回复，但 seq 既不是在等的、也不是最近超时的 | `on_packet` 的最后一个分支 | `on_reply` 的最后一个分支 |
| 外来回复 | 源 IP 不是对端 | `pingproto/src/lib.rs:197` 返回 `ForeignEchoReply` | 同左 |
| 时间戳不符 | 回复带回的时间戳 ≠ 我们写入的 T0 | `stats.rs:172` `verify_echo`，这样的样本不进延迟分布 | 同左 |

### 5.3 两条对账

打印在 `stats.rs:361` `Report::print` 的开头：

- 请求对账：`sent − received − timeouts − in-flight = 0`；
- 收包对账：`rx − (received + late + unexpected + foreign + other + arp) = 0`。

为了让第二条成立，认包的每个分支都必须给某个计数器加一（包括"ARP 应答没发出去"这种角落，见 `driver.rs` 和 `raw-ping/src/main.rs` 里 `ArpRequest` 分支的 `else`）。

### 5.4 干净退出

| 触发 | 代码 |
|---|---|
| 到时间 | 维护节拍里 `now >= end` → `stopping = true` |
| SIGINT / SIGTERM | `live.rs:34` 的处理函数只置 `STOP`；维护节拍里 `stop_requested()` |
| 网卡要求 reset | `port.rs:188` `reset_requested()`；A 的 `on_tick` 返回 false，B 直接 `break` |

`stopping` 置位后，session 不再发新请求，等在途的收到或超时，然后结束；全部结束后主循环退出。

### 5.5 Waker 的内存安全

问题：标准库规定 Waker 可以被送到别的线程，而我们的就绪队列不是线程安全的。
做法（`executor.rs:159 ~ 208`，`runtime.rs:57 ~ 112`）：wake 时检查"当前线程上正在运行的是不是这个 runtime"，不是就 abort。
过期的 Waker（任务已结束、槽被复用）靠代数识别。测试见第 6 章。

### 5.6 停顿检测（最大值尖刺从哪来）

`dpdk/src/tsc.rs:92` `StallWatch`。A 在 `Runtime::run` 里、B 在自己的主循环里各调用两处：`tick`（每轮读完时钟后）和 `tick_rx`（收到包的那一轮）。
"这一轮什么都没干却过了 1 µs 以上" = 我们的代码没在运行 = 被外部打断。

### 5.7 单实例锁

`pingkit/src/dataplane.rs:33` `instance_lock`：对 `/run/bqping-<PCI>.lock` 加一把不阻塞的文件锁，拿不到就报错退出。锁随进程结束自动释放。

### 5.8 unsafe 都在哪

| crate | 处数 | 是什么 |
|---|---|---|
| `dpdk` | 45 | 调用 DPDK 的 C 函数；读 mbuf 的字段；`rdtscp` 等指令 |
| `rt` | 6 | Waker 的 4 个 vtable 函数（`executor.rs:176 ~ 202`）；2 处解引用线程局部的 runtime 指针（`runtime.rs:72`、`91`） |
| `pingkit` | 4 | 注册信号处理函数、绑核（`live.rs`）；读内核单调时钟（`envinfo.rs`）；诊断开关里的 volatile 写（`args.rs`） |
| A、B | 各 1 | `main` 最后的 `eal.cleanup()` |
| `pingproto`、`timerq` | 0 | |

每一处上方都有 `// SAFETY:` 说明为什么是安全的。`scripts/check-compliance.sh` 会清点并核对。

### 5.9 诊断与离线分析的支持

| 功能 | 命令行 | 代码 |
|---|---|---|
| 原始样本 | `--samples <文件>` | `samples.rs`；写入点在 `stats.rs` 的 `on_reply` 末尾 |
| T0 前多做一件事 | `--diag-pre-t0 sfence\|mfence\|stores` | `args.rs:102 ~ 140`；打开后报告标明"不参与排名" |
| 段①、段②拆成子步骤 | 编译时 `--features probe` | `sender.rs` 里带 `#[cfg(feature = "probe")]` 的行；`rt/src/probe.rs` |
| 环境与构建信息 | 自动 | `envinfo.rs`、`pingkit/build.rs` |

---

## 第 6 章 测试

`cargo test --release --workspace`：35 个，不需要网卡和 root。

| 位置 | 个数 | 测什么 |
|---|---|---|
| `crates/rt/tests/offline.rs` | 11 | runtime：sleep 按 deadline 顺序醒来且不早到；信箱顺序交接；poll 期间自己唤醒自己 1000 次不丢；过期的 Waker 不会唤醒复用同一槽位的新任务；丢弃 runtime 时释放未完成 task 持有的资源；取消的 sleep 不误触发；deadline 已过的 sleep 在下一次 timer 阶段触发；死锁检测；在别的线程 / 另一个 runtime 里 wake 会 abort（在子进程里验证）；runtime 结束后的迟到 wake 被忽略 |
| `crates/pingproto/src/tests.rs` | 6 | 增量校验和与全量重算在 100 万组随机数据上逐位一致；帧布局；认包；别的主机发来的回复不会被当成我们的；ARP 原地应答；地址解析 |
| `crates/pingkit/src/hist.rs` 末尾 | 4 | 分桶边界；分位数精度；插值分位数（模拟 10 ns 步长的时钟）；尾部占比 |
| `crates/pingkit/src/samples.rs` 末尾 | 4 | 打包与还原；没开时不记录；写满即停；文件布局 |
| `crates/pingkit/src/stats.rs` 末尾 | 4 | 一个样本进来各段都对；时间戳不符的被计数并排除；异常明细有上限；sleep 误差与段③的拆分 |
| `crates/pingkit/src/args.rs` 末尾 | 3 | 默认值合法；边界值；样本缓冲区的容量估算 |
| `crates/dpdk/src/tsc.rs` 末尾 | 2 | 停顿检测区分两类；时钟标定结果合理 |
| `crates/timerq/src/lib.rs` 末尾 | 1 | 按 deadline 顺序弹出 |

需要网卡的测试：`scripts/fault.py`（故障注入，20 个场景 × A / B）。

---

## 第 7 章 脚本

都在 `scripts/` 下。

**搭环境、日常运行**

| 脚本 | 作用 |
|---|---|
| `common.sh` | 公共配置（版本号、核的规划、路径）和小工具，被其他脚本引入 |
| `setup.sh` | 一键搭环境，分阶段、可重复执行：装包 → Rust → 探测网卡 → 编译 DPDK → igb_uio → 大页 → 启动参数 → 中断 |
| `detect-nic.sh` | 自动探测网卡参数，生成 `config/nic.env` |
| `bind.sh` / `unbind.sh` | 把网卡交给 DPDK / 还给内核。`bind.sh` 拒绝绑定 SSH 所在的网卡 |
| `check-env.sh` | 只读的环境自检（重启后先跑它） |
| `run.sh A\|B\|C …` | 一键运行：自动绑定网卡、编译、以 root 运行 |
| `run-c.sh` | C：64 个系统 `ping` 进程 |

**测量**

| 脚本 | 作用 |
|---|---|
| `ab.sh N 秒数` | A、B 交替 N 对 |
| `campaign.sh [阶段…]` | 一键重测报告里的全部数据（主考核、交替、诊断口径、辅助对比、故障注入、30 分钟、probe） |
| `session.sh 名字` | 一次独立的复测会话（重启后 / 另一天用），结果不覆盖正式数据 |
| `drift.sh 目录 分钟数` | A、B 每 20 秒交替一次的长时间监测 |
| `versions.sh` | 两个代码版本的 A 与 B 轮流对比 |
| `build-version.sh 标签` | 把某个历史版本编译到仓库之外，供对比用 |
| `fault.py` | 故障注入矩阵 |
| `write_meta.py` | 记录一组测量属于哪次开机、哪个代码版本 |

**分析与报告**

| 脚本 | 作用 |
|---|---|
| `ci.py` | 读原始样本：置信区间、每秒序列、按批内位置拆分 |
| `make_report.py` | 用 `logs/` 下的 JSON 重新生成 `docs/REPORT.md` 里所有的表 |
| `report.py` | `make_report.py` 调用它生成"主考核"那一节 |
| `plots.py` | 生成 `docs/img/` 下的图 |
| `summarize.py` / `summarize_c.py` | 汇总多轮 A / B；汇总 C 的逐包输出 |
| `check-compliance.sh` | 把题目的硬性要求逐条机器核对 |

**数据是怎么流到报告里的**

```text
async-ping / raw-ping ──(--json)──▶ logs/…/*.json ─┐
        │                                          ├─▶ make_report.py ─▶ docs/REPORT.md 里的表
        └──(--samples)──▶ logs/tmp/*.samples       │
                               │                   ├─▶ plots.py ─▶ docs/img/*.png
                               └─▶ ci.py ─▶ ci.json┘
```

`docs/REPORT.md` 里 `<!-- BEGIN:x -->` 和 `<!-- END:x -->` 之间的内容是生成的，不要手改；它们之外的文字是手写的解读。

---

## 第 8 章 日志与数据的目录结构

```text
logs/
  final/        当前版本的主考核（A-600、B-600）、置信区间 ci.json、辅助对比
  ab-<时间>/    A / B 交替的各轮
  diag/         诊断口径：T0 前 sfence / mfence / N 次写入
  fault/<时间>/ 故障注入：每个场景的日志、JSON、汇总
  soak/         30 分钟连续运行
  probe/        probe 构建的诊断
  sessions/     重启后 / 另一天的复测会话
  versions/     不同代码版本的轮流对比
  exp/          没有并入正式代码的实验（补丁 + 结果）
  v1/           上一个代码版本（标签 v1）的全部数据，结构同上
  C-*/          系统 ping 的汇总
  history/      更早的运行
  setup/        搭环境的日志
  tmp/          临时文件、原始样本（不进仓库）
docs/
  REPORT.md     延迟报告            DEFENSE.md   答辩提纲
  WORKLOG.md    工作记录的副本       CODE_GUIDE.md 本文的副本
  img/          图                  v1/          上一个版本的报告和图（冻结）
```

每个 JSON 都带 `env` 字段，其中 `git_commit` 是构建那个二进制时的提交号；一组数据旁边的 `meta.json` 记着它属于哪次开机、哪个代码版本。

---

## 第 9 章 想改某样东西，该动哪里

| 想做的事 | 改哪里 | 注意 |
|---|---|---|
| 换一块网卡 | `config/nic.env`（或重跑 `scripts/detect-nic.sh`） | 代码不用动。非 ENA 的网卡要在 `setup.sh` 里加对应的驱动 |
| 加一个命令行参数 | `pingkit/src/args.rs` 的 `Args` | A 和 B 自动都有；记得在 `resolve` 里加合法性检查 |
| 加一个计数器 | `stats.rs` 的 `Counters` + `Report::print` 里打印 | 如果它是"收到的包"的一类，要加进收包对账的公式 |
| 加一个统计的时间段 | `stats.rs` 的 `Stats` 加一个 `Hist`，在 `rows` 里加一行 | 记录点要放在被测段之外 |
| 改超时策略 | A：`driver.rs` 的 `on_tick`；B：`scan_timeouts` | 两边要保持一致 |
| 改发送的内容 | `pingproto` 的 `EchoTemplate`；`sender.rs` | 校验和的单元测试会告诉你有没有改坏 |
| 改 runtime 的调度方式 | `rt/src/runtime.rs` 的 `run`、`executor.rs` 的 `run_ready` | `rt/tests/offline.rs` 不需要网卡，改完先跑它 |
| 加一个诊断开关 | `args.rs` 的 `PreT0` / `Diag` | 冷路径，不影响正常运行；报告会自动标明"不参与排名" |
| 改报告里的一张表 | `scripts/make_report.py` 对应的 `section_*` 函数 | 然后重跑 `make_report.py` |
| 加一个故障场景 | `scripts/fault.py` 的 `cases()` | |

**改了 `crates/` 下的任何东西之后**：`cargo clippy --release --all-targets`（应当 0 警告）→ `cargo test --release` → `scripts/fault.py` → 提交之后再测量（报告会记录构建时的提交号，并检查源码树是否干净）。

---

## 第 10 章 读代码时常见的"为什么"

**为什么 `rdtsc()` 这个函数里执行的是 `rdtscp`？**
`rdtsc` 不等前面的指令执行完就读时钟，会把前面一次 cache miss 的时间"藏掉"，而且对 A 和 B 藏得不一样多。`rdtscp` 会等。名字保留是为了不改调用处。（`dpdk/src/tsc.rs:3 ~ 9` 的注释。）

**为什么 `Mempool` 要"泄漏"成 `&'static`？**
这样任何地方都可以持有 `Mbuf` 而不用担心池先被销毁。池本来就要活到进程结束。

**为什么 `Port` 故意不能跨线程？**
同一个队列上的收发不是线程安全的。做成不能跨线程，编译器就替我们挡住了这种错误。

**为什么 `Port::tx` 失败时把 mbuf 还回来，而不是直接释放？**
让调用者决定怎么办（我们是释放并稍后重试）。更重要的是所有权清楚：要么网卡拿走了，要么还在你手里，不存在第三种状态。

**为什么就绪队列和信箱里大量用 `Cell` / `RefCell`？**
runtime 是单线程的，不需要锁；但多个地方（task、reactor）都要改同一份数据，Rust 默认不允许。`Cell` / `RefCell` 是"单线程下的共享可变"，前者没有运行时开销，后者有一次借用检查。

**为什么 `put` 里要"先释放借用再 wake"？**
wake 可能间接导致别的代码再来访问这个信箱。如果这时借用还没释放，`RefCell` 会 panic。

**为什么 `ReadyQueue::pop` 要在 poll 之前就清掉"已入队"标记？**
poll 的过程中这个 task 可能又被唤醒（比如它自己唤醒自己）。如果标记还在，这次唤醒会被当成"已经在队列里"而丢掉。

**为什么任务用 `Box<dyn Future>` 存？**
为了让 runtime 是通用的——它不需要知道 task 的具体类型。代价是每次 poll 多一次间接调用。

**为什么 A 的 `record` 放在 sleep 之后？**
题目给的循环形状就是"发 → 等 → 睡 → 记"，并要求睡的时候持有回复的 mbuf。B 照着同样的顺序做（`on_timers` 里先 `record` 再发下一个）。

**为什么 B 的 T0 不取在"发现 timer 到期"的那一刻？**
A 的顺序是"sleep 返回 → record → 进入 send() 才读 T0"，record 不在 A 的段①里。B 的 T0 若提前，它的段①就多包含了 record，两边不再对齐。

**为什么 `Sleep` 第一次 poll 不看时钟？**
"到没到期"只在主循环的 timer 阶段判断，那里每轮本来就读一次时钟。在 `Sleep` 里再读一次是多余的（约 18 ns），而且恰好发生在 session 拿到回复之后，会让同一批里后面的包多等。v2 去掉了它。

**为什么有 `shim.c`？不是说全部用 Rust 吗？**
DPDK 把最热的几个函数写成了头文件里的 `static inline`，库里没有它们的符号，任何语言要调用都得先包一层。这 17 行只是转发，没有逻辑。

**为什么报表里的数字都是 10 的整数倍？**
这台机器的时钟每 10 ns 才跳一步。程序启动时会标定并打印出来（"TSC 读数步长 26 周期 = 10.0 ns"）。JSON 里另有插值分位数（`p50_interp` 等）。
