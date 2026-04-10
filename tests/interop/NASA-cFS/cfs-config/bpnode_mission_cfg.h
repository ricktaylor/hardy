/*
 * BPNode Mission Configuration for Hardy interop testing.
 * Only change from default: PSP driver name → stcpsock_intf
 */

#ifndef BPNODE_MISSION_CFG_H
#define BPNODE_MISSION_CFG_H

#include "bpnode_interface_cfg.h"

#define DEFAULT_UDP_CLA
#define BPNODE_CLA_PSP_DRIVER_NAME "stcpsock_intf"

#define BPNODE_EID_SERVICE_NUM_FOR_CHANNEL_0 7
#define BPNODE_EID_SERVICE_NUM_FOR_CHANNEL_1 8
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_0    200
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_0 64
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_1    400
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_1 42
#define BPNODE_EID_NODE_NUM_FOR_CONTACT_2    600
#define BPNODE_EID_SERVICE_NUM_FOR_CONTACT_2 12

#endif /* BPNODE_MISSION_CFG_H */
