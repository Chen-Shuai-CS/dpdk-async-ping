//! 刚好够用的协议：Ethernet + IPv4（常量头）+ ICMP Echo，以及 ARP 应答。
//!
//! 帧布局（IPv4 IHL=5）：
//! ```text
//! 0      Ethernet 14 B   dst MAC | src MAC | 0x0800
//! 14     IPv4 20 B       全常量，启动时算一次首部校验和
//! 34     ICMP 8 B        type=8 code=0 | cksum(36) | id(38) | seq(40)
//! 42     payload         发送时刻 TSC（大端 8 B）+ 常量 padding
//! ```
//! 每个包只改 id、seq、TSC 和 ICMP 校验和。

pub const ETH_HDR: usize = 14;
pub const IPV4_HDR: usize = 20;
pub const ICMP_HDR: usize = 8;
pub const ICMP_OFF: usize = ETH_HDR + IPV4_HDR; // 34
const OFF_CSUM: usize = ICMP_OFF + 2; // 36
const OFF_ID: usize = ICMP_OFF + 4; // 38
const OFF_SEQ: usize = ICMP_OFF + 6; // 40
const OFF_TSC: usize = ICMP_OFF + ICMP_HDR; // 42，偶数偏移 → 16 位对齐，校验和可直接按字相加
pub const TSC_LEN: usize = 8;

const ETHERTYPE_IPV4: u16 = 0x0800;
const ETHERTYPE_ARP: u16 = 0x0806;
const IPPROTO_ICMP: u8 = 1;
const ICMP_ECHO_REQUEST: u8 = 8;
const ICMP_ECHO_REPLY: u8 = 0;

/// 本端与对端的地址。
#[derive(Debug, Clone, Copy)]
pub struct Endpoints {
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub src_ip: [u8; 4],
    pub dst_ip: [u8; 4],
}

// ---------------------------------------------------------------------------
// Internet checksum（RFC 1071）
// ---------------------------------------------------------------------------

/// 16 位反码和（未取反、未折叠到 16 位也可以继续累加）。按网络字节序把相邻两字节当作一个字。
#[inline]
pub fn sum16(data: &[u8]) -> u32 {
    let (words, rest) = data.as_chunks::<2>();
    let mut s: u32 = words.iter().map(|w| u16::from_be_bytes(*w) as u32).sum();
    if let [last] = rest {
        s += (*last as u32) << 8;
    }
    s
}

/// 把 32 位累加值折叠成 16 位反码和（进位折回最低位）。
#[inline]
pub fn fold(mut s: u32) -> u16 {
    s = (s & 0xffff) + (s >> 16);
    s = (s & 0xffff) + (s >> 16);
    s as u16
}

/// 完整校验和：对数据求反码和再取反。参考实现，用于启动时和单测。
pub fn checksum(data: &[u8]) -> u16 {
    !fold(sum16(data))
}

// ---------------------------------------------------------------------------
// Echo request 模板
// ---------------------------------------------------------------------------

/// 预先构造好的 echo request 帧。id / seq / TSC 字段为 0、ICMP 校验和字段为 0，
/// 并预先算好其余所有字（常量部分）的反码和 `base_sum`。
///
/// 每个包的校验和 = !fold(base_sum + id + seq + TSC 的 4 个字)。
/// 这就是 RFC 1624 的思路：只把变化的字加进去，不重算整个包；而且因为反码加法满足交换律，
/// 结果与全量重算逐位相同（单测交叉验证），不存在 RFC 1141 那种 0x0000/0xFFFF 的边界问题。
#[derive(Clone)]
pub struct EchoTemplate {
    frame: Vec<u8>,
    base_sum: u32,
}

impl EchoTemplate {
    /// `payload_len` ≥ 8（要放 TSC）。帧长 = 42 + payload_len。
    pub fn new(ep: &Endpoints, payload_len: usize) -> EchoTemplate {
        assert!(payload_len >= TSC_LEN, "payload 至少 {TSC_LEN} 字节（放发送时刻 TSC）");
        let total = ICMP_OFF + ICMP_HDR + payload_len;
        let mut f = vec![0u8; total];
        // Ethernet
        f[0..6].copy_from_slice(&ep.dst_mac);
        f[6..12].copy_from_slice(&ep.src_mac);
        f[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());
        // IPv4（20 B，全部常量）
        let ip = &mut f[ETH_HDR..ICMP_OFF];
        ip[0] = 0x45; // version 4, IHL 5
        ip[1] = 0; // DSCP/ECN
        ip[2..4].copy_from_slice(&((IPV4_HDR + ICMP_HDR + payload_len) as u16).to_be_bytes());
        ip[4..6].copy_from_slice(&0u16.to_be_bytes()); // identification：DF 置位时不参与分片，常量即可
        ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes()); // DF
        ip[8] = 64; // TTL
        ip[9] = IPPROTO_ICMP;
        ip[12..16].copy_from_slice(&ep.src_ip);
        ip[16..20].copy_from_slice(&ep.dst_ip);
        let c = checksum(ip);
        ip[10..12].copy_from_slice(&c.to_be_bytes());
        // ICMP：type 8 code 0，其余先置 0；padding 用固定花纹
        f[ICMP_OFF] = ICMP_ECHO_REQUEST;
        for (i, b) in f[OFF_TSC + TSC_LEN..].iter_mut().enumerate() {
            *b = (i as u8).wrapping_add(0x10);
        }
        let base_sum = sum16(&f[ICMP_OFF..]);
        EchoTemplate { frame: f, base_sum }
    }

    pub fn len(&self) -> usize {
        self.frame.len()
    }

    pub fn is_empty(&self) -> bool {
        self.frame.is_empty()
    }

    pub fn bytes(&self) -> &[u8] {
        &self.frame
    }

    /// 在 `dst`（长度 ≥ 模板长度）里写出一个完整的 echo request。热路径：一次 memcpy + 6 次加法。
    #[inline]
    pub fn write_request(&self, dst: &mut [u8], id: u16, seq: u16, tsc: u64) {
        let dst = &mut dst[..self.frame.len()];
        dst.copy_from_slice(&self.frame);
        dst[OFF_ID..OFF_ID + 2].copy_from_slice(&id.to_be_bytes());
        dst[OFF_SEQ..OFF_SEQ + 2].copy_from_slice(&seq.to_be_bytes());
        dst[OFF_TSC..OFF_TSC + 8].copy_from_slice(&tsc.to_be_bytes());
        let s = self.base_sum
            + id as u32
            + seq as u32
            + (tsc >> 48) as u32
            + ((tsc >> 32) & 0xffff) as u32
            + ((tsc >> 16) & 0xffff) as u32
            + (tsc & 0xffff) as u32;
        dst[OFF_CSUM..OFF_CSUM + 2].copy_from_slice(&(!fold(s)).to_be_bytes());
    }
}

// ---------------------------------------------------------------------------
// 收包分类
// ---------------------------------------------------------------------------

/// 收到的帧是什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rx {
    /// 发给我的 ICMP echo reply。`tx_tsc` 是对端原样带回的、我们发送时写入的 TSC。
    EchoReply { id: u16, seq: u16, tx_tsc: u64 },
    /// 问我 IP 的 ARP request —— 必须回答，否则对端不知道把 reply 发到哪个 MAC。
    ArpRequest,
    /// 其他一切（不是给我的、不认识的协议、畸形包）。
    Other,
}

/// 解析一个帧。只检查必要字段：不要对对端 IP 头的 TTL、identification 等做任何假设。
#[inline]
pub fn classify(f: &[u8], my_ip: [u8; 4]) -> Rx {
    if f.len() < ETH_HDR + 20 {
        return Rx::Other;
    }
    match u16::from_be_bytes([f[12], f[13]]) {
        ETHERTYPE_IPV4 => {
            let ip = &f[ETH_HDR..];
            let ihl = ((ip[0] & 0x0f) as usize) * 4;
            if ip[0] >> 4 != 4 || ihl < IPV4_HDR || ip[9] != IPPROTO_ICMP || ip[16..20] != my_ip {
                return Rx::Other;
            }
            // 不处理分片（我们发的包带 DF，reply 不会分片）
            if u16::from_be_bytes([ip[6], ip[7]]) & 0x3fff != 0 {
                return Rx::Other;
            }
            let icmp_off = ETH_HDR + ihl;
            if f.len() < icmp_off + ICMP_HDR + TSC_LEN {
                return Rx::Other;
            }
            let icmp = &f[icmp_off..];
            if icmp[0] != ICMP_ECHO_REPLY || icmp[1] != 0 {
                return Rx::Other;
            }
            let id = u16::from_be_bytes([icmp[4], icmp[5]]);
            let seq = u16::from_be_bytes([icmp[6], icmp[7]]);
            let mut t = [0u8; 8];
            t.copy_from_slice(&icmp[ICMP_HDR..ICMP_HDR + TSC_LEN]);
            Rx::EchoReply { id, seq, tx_tsc: u64::from_be_bytes(t) }
        }
        ETHERTYPE_ARP => {
            let a = &f[ETH_HDR..];
            if a.len() >= 28
                && a[0..2] == [0, 1]          // htype = Ethernet
                && a[2..4] == [0x08, 0x00]    // ptype = IPv4
                && a[4] == 6 && a[5] == 4     // hlen, plen
                && a[6..8] == [0, 1]          // oper = request
                && a[24..28] == my_ip
            {
                Rx::ArpRequest
            } else {
                Rx::Other
            }
        }
        _ => Rx::Other,
    }
}

/// 把一个"问我 IP"的 ARP request **原地**改写成 ARP reply（省一次分配）。
/// 调用前应已由 [`classify`] 确认是 [`Rx::ArpRequest`]。
pub fn arp_reply_in_place(f: &mut [u8], my_mac: [u8; 6], my_ip: [u8; 4]) {
    let mut req_mac = [0u8; 6];
    let mut req_ip = [0u8; 4];
    req_mac.copy_from_slice(&f[ETH_HDR + 8..ETH_HDR + 14]); // sender hardware address
    req_ip.copy_from_slice(&f[ETH_HDR + 14..ETH_HDR + 18]); // sender protocol address
    // Ethernet：发回给请求者
    f[0..6].copy_from_slice(&req_mac);
    f[6..12].copy_from_slice(&my_mac);
    let a = &mut f[ETH_HDR..];
    a[6..8].copy_from_slice(&2u16.to_be_bytes()); // oper = reply
    a[8..14].copy_from_slice(&my_mac); // sender = 我
    a[14..18].copy_from_slice(&my_ip);
    a[18..24].copy_from_slice(&req_mac); // target = 请求者
    a[24..28].copy_from_slice(&req_ip);
}

/// 解析 "aa:bb:cc:dd:ee:ff"。
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut it = s.split(':');
    for b in &mut m {
        *b = u8::from_str_radix(it.next()?, 16).ok()?;
    }
    it.next().is_none().then_some(m)
}

/// 解析 "a.b.c.d"。
pub fn parse_ipv4(s: &str) -> Option<[u8; 4]> {
    let ip: std::net::Ipv4Addr = s.parse().ok()?;
    Some(ip.octets())
}

#[cfg(test)]
mod tests;
