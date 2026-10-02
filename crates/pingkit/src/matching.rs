//! 一个到达的 echo reply 算不算"这个 session 正在等的那个请求"的应答。A 和 B 共用这一个函数，保证两边的判定语义相同。
//!
//! 判定必须在**把请求标记为完成之前**做完：`seq` 只有 16 位，每个 session 约 43 秒回绕一次，
//! 一个迟到很久或被重放的旧回复有可能恰好撞上当前的 `id/seq`。所以除了 `seq` 还要核对回复里带回的发送时间戳——
//! 它等于我们发这个请求时写进包里的 T0，每个请求都不同。对不上的回复被拒绝（单独计数、释放 mbuf），
//! **原请求保持在途、原超时时刻不变**：之后真正的回复照常完成它，一直不来则照常超时。
//!
//! 这次比较发生在 T2 → T3 之内（段②），A、B 各多一次 8 字节的比较。

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// 就是在等的那个请求的应答：请求完成
    Accept,
    /// `seq` 对得上，回带的时间戳对不上：不是这个请求的应答。拒绝，请求继续等
    TscMismatch,
    /// 对应一个已经超时的请求
    Late,
    /// 对不上任何在途或已超时的请求
    Unexpected,
}

/// `expect` = 正在等的请求的（seq，发送时写入的 T0）；没有在途请求时为 `None`。
/// `timed_out_before` 只在不匹配时才被调用（冷路径）。
#[inline(always)]
pub fn judge(expect: Option<(u16, u64)>, seq: u16, echoed_tsc: u64, timed_out_before: impl FnOnce() -> bool) -> Verdict {
    match expect {
        Some((want, t0)) if want == seq => {
            if t0 == echoed_tsc {
                Verdict::Accept
            } else {
                Verdict::TscMismatch
            }
        }
        _ if timed_out_before() => Verdict::Late,
        _ => Verdict::Unexpected,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_table() {
        let no = || false;
        let yes = || true;
        // 在等 (seq 7, T0 1000)
        assert_eq!(judge(Some((7, 1000)), 7, 1000, no), Verdict::Accept);
        assert_eq!(judge(Some((7, 1000)), 7, 999, no), Verdict::TscMismatch);
        // seq 对得上时，即使这个 seq 以前超时过（回绕），也按时间戳判：对不上就是 TscMismatch，不是 Late
        assert_eq!(judge(Some((7, 1000)), 7, 999, yes), Verdict::TscMismatch);
        assert_eq!(judge(Some((7, 1000)), 6, 1000, yes), Verdict::Late);
        assert_eq!(judge(Some((7, 1000)), 6, 1000, no), Verdict::Unexpected);
        // 没有在途请求
        assert_eq!(judge(None, 7, 1000, yes), Verdict::Late);
        assert_eq!(judge(None, 7, 1000, no), Verdict::Unexpected);
    }

    /// 匹配时不应去查"以前超时过没有"（那是冷路径）。
    #[test]
    fn hot_path_does_not_consult_the_timeout_history() {
        let mut called = false;
        let v = judge(Some((1, 5)), 1, 5, || {
            called = true;
            false
        });
        assert_eq!(v, Verdict::Accept);
        assert!(!called);
    }
}
