#!/usr/bin/env bash
# timer 批量唤醒的对照实验：A（现状：一次触发全部到期 timer，再统一 poll）、A-fire1（触发一个、poll 一个）、B 三者轮流，每轮各 60 秒。
set -u
O=$HOME/bq-archive/v4-notes/timer-batch
R=$HOME/dpdk-async-ping
declare -A BIN=( [Abase]=$HOME/.cache/bq-build/exp/fire-base/target/release [Afire1]=$HOME/.cache/bq-build/exp/fire1/target/release [B]=$R/target/release )
orders=("Abase Afire1 B" "Afire1 B Abase" "B Abase Afire1" "Abase B Afire1" "Afire1 Abase B" "B Afire1 Abase")
for r in 1 2 3 4 5 6; do
  for t in ${orders[$((r-1))]}; do
    c=A; [[ $t == B ]] && c=B
    BQ_BIN_DIR=${BIN[$t]} RUN_LOG=/dev/null $R/scripts/run.sh $c --delay-us 500 --duration-sec 60 --progress-sec 0 --json $O/$t-$r.json > $O/$t-$r.log 2>&1
    echo "$(date -u +%T) round $r $t rc=$?" >> $O/progress.txt
  done
done
