/* DPDK 的热路径函数几乎都是 static inline，库里没有符号，Rust 无法直接调用。
 * 这里把本项目用到的几个包成普通函数；编译时开 -O3 -march=native，与 DPDK 自身一致。 */
#ifndef BQ_DPDK_SHIM_H
#define BQ_DPDK_SHIM_H
#include <stdint.h>
struct rte_mbuf;
struct rte_mempool;

uint16_t shim_eth_rx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **pkts, uint16_t n);
uint16_t shim_eth_tx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **pkts, uint16_t n);
struct rte_mbuf *shim_pktmbuf_alloc(struct rte_mempool *mp);
void shim_pktmbuf_free(struct rte_mbuf *m);
uint16_t shim_mbuf_refcnt_read(const struct rte_mbuf *m);
int shim_rte_errno(void);
unsigned shim_lcore_id(void);
#endif
