#include <rte_errno.h>
#include <rte_ethdev.h>
#include <rte_lcore.h>
#include <rte_mbuf.h>
#include "shim.h"

uint16_t shim_eth_rx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **pkts, uint16_t n)
{ return rte_eth_rx_burst(port_id, queue_id, pkts, n); }

uint16_t shim_eth_tx_burst(uint16_t port_id, uint16_t queue_id, struct rte_mbuf **pkts, uint16_t n)
{ return rte_eth_tx_burst(port_id, queue_id, pkts, n); }

struct rte_mbuf *shim_pktmbuf_alloc(struct rte_mempool *mp) { return rte_pktmbuf_alloc(mp); }
void shim_pktmbuf_free(struct rte_mbuf *m) { rte_pktmbuf_free(m); }
uint16_t shim_mbuf_refcnt_read(const struct rte_mbuf *m) { return rte_mbuf_refcnt_read(m); }
int shim_rte_errno(void) { return rte_errno; }
unsigned shim_lcore_id(void) { return rte_lcore_id(); }
