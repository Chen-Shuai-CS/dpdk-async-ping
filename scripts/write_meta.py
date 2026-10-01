#!/usr/bin/env python3
"""记录一组测量的"身份"：哪一次开机、什么时候开始、哪个代码版本。写成 <目录>/meta.json。

用法：scripts/write_meta.py <meta.json 路径> <名字> [--version v1|v2|...] [--commit <提交号>]

版本的判定：与标签 v1 / v2 / … 相比，会影响生成代码的路径（crates/ 等）完全相同，就算那个版本；都不相同则记为 dev。
"""
import argparse
import json
import subprocess

CODE_PATHS = "crates Cargo.toml Cargo.lock .cargo rust-toolchain.toml"


def sh(cmd):
    return subprocess.run(cmd, shell=True, capture_output=True, text=True).stdout.strip()


def detect_version(ref="HEAD"):
    for tag in sorted(sh("git tag --list 'v[0-9]*'").split(), reverse=True):
        if sh(f"git diff {tag}..{ref} -- {CODE_PATHS} | wc -l") == "0":
            return tag
    return "dev"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("path")
    ap.add_argument("name")
    ap.add_argument("--version", default="")
    ap.add_argument("--commit", default="")
    a = ap.parse_args()
    meta = {
        "name": a.name,
        "version": a.version or detect_version(),
        "boot_id": open("/proc/sys/kernel/random/boot_id").read().strip(),
        "boot_time": sh("uptime -s"),
        "started": sh("date -u '+%Y-%m-%d %H:%M:%S'"),
        "kernel": sh("uname -r"),
        "git_commit": a.commit or sh("git rev-parse --short=12 HEAD"),
    }
    with open(a.path, "w") as f:
        json.dump(meta, f, ensure_ascii=False, indent=1)
    print(f"{a.path}: {meta['name']}，版本 {meta['version']}，开机于 {meta['boot_time']}")


if __name__ == "__main__":
    main()
