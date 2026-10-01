use super::*;

fn ep() -> Endpoints {
    Endpoints {
        src_mac: parse_mac("06:ff:df:7d:66:91").unwrap(),
        dst_mac: parse_mac("06:ff:fd:b6:f0:cd").unwrap(),
        src_ip: parse_ipv4("10.202.15.133").unwrap(),
        dst_ip: parse_ipv4("10.202.8.15").unwrap(),
    }
}

/// 简单的 xorshift，避免引入 rand 依赖。
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

/// 模拟对端 Linux 内核的 icmp_echo：交换地址、type 改 0、重算校验和；TTL 用对端自己的值。
fn make_reply(req: &[u8], ttl: u8) -> Vec<u8> {
    let mut r = req.to_vec();
    r[0..6].copy_from_slice(&req[6..12]);
    r[6..12].copy_from_slice(&req[0..6]);
    r[ETH_HDR + 12..ETH_HDR + 16].copy_from_slice(&req[ETH_HDR + 16..ETH_HDR + 20]);
    r[ETH_HDR + 16..ETH_HDR + 20].copy_from_slice(&req[ETH_HDR + 12..ETH_HDR + 16]);
    r[ETH_HDR + 8] = ttl;
    r[ETH_HDR + 10..ETH_HDR + 12].copy_from_slice(&[0, 0]);
    let c = checksum(&r[ETH_HDR..ICMP_OFF]);
    r[ETH_HDR + 10..ETH_HDR + 12].copy_from_slice(&c.to_be_bytes());
    r[ICMP_OFF] = 0;
    r[OFF_CSUM..OFF_CSUM + 2].copy_from_slice(&[0, 0]);
    let c = checksum(&r[ICMP_OFF..]);
    r[OFF_CSUM..OFF_CSUM + 2].copy_from_slice(&c.to_be_bytes());
    r
}

#[test]
fn frame_layout_and_ip_checksum() {
    let t = EchoTemplate::new(&ep(), 64);
    assert_eq!(t.len(), 106, "SPEC：默认 64B payload → 106B 帧");
    let f = t.bytes();
    assert_eq!(&f[12..14], &[0x08, 0x00]);
    assert_eq!(u16::from_be_bytes([f[16], f[17]]), 92); // IPv4 总长度
    assert_eq!(fold(sum16(&f[ETH_HDR..ICMP_OFF])), 0xffff, "IPv4 头校验和应当自洽");
}

#[test]
fn incremental_checksum_matches_full_recompute() {
    for payload in [8usize, 64, 65, 128, 1000] {
        let t = EchoTemplate::new(&ep(), payload);
        let mut buf = vec![0u8; t.len()];
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15 ^ payload as u64);
        for i in 0..200_000u32 {
            // 前几轮刻意覆盖边界值
            let (id, seq, tsc) = match i {
                0 => (0, 0, 0),
                1 => (0xffff, 0xffff, u64::MAX),
                2 => (63, 0xfffe, 0x0000_1234_5678_ffff),
                _ => (rng.next() as u16, rng.next() as u16, rng.next()),
            };
            t.write_request(&mut buf, id, seq, tsc);
            let written = u16::from_be_bytes([buf[OFF_CSUM], buf[OFF_CSUM + 1]]);
            let mut z = buf.clone();
            z[OFF_CSUM..OFF_CSUM + 2].copy_from_slice(&[0, 0]);
            assert_eq!(written, checksum(&z[ICMP_OFF..]), "payload={payload} id={id} seq={seq} tsc={tsc:#x}");
            assert_eq!(fold(sum16(&buf[ICMP_OFF..])), 0xffff);
        }
    }
}

#[test]
fn classify_echo_reply_roundtrip() {
    let e = ep();
    let t = EchoTemplate::new(&e, 64);
    let mut req = vec![0u8; t.len()];
    t.write_request(&mut req, 42, 7, 0xdead_beef_0123_4567);
    let reply = make_reply(&req, 127); // 实测对端回包 TTL=127
    assert_eq!(classify(&reply, e.src_ip, e.dst_ip), Rx::EchoReply { id: 42, seq: 7, tx_tsc: 0xdead_beef_0123_4567 });
    // 不是给我的 / 是 request 而不是 reply / 截断的包 → Other
    assert_eq!(classify(&reply, [10, 0, 0, 1], e.dst_ip), Rx::Other);
    assert_eq!(classify(&req, e.dst_ip, e.src_ip), Rx::Other);
    assert_eq!(classify(&reply[..40], e.src_ip, e.dst_ip), Rx::Other);
}

#[test]
fn arp_request_is_answered_in_place() {
    let e = ep();
    let peer_mac = e.dst_mac;
    let peer_ip = e.dst_ip;
    // 对端广播 "谁是 10.202.15.133？"（以太网最短帧 60 B，含 padding）
    let mut f = vec![0u8; 60];
    f[0..6].copy_from_slice(&[0xff; 6]);
    f[6..12].copy_from_slice(&peer_mac);
    f[12..14].copy_from_slice(&[0x08, 0x06]);
    let a = &mut f[ETH_HDR..];
    a[0..8].copy_from_slice(&[0, 1, 8, 0, 6, 4, 0, 1]);
    a[8..14].copy_from_slice(&peer_mac);
    a[14..18].copy_from_slice(&peer_ip);
    a[24..28].copy_from_slice(&e.src_ip);
    assert_eq!(classify(&f, e.src_ip, e.dst_ip), Rx::ArpRequest);
    assert_eq!(classify(&f, [10, 0, 0, 9], e.dst_ip), Rx::Other, "问的不是我");

    arp_reply_in_place(&mut f, e.src_mac, e.src_ip);
    assert_eq!(&f[0..6], &peer_mac);
    assert_eq!(&f[6..12], &e.src_mac);
    let a = &f[ETH_HDR..];
    assert_eq!(&a[6..8], &[0, 2]);
    assert_eq!(&a[8..14], &e.src_mac);
    assert_eq!(&a[14..18], &e.src_ip);
    assert_eq!(&a[18..24], &peer_mac);
    assert_eq!(&a[24..28], &peer_ip);
    assert_eq!(classify(&f, e.src_ip, e.dst_ip), Rx::Other, "reply 不应再被当成 request");
}

#[test]
fn parse_helpers() {
    assert_eq!(parse_mac("06:ff:fd:b6:f0:cd"), Some([6, 0xff, 0xfd, 0xb6, 0xf0, 0xcd]));
    assert_eq!(parse_mac("06:ff:fd:b6:f0"), None);
    assert_eq!(parse_ipv4("10.202.8.15"), Some([10, 202, 8, 15]));
}

#[test]
fn echo_reply_from_another_host_is_never_delivered_to_a_session() {
    // 实测遇到过：别的主机发给我们 IP 的 echo reply（id=16509）。即使它的 id / seq 碰巧合法，也不能当成 session 的回复。
    let e = ep();
    let t = EchoTemplate::new(&e, 64);
    let mut req = vec![0u8; t.len()];
    t.write_request(&mut req, 5, 100, 0x1122_3344_5566_7788); // id=5、seq=100：完全像是 session 5 在等的回复
    let mut reply = make_reply(&req, 64);
    let stranger = [10, 202, 9, 99];
    reply[ETH_HDR + 12..ETH_HDR + 16].copy_from_slice(&stranger); // 源 IP 改成别的主机
    assert_eq!(classify(&reply, e.src_ip, e.dst_ip), Rx::ForeignEchoReply { src: stranger, id: 5, seq: 100 });
}
