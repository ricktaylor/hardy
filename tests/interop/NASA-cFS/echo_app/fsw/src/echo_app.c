/*
 * Echo App for Hardy BPNode interop testing.
 *
 * Bridges the cFS Software Bus self-delivery gap so that a single BPLib
 * channel (Channel 0, service 7) can handle both inbound and outbound
 * echo.  Without this app, Channel 0 would need to subscribe to its own
 * published message, which the SB does not deliver back to the same pipe.
 *
 * Flow:
 *   Channel 0 AduWrapping  --> ADU_OUT_MID  --> [this app] -->
 *   ECHO_RESPONSE_MID  --> Channel 0 AduUnwrapping --> BPLib --> CLA Out
 *
 * The echo response bundle is created by Channel 0 (service 7), so its
 * source EID matches the destination that was pinged — RFC 9171 compliant.
 */

#include "cfe.h"
#include <string.h>

/*
 * Message IDs (V1 TLM format: 0x0800 | topic).
 *
 * ADU_OUT_MID   = BPNode ADU output, topic 0xA0 — published by Channel 0.
 * ECHO_RESP_MID = Echo relay,        topic 0xA1 — subscribed by Channel 0.
 */
#define ADU_OUT_MID    0x08A0
#define ECHO_RESP_MID  0x08A1

#define PIPE_DEPTH  10

void EchoApp_Main(void)
{
    CFE_SB_PipeId_t Pipe;
    CFE_SB_Buffer_t *BufPtr;

    /* Static buffer — 65 KiB payload + TLM header, too large for stack */
    static union {
        CFE_SB_Buffer_t SB;
        uint8           Pad[sizeof(CFE_MSG_TelemetryHeader_t) + 65536];
    } Out;

    if (CFE_SB_CreatePipe(&Pipe, PIPE_DEPTH, "ECHO_PIPE") != CFE_SUCCESS)
    {
        CFE_ES_WriteToSysLog("EchoApp: pipe creation failed\n");
        return;
    }

    if (CFE_SB_Subscribe(CFE_SB_ValueToMsgId(ADU_OUT_MID), Pipe) != CFE_SUCCESS)
    {
        CFE_ES_WriteToSysLog("EchoApp: subscribe failed\n");
        return;
    }

    CFE_ES_WriteToSysLog("EchoApp: relaying ADU payloads for echo\n");

    while (CFE_ES_RunLoop(NULL))
    {
        if (CFE_SB_ReceiveBuffer(&BufPtr, Pipe, CFE_SB_PEND_FOREVER) == CFE_SUCCESS)
        {
            CFE_MSG_Size_t Sz;
            CFE_MSG_GetSize(&BufPtr->Msg, &Sz);

            memcpy(&Out, BufPtr, Sz);
            CFE_MSG_SetMsgId(&Out.SB.Msg, CFE_SB_ValueToMsgId(ECHO_RESP_MID));
            CFE_SB_TransmitMsg(&Out.SB.Msg, true);
        }
    }
}
