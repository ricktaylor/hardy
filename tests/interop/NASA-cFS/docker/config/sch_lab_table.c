/* Schedule table for BPNode interop testing — sends periodic wakeups to BPNode */

#include "cfe_tbl_filedef.h"
#include "sch_lab_tbl.h"
#include "cfe_sb_api_typedefs.h"
#include "cfe_msgids.h"
#include "bpnode_msgids.h"

SCH_LAB_ScheduleTable_t Schedule = {
    .TickRate = 100,
    .Config   = {
        {CFE_SB_MSGID_WRAP_VALUE(CFE_ES_SEND_HK_MID), 100, 0},
        {CFE_SB_MSGID_WRAP_VALUE(CFE_SB_SEND_HK_MID), 100, 0},
        {CFE_SB_MSGID_WRAP_VALUE(CFE_EVS_SEND_HK_MID), 100, 0},
        {CFE_SB_MSGID_WRAP_VALUE(CFE_TIME_SEND_HK_MID), 100, 0},
        {CFE_SB_MSGID_WRAP_VALUE(CFE_TBL_SEND_HK_MID), 100, 0},
        {CFE_SB_MSGID_WRAP_VALUE(BPNODE_WAKEUP_MID), 1, 0},
    }
};

CFE_TBL_FILEDEF(Schedule, SCH_LAB_APP.Schedule, Schedule Lab MsgID Table, sch_lab_table.tbl)
