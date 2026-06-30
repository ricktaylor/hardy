/*
 * BPNode ADU Proxy Table for Hardy interop testing.
 *
 * Channel 0 (service 7) carries both directions of a Hardy round-trip:
 *   inbound  bundle ADU → SB(BPNODE_ADU_OUT_SEND_TO_MID)  [test app reads]
 *   outbound SB(HARDY_TEST_OUT_MID) → bundle ADU          [test app writes]
 *
 * A driver app is needed because the cFS Software Bus does not deliver a
 * message back to the publishing pipe:
 *   - echo_app (TEST 1) reflects inbound ADUs back out — Hardy pings cFS.
 *   - ping_app (TEST 2) originates ADUs and counts replies — cFS pings Hardy.
 * Using one channel (service 7) for both directions keeps the response
 * source EID equal to the pinged destination, per RFC 9171 §4.2.2.
 */

#include "cfe.h"
#include "fwp_adup.h"
#include "cfe_tbl_filedef.h"
#include "bpnode_msgids.h"

/* Test app's outbound message — topic 0xA1, TLM V1.  Channel 0 wraps it
 * into a bundle bound for Hardy.  Must match echo_app/ping_app. */
#define HARDY_TEST_OUT_MID 0x08A1

BPA_ADUP_Config_t ADUProxyTable[BPLIB_MAX_NUM_CHANNELS] = {
    /* Channel 0: publish inbound payloads, subscribe to echo responses */
    {
        .SendToMsgId      = CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        .NumRecvFrmMsgIds = 1,
        .RecvFrmMsgIds    = {
            CFE_SB_MSGID_WRAP_VALUE(HARDY_TEST_OUT_MID),
        },
        .MsgLims          = {
            10
        }
    },
    /* Channel 1: unused */
    {
        .SendToMsgId      = CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        .NumRecvFrmMsgIds = 0,
        .RecvFrmMsgIds    = {
            CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        },
        .MsgLims          = {
            10
        }
    }
};

CFE_TBL_FILEDEF(ADUProxyTable, BPNODE.ADUProxyTable, ADU Proxy Config Table, bpnode_adup.tbl)
