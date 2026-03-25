/*
 * Custom BPNode Mission Configuration for Hardy interop testing.
 * Only change from default: PSP driver name → stcpsock_intf
 * All EID values kept at defaults to pass BPLib validation.
 */

#ifndef BPNODE_MISSION_CFG_H
#define BPNODE_MISSION_CFG_H

#include "bpnode_interface_cfg.h"

#define DEFAULT_UDP_CLA
#define BPNODE_CLA_PSP_DRIVER_NAME "stcpsock_intf"

/* Channel 0: echo inbound (receives ping bundles on service 7) */
#define BPNODE_EID_SERVICE_NUM_FOR_CHANNEL_0 7
/* Channel 1: echo outbound (sends responses from service 8) */
#define BPNODE_EID_SERVICE_NUM_FOR_CHANNEL_1 8
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_0    200
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0 64
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_1    400
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_1 42
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_2    600
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_2 12

#endif /* BPNODE_MISSION_CFG_H */
