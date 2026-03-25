/*
 * Custom BPNode ADU Proxy Table for Hardy interop testing.
 *
 * Echo wiring via cFE Software Bus:
 *   Channel 0 ADU Out: publishes incoming bundle payloads to BPNODE_ADU_OUT_SEND_TO_MID
 *   Channel 1 ADU In:  subscribes to BPNODE_ADU_OUT_SEND_TO_MID, feeds back to BPLib
 *
 * This creates an echo path: bundle in -> SB -> bundle out, with no separate echo app.
 */

#include "cfe.h"
#include "fwp_adup.h"
#include "cfe_tbl_filedef.h"
#include "bpnode_msgids.h"

BPA_ADUP_Config_t ADUProxyTable[BPLIB_MAX_NUM_CHANNELS] = {
    /* Channel 0: Echo inbound — receives bundles from BPLib, publishes to SB.
     * No SB subscriptions needed (NumRecvFrmMsgIds=0).
     * RecvFrmMsgIds/MsgLims use a dummy valid entry to satisfy table validation. */
    {
        .SendToMsgId      = CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        .NumRecvFrmMsgIds = 0,
        .RecvFrmMsgIds    = {
            CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        },
        .MsgLims          = {
            10
        }
    },
    /* Channel 1: Echo outbound — subscribes to SB, creates bundles back to Hardy */
    {
        .SendToMsgId      = CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        .NumRecvFrmMsgIds = 1,
        .RecvFrmMsgIds    = {
            CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        },
        .MsgLims          = {
            10
        }
    }
};

CFE_TBL_FILEDEF(ADUProxyTable, BPNODE.ADUProxyTable, ADU Proxy Config Table, bpnode_adup.tbl)
