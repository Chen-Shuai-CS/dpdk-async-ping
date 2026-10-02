#!/usr/bin/env python3
"""故障注入：主动制造各种异常，核对程序每次都能干净收场。

用法：scripts/fault.py [--only 名字片段] [--clients A,B]

"跑 10 分钟没出事"只能说明正常情况下没问题。这里反过来：故意制造超时、黑洞、信号、进程被冻结、
mbuf 耗尽、非法参数、重复启动……每个场景对 A、B 各跑一次，逐条核对：

  1. 进程没有崩溃（不是被信号杀死，退出码符合预期）
  2. mbuf 零泄漏（关停后 mempool 的可用数回到初值）
  3. 请求对账为 0：sent = received + timeouts + in-flight
  4. 收包对账为 0：收到的每个包恰好落入一类
  5. 该场景特有的预期（例如黑洞场景必须 100% 超时、0 收到）

结果写到 logs/fault/<时间>/（每个场景的日志、JSON，以及 summary.md / summary.json）。
"""
import argparse
import datetime
import json
import os
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = {"A": "async-ping", "B": "raw-ping"}


def nic_env():
    kv = {}
    with open(os.path.join(ROOT, "config/nic.env")) as f:
        for line in f:
            line = line.split("#")[0].strip()
            if "=" in line:
                k, v = line.split("=", 1)
                kv[k.strip()] = v.strip()
    return kv


class Case:
    def __init__(self, name, what, args, expect, rc=0, env=None, action=None, wants_report=True, expect_text=None, build=None):
        self.name, self.what, self.args, self.expect = name, what, args, expect
        self.rc, self.env, self.action, self.wants_report, self.expect_text = rc, env or {}, action, wants_report, expect_text
        # build="fault"：用带故障注入钩子的专用构建（target-fault/，cargo 特性 fault）；正式的可执行文件里没有这个钩子
        self.build = build


def bin_dir(case):
    return os.path.join(ROOT, "target-fault/release" if case.build == "fault" else "target/release")


def sig(name, after, pause=None):
    """运行 after 秒后给进程发信号；pause 不为空时，先发 STOP，pause 秒后再发 CONT。"""
    def act(binary, sudo_pid):
        time.sleep(after)
        t0 = time.time()
        if pause is None:
            subprocess.run(["sudo", "pkill", f"-{name}", "-x", binary], check=False)
        else:
            subprocess.run(["sudo", "pkill", "-STOP", "-x", binary], check=False)
            time.sleep(pause)
            subprocess.run(["sudo", "pkill", "-CONT", "-x", binary], check=False)
            # sudo 发现子进程被暂停后会把自己也暂停（为了配合 shell 的作业控制），所以它也要唤醒
            subprocess.run(["sudo", "kill", "-CONT", str(sudo_pid)], check=False)
        return t0
    return act


def second_instance(other_bin):
    """主进程运行 2 秒后，再启动另一个客户端去抢同一张网卡；它必须被拒绝。"""
    def act(binary, nic, outdir, case_name):
        time.sleep(2)
        cmd = ["sudo", os.path.join(ROOT, "target/release", other_bin), "--pci", nic["DPDK_PCI"], "--src-ip", nic["DPDK_IP"],
               "--dst-ip", nic["PEER_IP"], "--dst-mac", nic["PEER_MAC"], "--delay-us", "500", "--duration-sec", "2", "--progress-sec", "0"]
        p = subprocess.run(cmd, capture_output=True, text=True)
        with open(os.path.join(outdir, f"{case_name}.second.log"), "w") as f:
            f.write(p.stdout + p.stderr)
        return {"second_rc": p.returncode, "second_refused": "正被另一个" in (p.stdout + p.stderr)}
    return act


def cases():
    c = lambda r: r["counters"]
    return [
        Case("baseline", "正常运行 5 秒（对照组）", ["--delay-us", "500", "--duration-sec", "5"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 100_000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("timeout-race", "超时设成 1 µs：超时与回复反复抢跑（回复先到算收到，超时扫描先到则算丢失、回复算迟到）",
             ["--delay-us", "500", "--duration-sec", "5", "--timeout-us", "1"],
             lambda r, x: (c(r)["timeouts"] > 1000 and c(r)["late"] > 1000 and c(r)["timeouts"] - c(r)["late"] <= r["sessions"],
                           f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']:,}，其中迟到收到 {c(r)['late']:,}（超时的请求事后几乎都等到了迟到的回复）")),
        Case("blackhole", "对端 MAC 填一个不存在的地址：发出去的包全部石沉大海",
             ["--delay-us", "500", "--duration-sec", "5", "--dst-mac", "02:00:00:00:00:99"],
             lambda r, x: (c(r)["received"] == 0 and c(r)["timeouts"] > 0 and c(r)["late"] == 0,
                           f"发 {c(r)['sent']:,}，收到 0，超时 {c(r)['timeouts']:,}（100% 丢失，如实计数）")),
        Case("sigint", "运行中途收到 SIGINT（Ctrl-C）", ["--delay-us", "500", "--duration-sec", "60"],
             lambda r, x: ("SIGINT" in r["exit_reason"] and c(r)["timeouts"] == 0 and x["exit_delay"] < 1.0,
                           f"信号发出后 {x['exit_delay'] * 1000:.0f} ms 退出，报告完整，收到 {c(r)['received']:,}"),
             action=sig("INT", 3)),
        Case("sigterm", "运行中途收到 SIGTERM（kill）", ["--delay-us", "500", "--duration-sec", "60"],
             lambda r, x: ("SIGINT/SIGTERM" in r["exit_reason"] and c(r)["timeouts"] == 0 and x["exit_delay"] < 1.0,
                           f"信号发出后 {x['exit_delay'] * 1000:.0f} ms 退出，报告完整"),
             action=sig("TERM", 3)),
        Case("freeze", "进程被冻结 2 秒（SIGSTOP → SIGCONT）：模拟被调度器 / 宿主机长时间夺走 CPU",
             ["--delay-us", "500", "--duration-sec", "8"],
             lambda r, x: (max(r["stalls"]["max_ns"], r["stalls"]["rx_max_ns"]) > 1.5e9 and c(r)["received"] > 100_000,
                           f"停顿检测报告最长停顿 {max(r['stalls']['max_ns'], r['stalls']['rx_max_ns']) / 1e9:.2f} s；"
                           f"恢复后继续运行，收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}，迟到 {c(r)['late']}"),
             action=sig("STOP", 2, pause=2)),
        Case("mbuf-exhaust", "mbuf 池故意配得太小（1040 个，光 RX 环就要 1023 个）：运行中反复取不到 mbuf",
             ["--delay-us", "500", "--duration-sec", "5", "--mbufs", "1040"],
             # RX 环没有空 mbuf 时网卡只能把到达的包丢掉，并记入 imissed。被丢的绝大多数是 echo reply（→ 我们的超时），
             # 偶尔也会是一个不相干的帧（ARP 等），所以 imissed 可以比超时数多出一两个。
             # 反过来，丢得很凶的时候（一次运行上万个），有一部分丢失不在 imissed 里，也不在网卡的任何其他计数器里（见报告 §8）：
             # 这部分未被现有计数器解释，丢在哪里没有确定。所以这里核对的是"imissed 在数量上解释了至少四分之三的丢失"
             # （故障测试的容差，不是"原因完全定位"的证据），并把没解释的部分如实写出来。
             lambda r, x: ((c(r)["no_mbuf"] > 0 or r["port"]["rx_nombuf"] > 0) and r["port"]["imissed"] > 0
                           and r["port"]["imissed"] - c(r)["timeouts"] <= 2 and c(r)["timeouts"] - r["port"]["imissed"] <= 0.25 * c(r)["timeouts"],
                           f"驱动补 RX 环失败 {r['port']['rx_nombuf']:,} 次，发送侧取不到 mbuf {c(r)['no_mbuf']:,} 次（1 µs 后重试）；"
                           f"收到 {c(r)['received']:,}，丢失 {c(r)['timeouts']:,}，网卡 imissed 计数 {r['port']['imissed']:,}"
                           + ("（丢包全部能由网卡的丢弃计数解释）" if abs(c(r)["timeouts"] - r["port"]["imissed"]) <= 2 else
                              f"（网卡的丢弃计数解释了其中 {100 * r['port']['imissed'] / c(r)['timeouts']:.0f}%；"
                              f"其余 {c(r)['timeouts'] - r['port']['imissed']:,} 个不在网卡的任何计数器里，见报告 §8）")),
             env={"BQ_FAULT_SKIP_MBUF_CHECK": "1"}),
        Case("small-rings", "RX / TX 环缩到 256 个描述符", ["--delay-us", "500", "--duration-sec", "5", "--rxd", "256", "--txd", "256"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 100_000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}，tx-full {c(r)['tx_full']}")),
        Case("sessions-1", "边界：只有 1 个 session", ["--delay-us", "500", "--duration-sec", "3", "--sessions", "1"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 1000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("sessions-256", "边界：256 个 session（delay 2000 µs）", ["--delay-us", "2000", "--duration-sec", "5", "--sessions", "256"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 100_000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("delay-0", "边界：delay = 0（收到回复立刻发下一个，4 个 session）", ["--delay-us", "0", "--duration-sec", "3", "--sessions", "4"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 10_000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("payload-8", "边界：payload 最小值 8 字节（只放得下时间戳）", ["--delay-us", "500", "--duration-sec", "3", "--payload", "8"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 100_000 and c(r)["tsc_mismatch"] == 0, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("payload-1472", "边界：payload 最大值 1472 字节（正好一个 1500 MTU 的包；delay 2000 µs，约 0.4 Gbit/s）",
             ["--delay-us", "2000", "--duration-sec", "5", "--payload", "1472"],
             lambda r, x: (c(r)["timeouts"] == 0 and c(r)["received"] > 50_000, f"收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}")),
        Case("bandwidth-1.2G", "满 MTU 的包 + delay 500 µs：单向约 1.2 Gbit/s，把链路 / 对端推到带宽上限附近",
             ["--delay-us", "500", "--duration-sec", "8", "--payload", "1472"],
             lambda r, x: (c(r)["timeouts"] < c(r)["sent"] / 1000 and c(r)["late"] == 0 and r["port"]["imissed"] == 0
                           and r["port"]["ierrors"] == 0 and r["port"]["rx_nombuf"] == 0 and all(v == 0 for _, v in r["port"]["allowance_exceeded"]),
                           f"发 {c(r)['sent']:,}（{c(r)['sent'] / r['elapsed_sec'] * 1514 * 8 / 1e9:.2f} Gbit/s），丢失 {c(r)['timeouts']}"
                           f"（{c(r)['timeouts'] / c(r)['sent'] * 1e6:.0f} ppm），迟到 0；本机网卡 imissed / ierrors / rx_nombuf / AWS 限额计数全为 0 "
                           "→ 包不是在本机丢的（见下文说明）")),
        Case("duration-1", "边界：只跑 1 秒", ["--delay-us", "500", "--duration-sec", "1"],
             lambda r, x: (c(r)["timeouts"] == 0 and 0.9 < r["elapsed_sec"] < 1.2, f"实际 {r['elapsed_sec']:.2f} s，收到 {c(r)['received']:,}")),
        Case("second-instance", "运行中再启动另一个客户端抢同一张网卡",
             ["--delay-us", "500", "--duration-sec", "6"],
             lambda r, x: (x["second_refused"] and x["second_rc"] == 2 and c(r)["timeouts"] == 0,
                           f"第二个进程被拒绝（退出码 {x['second_rc']}）；先启动的进程不受影响，收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}"),
             action="second"),
        # ---- 发送持续失败时还能不能按时收尾（审查意见 R2）。用带钩子的专用构建：BQ_FAULT_TX=<txfull|nombuf>:<起>[-<止>]（秒） ----
        Case("tx-stuck-txfull", "发送从第 2 秒起一直失败（TX 环满），直到 duration（4 秒）到期：必须按时收尾，不能卡在重试里",
             ["--delay-us", "500", "--duration-sec", "4"],
             lambda r, x: (c(r)["tx_full"] > 1000 and r["elapsed_sec"] < 4.5 and c(r)["received"] > 100_000 and c(r)["timeouts"] == 0,
                           f"实际 {r['elapsed_sec']:.2f} s 结束（设定 4 s）；发送失败 {c(r)['tx_full']:,} 次；故障前收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}"),
             env={"BQ_FAULT_TX": "txfull:2"}, build="fault"),
        Case("tx-stuck-nombuf", "发送从第 2 秒起一直失败（取不到 mbuf），直到 duration（4 秒）到期",
             ["--delay-us", "500", "--duration-sec", "4"],
             lambda r, x: (c(r)["no_mbuf"] > 1000 and r["elapsed_sec"] < 4.5 and c(r)["received"] > 100_000 and c(r)["timeouts"] == 0,
                           f"实际 {r['elapsed_sec']:.2f} s 结束（设定 4 s）；发送失败 {c(r)['no_mbuf']:,} 次；故障前收到 {c(r)['received']:,}，超时 {c(r)['timeouts']}"),
             env={"BQ_FAULT_TX": "nombuf:2"}, build="fault"),
        Case("tx-stuck-sigint", "发送从第 2 秒起一直失败，第 4 秒收到 SIGINT（设定 60 秒）：必须立刻收尾",
             ["--delay-us", "500", "--duration-sec", "60"],
             lambda r, x: (x["exit_delay"] < 2 and "SIGINT" in r["exit_reason"] and c(r)["tx_full"] > 1000,
                           f"信号后 {x['exit_delay']:.2f} s 退出；{r['exit_reason']}；发送失败 {c(r)['tx_full']:,} 次"),
             env={"BQ_FAULT_TX": "txfull:2"}, action=sig("INT", 4), build="fault"),
        Case("tx-recovers-late", "发送在第 2 ~ 8 秒之间失败，duration 4 秒：停止之后才恢复。不能等到恢复，也不能在停止阶段再发新请求",
             ["--delay-us", "500", "--duration-sec", "4"],
             lambda r, x: (r["elapsed_sec"] < 4.5 and x["wall_sec"] < 8 and c(r)["tx_full"] > 1000 and c(r)["timeouts"] == 0,
                           f"实际 {r['elapsed_sec']:.2f} s 结束，进程总共存活 {x['wall_sec']:.1f} s（故障要到第 8 秒才解除）；发出 {c(r)['sent']:,} = 收到 {c(r)['received']:,}"),
             env={"BQ_FAULT_TX": "txfull:2-8"}, build="fault"),
        Case("tx-never-works", "发送从一开始就一直失败（duration 2 秒）：一个请求也发不出去，报告仍然要合法",
             ["--delay-us", "500", "--duration-sec", "2"],
             lambda r, x: (c(r)["sent"] == 0 and c(r)["received"] == 0 and r["elapsed_sec"] < 2.5 and c(r)["tx_full"] > 1000,
                           f"发出 {c(r)['sent']}，收到 {c(r)['received']}，实际 {r['elapsed_sec']:.2f} s 结束；发送失败 {c(r)['tx_full']:,} 次；报告正常生成"),
             env={"BQ_FAULT_TX": "txfull:0"}, build="fault"),
        Case("bad-sessions", "非法参数：--sessions 0", ["--delay-us", "500", "--duration-sec", "1", "--sessions", "0"], None, rc=2,
             wants_report=False, expect_text="--sessions 至少为 1"),
        Case("bad-payload", "非法参数：--payload 1473（超过 MTU）", ["--delay-us", "500", "--duration-sec", "1", "--payload", "1473"], None,
             rc=2, wants_report=False, expect_text="--payload 需在"),
        Case("bad-mbufs", "非法参数：--mbufs 太小（未跳过检查）", ["--delay-us", "500", "--duration-sec", "1", "--mbufs", "1023"], None,
             rc=2, wants_report=False, expect_text="太小"),
        Case("bad-pci", "网卡地址不存在", ["--delay-us", "500", "--duration-sec", "1", "--pci", "0000:ff:1f.7"], None, rc=2,
             wants_report=False, expect_text="初始化失败"),
    ]


def run_case(case, client, nic, outdir):
    binary = BIN[client]
    name = f"{case.name}.{client}"
    log_path, json_path = os.path.join(outdir, name + ".log"), os.path.join(outdir, name + ".json")
    base = {"--pci": nic["DPDK_PCI"], "--src-ip": nic["DPDK_IP"], "--dst-ip": nic["PEER_IP"], "--dst-mac": nic["PEER_MAC"]}
    for i in range(0, len(case.args), 2):   # 场景自己给了的参数优先
        base.pop(case.args[i], None)
    cmd = ["sudo"] + [f"{k}={v}" for k, v in case.env.items()] + [os.path.join(bin_dir(case), binary)]
    for k, v in base.items():
        cmd += [k, v]
    cmd += case.args + ["--progress-sec", "0", "--json", json_path]
    extra = {}
    with open(log_path, "w") as log:
        t_start = time.time()
        p = subprocess.Popen(cmd, stdout=log, stderr=subprocess.STDOUT)
        if case.action == "second":
            extra.update(second_instance(BIN["B" if client == "A" else "A"])(binary, nic, outdir, name))
        elif case.action:
            t_sig = case.action(binary, p.pid)
            p.wait(timeout=120)
            extra["exit_delay"] = time.time() - t_sig
        rc = p.wait(timeout=180)
        extra["wall_sec"] = round(time.time() - t_start, 2)
    subprocess.run(["sudo", "chown", f"{os.getuid()}:{os.getgid()}", json_path], stderr=subprocess.DEVNULL, check=False)
    with open(log_path) as f:
        text = f.read()
    checks, detail = [], ""
    checks.append(("没有崩溃", rc >= 0 and "panicked" not in text))
    checks.append((f"退出码 {case.rc}", rc == case.rc))
    if case.wants_report:
        try:
            with open(json_path) as f:
                r = json.load(f)
        except (OSError, ValueError):
            return {"case": case.name, "client": client, "ok": False, "checks": checks + [("写出了报告", False)], "detail": "没有生成报告", "rc": rc}
        c, m = r["counters"], r["mbuf"]
        checks.append(("mbuf 零泄漏", m["avail_initial"] == m["avail_final"]))
        checks.append(("请求对账 = 0", c["sent"] - c["received"] - c["timeouts"] - c["in_flight_at_end"] == 0))
        checks.append(("收包对账 = 0",
                       c["rx_pkts"] - (c["received"] + c["late"] + c["unexpected"] + c["foreign"] + c["tsc_mismatch"] + c["other_rx"] + c["arp_replies"]) == 0))
        samples = next(x["count"] for x in r["metrics"] if x["name"].startswith("in-process"))
        checks.append(("样本数 = received，且 record 时的二次核对全部通过", samples == c["received"] and c["record_mismatch"] == 0))
        ok, detail = case.expect(r, extra)
        checks.append(("场景预期", ok))
        extra["exit_reason"] = r["exit_reason"]
    else:
        checks.append(("给出了明确的错误原因", case.expect_text in text))
        detail = next((l.strip() for l in text.splitlines() if case.expect_text in l), "")[:150]
    return {"case": case.name, "client": client, "ok": all(v for _, v in checks), "checks": checks, "detail": detail, "rc": rc, **extra}


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--only", default="", help="只跑名字里含这个片段的场景")
    ap.add_argument("--clients", default="A,B")
    a = ap.parse_args()
    nic = nic_env()
    subprocess.run([os.path.join(ROOT, "scripts/bind.sh")], stdout=subprocess.DEVNULL, check=True)
    subprocess.run("source $HOME/.cargo/env && cargo build --release -q", shell=True, cwd=ROOT, check=True, executable="/bin/bash")
    outdir = os.path.join(ROOT, "logs/fault", datetime.datetime.now().strftime("%Y%m%d-%H%M%S"))
    os.makedirs(outdir)
    todo = [c for c in cases() if a.only in c.name]
    if any(c.build == "fault" for c in todo):
        subprocess.run("source $HOME/.cargo/env && cargo build --release -q -p async-ping -p raw-ping --features fault --target-dir target-fault",
                       shell=True, cwd=ROOT, check=True, executable="/bin/bash")
    results = []
    for case in todo:
        for client in a.clients.split(","):
            res = run_case(case, client, nic, outdir)
            res["what"] = case.what
            results.append(res)
            failed = [n for n, v in res["checks"] if not v]
            print(f"{'✔' if res['ok'] else '✘'} {case.name:<16} {client}  {res['detail']}" + (f"   ✘ 未通过：{failed}" if failed else ""), flush=True)
    with open(os.path.join(outdir, "summary.json"), "w") as f:
        json.dump(results, f, ensure_ascii=False, indent=1)
    lines = ["| 场景 | 注入的故障 | 客户端 | 结果 | 观察到的行为 |", "|---|---|---|---|---|"]
    for r in results:
        failed = [n for n, v in r["checks"] if not v]
        verdict = "✔ 通过" if r["ok"] else "✘ " + "、".join(failed)
        lines.append(f"| `{r['case']}` | {r['what']} | {r['client']} | {verdict} | {r['detail']} |")
    passed = sum(r["ok"] for r in results)
    lines.append(f"\n共 {len(results)} 项，通过 {passed} 项。每一项都核对：没有崩溃、退出码符合预期、mbuf 零泄漏、请求对账 = 0、收包对账 = 0、"
                 "样本数 = received（record 时的二次核对全部通过），以及该场景特有的预期。")
    with open(os.path.join(outdir, "summary.md"), "w") as f:
        f.write("\n".join(lines) + "\n")
    print(f"\n{passed}/{len(results)} 通过 → {outdir}/summary.md")
    sys.exit(0 if passed == len(results) else 1)


if __name__ == "__main__":
    main()
