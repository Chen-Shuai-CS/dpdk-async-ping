//! 可选的逐样本原始记录（`--samples <文件>`），给离线分析用（`scripts/ci.py`：置信区间、按批内位置拆分段②）。
//!
//! 直方图只能回答"分位数是多少"；要回答"这个分位数有多可信"（置信区间）、"样本之间相关性多强"，
//! 需要按时间顺序排列的原始样本。
//!
//! 对热路径的影响：
//! - 不开（默认）：`push` 里一次必然不成立的比较，没有任何写入；
//! - 开：每个样本顺序写 8 字节（每 8 个样本才碰一条新的缓存行），发生在 sleep 之后的 `record(reply)` 里，
//!   不在段①、段②之内。缓冲区在启动时一次分配并逐页写过（预先触发缺页），运行期间不分配、不缺页；
//!   写满后停止记录（报告里会注明），绝不扩容。
//! - 每写一个样本，顺手把**下一条缓存行**预取进来（v3 加的）。缓冲区有几百 MB，不在缓存里；不预取的话，每 8 个样本
//!   就有一次写入要等内存（约 100 ns），这次写入挂在 CPU 的写入队列里，会让紧随其后的 `send()` 里的写入排不进去——
//!   虽然 `push` 本身不在段①之内，A 的慢发送占比却会从约 0.5% 升到约 3%（实测，`logs/exp/samples-slow/`）。
//!   预取之后，带不带 `--samples` 没有差别。
//!
//! 每个样本压成一个 u64：`段①(16 位) | 段②(16 位) | T2 的低 32 位`，单位都是 TSC 周期。
//! - 段①、段②超过 65535 周期（约 25 µs）时记为 65535（这种样本极少，是被外部打断造成的）；
//! - T2 的低 32 位约 1.6 秒回绕一次；样本按时间顺序写入，相邻样本相差远小于这个值，所以离线可以无歧义地还原时间轴；
//! - 同一次 rx_burst 收到的包共用一个 T2，所以 T2 相同的相邻样本就是"同一批"，由此可得每个样本在批内的位置。

use std::io::Write;
use std::path::Path;

pub const MAGIC: &[u8; 8] = b"BQSAMPL1";
/// 上限：2^27 个样本 = 1 GiB。
pub const MAX_SAMPLES: usize = 1 << 27;
const SEG_MAX: u64 = 0xFFFF;

#[derive(Default)]
pub struct SampleLog {
    buf: Vec<u64>,
    len: usize,
}

#[inline(always)]
pub fn pack(seg1: u64, seg2: u64, t2: u64) -> u64 {
    (seg1.min(SEG_MAX) << 48) | (seg2.min(SEG_MAX) << 32) | (t2 & 0xFFFF_FFFF)
}

/// [`pack`] 的逆：（段①，段②，T2 的低 32 位）。
pub fn unpack(v: u64) -> (u64, u64, u64) {
    (v >> 48, (v >> 32) & SEG_MAX, v & 0xFFFF_FFFF)
}

impl SampleLog {
    /// 预分配 `cap` 个样本的空间，并把每一页都写一遍（现在就触发缺页，而不是在运行中）。
    pub fn with_capacity(cap: usize) -> SampleLog {
        let mut buf = Vec::with_capacity(cap);
        // 用非零值填充：全零填充可能被优化成"向内核要一段惰性清零的内存"，那样页面要到第一次写入时才真正分配
        buf.resize(cap, u64::MAX);
        SampleLog { buf, len: 0 }
    }

    #[inline(always)]
    pub fn push(&mut self, seg1: u64, seg2: u64, t2: u64) {
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = pack(seg1, seg2, t2);
            self.len += 1;
            // 8 个样本之后才会写到那条缓存行；现在就取，到时候写入不用等内存（见模块说明）。越界的地址只是被忽略
            dpdk::tsc::prefetch_write(self.buf.as_ptr().wrapping_add(self.len + 8));
        }
    }

    pub fn enabled(&self) -> bool {
        !self.buf.is_empty()
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.buf[..self.len]
    }

    /// 写文件：32 字节头（魔数、TSC 频率、样本数、保留）+ 每个样本一个小端 u64。
    pub fn write_to(&self, path: &Path, tsc_hz: u64) -> std::io::Result<()> {
        let mut w = std::io::BufWriter::with_capacity(1 << 20, std::fs::File::create(path)?);
        w.write_all(MAGIC)?;
        w.write_all(&tsc_hz.to_le_bytes())?;
        w.write_all(&(self.len as u64).to_le_bytes())?;
        w.write_all(&0u64.to_le_bytes())?;
        for v in self.as_slice() {
            w.write_all(&v.to_le_bytes())?;
        }
        w.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_roundtrip_and_saturation() {
        assert_eq!(unpack(pack(130, 338, 0x1_2345_6789)), (130, 338, 0x2345_6789));
        assert_eq!(unpack(pack(70_000, u64::MAX, 7)), (0xFFFF, 0xFFFF, 7));
    }

    #[test]
    fn disabled_log_records_nothing() {
        let mut s = SampleLog::default();
        s.push(1, 2, 3);
        assert!(!s.enabled() && s.is_empty());
    }

    #[test]
    fn full_log_stops_instead_of_growing() {
        let mut s = SampleLog::with_capacity(2);
        for i in 0..5 {
            s.push(i, i, i);
        }
        assert_eq!((s.len(), s.capacity()), (2, 2));
        assert_eq!(s.as_slice(), &[pack(0, 0, 0), pack(1, 1, 1)]);
    }

    #[test]
    fn file_layout() {
        let mut s = SampleLog::with_capacity(4);
        s.push(10, 20, 30);
        s.push(11, 21, 31);
        let p = std::env::temp_dir().join(format!("bq-samples-test-{}.bin", std::process::id()));
        s.write_to(&p, 2_600_000_000).unwrap();
        let b = std::fs::read(&p).unwrap();
        std::fs::remove_file(&p).unwrap();
        assert_eq!(b.len(), 32 + 2 * 8);
        assert_eq!(&b[..8], MAGIC);
        assert_eq!(u64::from_le_bytes(b[8..16].try_into().unwrap()), 2_600_000_000);
        assert_eq!(u64::from_le_bytes(b[16..24].try_into().unwrap()), 2);
        assert_eq!(unpack(u64::from_le_bytes(b[40..48].try_into().unwrap())), (11, 21, 31));
    }
}
