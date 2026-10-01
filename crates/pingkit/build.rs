//! 把"这个二进制是从哪个提交、用哪个编译器构建的"编进程序，写进每份运行报告（JSON 的 `env` 字段），
//! 这样任何一个数字都能追溯到确切的代码版本。
use std::process::Command;

fn run(cmd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn main() {
    // 构建脚本的工作目录是 crates/pingkit，仓库根在上两级
    let commit = run("git", &["-C", "../..", "rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // 只看会影响生成代码的路径：日志、文档的改动不算"脏"
    let dirty = run(
        "git",
        &["-C", "../..", "status", "--porcelain", "--", "crates", "Cargo.toml", "Cargo.lock", ".cargo", "rust-toolchain.toml"],
    )
    .is_some_and(|s| !s.is_empty());
    let rustc = std::env::var("RUSTC").ok().and_then(|r| run(&r, &["-V"])).unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=BQ_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=BQ_GIT_DIRTY={dirty}");
    println!("cargo:rustc-env=BQ_RUSTC={rustc}");
    println!("cargo:rustc-env=BQ_PROFILE={}", std::env::var("PROFILE").unwrap_or_default());
    // 任何源码改动、任何新提交都要重新取一次
    for p in ["..", "../../Cargo.toml", "../../Cargo.lock", "../../.git/logs/HEAD"] {
        println!("cargo:rerun-if-changed={p}");
    }
}
