/*
 * BPNode ADU Proxy Table for Hardy interop testing.
 *
 * Echo path: CLA In → Channel 0 AduWrapping → ADU_OUT_MID →
 *            echo_app → ECHO_RESPONSE_MID → Channel 0 AduUnwrapping →
 *            BPLib → Contact 0 → CLA Out
 *
 * A relay app is needed because the cFS Software Bus does not deliver
 * messages back to the publishing pipe.  Using Channel 0 (service 7) for
 * both directions ensures the echo source EID matches the pinged
 * destination, per RFC 9171 §4.2.2.
 */

#include "cfe.h"
#include "fwp_adup.h"
#include "cfe_tbl_filedef.h"
#include "bpnode_msgids.h"

/* Echo response message — topic 0xA1, TLM V1.  Must match echo_app. */
#define ECHO_APP_RESPONSE_MID 0x08A1

BPA_ADUP_Config_t ADUProxyTable[BPLIB_MAX_NUM_CHANNELS] = {
    /* Channel 0: publish inbound payloads, subscribe to echo responses */
    {
        .SendToMsgId      = CFE_SB_MSGID_WRAP_VALUE(BPNODE_ADU_OUT_SEND_TO_MID),
        .NumRecvFrmMsgIds = 1,
        .RecvFrmMsgIds    = {
            CFE_SB_MSGID_WRAP_VALUE(ECHO_APP_RESPONSE_MID),
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
