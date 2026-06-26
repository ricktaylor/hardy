/*
 * Ping App for Hardy BPNode interop testing (TEST 2 — cFS originates).
 *
 * The mirror image of echo_app: where echo_app reflects, ping_app
 * originates.  On a ground command it injects N ADUs that Channel 0
 * (service 7) wraps into bundles addressed to Hardy's echo service
 * (ipn:1.128).  Hardy reflects each one back to the source EID
 * (ipn:<node>.7); Channel 0 unwraps them onto the Software Bus and
 * ping_app counts them.  A second command reports the tally so the test
 * harness can assert sent == received — a count-based ping with no UDP
 * telemetry hop.
 *
 * Flow (command-driven):
 *   [START N] --> ping_app --> HARDY_TEST_OUT_MID --> Channel 0 wrap -->
 *       bundle --> Hardy echo@128 --> reflect --> Channel 0 unwrap -->
 *       HARDY_TEST_IN_MID --> ping_app (received++)
 *   [REPORT]  --> ping_app --> SysLog "PINGAPP: RESULT sent=X received=Y"
 *
 * Reuses the same Channel 0 ADU proxy wiring as echo_app (cf.
 * bpnode_adup.c), so switching a node between the echo and ping roles is
 * just a matter of which app the startup script loads — no table change.
 */

#include "cfe.h"

/*
 * Message IDs (cf. echo_app / bpnode_adup.c).
 *
 * HARDY_TEST_OUT_MID = outbound to Hardy, topic 0xA1 — read by Channel 0 to
 *                      wrap a bundle; ping_app publishes to originate a ping.
 * HARDY_TEST_IN_MID  = inbound from Hardy, topic 0xA0 — published by Channel 0
 *                      (== BPNODE_ADU_OUT_SEND_TO_MID); ping_app subscribes to
 *                      count replies.
 * PING_APP_CMD_MID   = ground command, V1 CMD topic 0xA1 — delivered via
 *                      ci_lab.  Clear of the cFE core and BPNode (0x1838)
 *                      command MIDs.
 */
#define HARDY_TEST_OUT_MID  0x08A1
#define HARDY_TEST_IN_MID   0x08A0
#define PING_APP_CMD_MID    0x18A1

/* Command function codes */
#define PING_APP_START_CC   0
#define PING_APP_REPORT_CC  1

#define PING_APP_PAYLOAD_SIZE  64
#define PING_APP_PIPE_DEPTH    64
#define PING_APP_SEND_GAP_MS   10  /* pace sends to Channel 0's wakeup drain */

typedef struct
{
    CFE_MSG_CommandHeader_t CommandHeader;
    uint32                  Count;
} PING_APP_StartCmd_t;

typedef struct
{
    CFE_MSG_TelemetryHeader_t TelemetryHeader;
    uint32                    Seq;
    uint8                     Payload[PING_APP_PAYLOAD_SIZE];
} PING_APP_RequestMsg_t;

void PingApp_Main(void)
{
    CFE_SB_PipeId_t  Pipe   = CFE_SB_INVALID_PIPE;
    CFE_SB_Buffer_t *BufPtr = NULL;

    static PING_APP_RequestMsg_t Request;

    uint32 Target   = 0;
    uint32 Sent     = 0;
    uint32 Received = 0;
    bool   Active   = false;

    if (CFE_SB_CreatePipe(&Pipe, PING_APP_PIPE_DEPTH, "PING_APP_PIPE") != CFE_SUCCESS)
    {
        CFE_ES_WriteToSysLog("PingApp: pipe creation failed\n");
        return;
    }

    if (CFE_SB_Subscribe(CFE_SB_ValueToMsgId(PING_APP_CMD_MID), Pipe) != CFE_SUCCESS ||
        CFE_SB_Subscribe(CFE_SB_ValueToMsgId(HARDY_TEST_IN_MID), Pipe) != CFE_SUCCESS)
    {
        CFE_ES_WriteToSysLog("PingApp: subscribe failed\n");
        return;
    }

    CFE_MSG_Init(&Request.TelemetryHeader.Msg,
                 CFE_SB_ValueToMsgId(HARDY_TEST_OUT_MID), sizeof(Request));

    CFE_ES_WriteToSysLog("PingApp: ready (command-driven Hardy ping originator)\n");

    while (CFE_ES_RunLoop(NULL))
    {
        /* While a burst is in flight, poll so the loop also gets to send;
         * otherwise block until the next command or reply arrives. */
        int32 TimeOut = (Active && Sent < Target) ? CFE_SB_POLL : CFE_SB_PEND_FOREVER;

        if (CFE_SB_ReceiveBuffer(&BufPtr, Pipe, TimeOut) == CFE_SUCCESS && BufPtr != NULL)
        {
            CFE_SB_MsgId_t MsgId = CFE_SB_INVALID_MSG_ID;
            CFE_MSG_GetMsgId(&BufPtr->Msg, &MsgId);

            if (CFE_SB_MsgIdToValue(MsgId) == PING_APP_CMD_MID)
            {
                CFE_MSG_FcnCode_t FcnCode = 0;
                CFE_MSG_GetFcnCode(&BufPtr->Msg, &FcnCode);

                if (FcnCode == PING_APP_START_CC)
                {
                    const PING_APP_StartCmd_t *Cmd = (const PING_APP_StartCmd_t *)BufPtr;

                    Target   = Cmd->Count;
                    Sent     = 0;
                    Received = 0;
                    Active   = true;
                    CFE_ES_WriteToSysLog("PingApp: START count=%u\n", (unsigned int)Target);
                }
                else if (FcnCode == PING_APP_REPORT_CC)
                {
                    CFE_ES_WriteToSysLog("PINGAPP: RESULT sent=%u received=%u\n",
                                         (unsigned int)Sent, (unsigned int)Received);
                }
            }
            else if (CFE_SB_MsgIdToValue(MsgId) == HARDY_TEST_IN_MID)
            {
                Received++;
            }
        }

        if (Active && Sent < Target)
        {
            Request.Seq = Sent;
            CFE_SB_TimeStampMsg(&Request.TelemetryHeader.Msg);
            CFE_SB_TransmitMsg(&Request.TelemetryHeader.Msg, true);
            Sent++;
            OS_TaskDelay(PING_APP_SEND_GAP_MS);
        }
    }
}
