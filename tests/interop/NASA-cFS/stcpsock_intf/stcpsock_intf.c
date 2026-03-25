/*
 * STCP (Simple TCP Convergence Layer) PSP Module for NASA cFS BPNode
 *
 * Implements the IODriver interface using TCP sockets with STCP framing:
 * each bundle is preceded by a 4-byte big-endian u32 length prefix.
 *
 * This module is structurally based on the udpsock_intf PSP module but
 * uses SOCK_STREAM (TCP) instead of SOCK_DGRAM (UDP), with STCP framing
 * to delimit bundle boundaries on the TCP stream.
 *
 * For interoperability testing only — not flight-qualified.
 */

#include <poll.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <netinet/in.h>
#include <netinet/tcp.h>
#include <arpa/inet.h>
#include <stdlib.h>

#include "osapi.h"
#include "cfe_psp.h"
#include "cfe_psp_module.h"
#include "iodriver_impl.h"
#include "iodriver_packet_io.h"

/*
 * Global Data
 */

static int32 stcpsockDevCmd(uint32 CommandCode, uint16 InstanceNumber, uint16 SubChannel,
                            CFE_PSP_IODriver_Arg_t Arg);

CFE_PSP_IODriver_API_t stcpsock_intf_DevApi =
{
    .DeviceCommand = stcpsockDevCmd
};

CFE_PSP_MODULE_DECLARE_IODEVICEDRIVER(stcpsock_intf);

/*
 * Macro Definitions
 */

#define STCPSOCK_MAX_INTERFACE_DEVS 6
#define STCPSOCK_RECV_TIMEOUT_MS    10
#define STCPSOCK_CONNECT_TIMEOUT_MS 1000

/*
 * Local Data Definitions
 */

typedef struct
{
    char IntfName[OS_MAX_API_NAME];
    CFE_PSP_IODriver_Direction_t ConfigDir;
    struct sockaddr_in LocalAddr;
    int ListenFd;       /* Listening socket (INPUT only), -1 if unused */
    int DeviceFd;       /* Connected socket, -1 if not connected */

    /* STCP read state machine (INPUT only) */
    uint8  LenBuf[4];       /* Accumulator for 4-byte length prefix */
    size_t LenBytesRead;    /* Bytes of length prefix received so far */
    uint32 PendingLen;      /* Payload length from decoded prefix, 0 = need header */
    size_t PayloadBytesRead;/* Payload bytes received so far */
} stcpsock_intf_State_t;

static struct
{
    uint32 MyPspModuleId;
    stcpsock_intf_State_t State[STCPSOCK_MAX_INTERFACE_DEVS];
} StcpSock_Global;


/*
 * Local Function Declarations
 */

static int32 stcpsock_intf_OpenPort    (stcpsock_intf_State_t *State, uint32 Instance);
static int32 stcpsock_intf_Configure   (stcpsock_intf_State_t *State, uint32 Instance, const char *ConfigString);
void  stcpsock_intf_Init        (uint32 PspModuleId);
static int32 stcpsock_intf_ReadPacket  (stcpsock_intf_State_t *State, CFE_PSP_IODriver_ReadPacketBuffer_t *Dest);
static int32 stcpsock_intf_WritePacket (stcpsock_intf_State_t *State, CFE_PSP_IODriver_WritePacketBuffer_t *Source);

/*
 * Helper: write exactly `len` bytes, retrying on partial writes.
 * Returns number of bytes written, or -1 on error.
 */
static ssize_t stcpsock_writeFull(int fd, const void *buf, size_t len)
{
    size_t total = 0;
    while (total < len)
    {
        ssize_t n = send(fd, (const uint8 *)buf + total, len - total, MSG_NOSIGNAL);
        if (n < 0)
            return -1;
        total += n;
    }
    return (ssize_t)total;
}

/*
 * Helper: close the connected socket and reset read state.
 */
static void stcpsock_resetConnection(stcpsock_intf_State_t *State)
{
    if (State->DeviceFd >= 0)
    {
        close(State->DeviceFd);
        State->DeviceFd = -1;
    }
    State->LenBytesRead    = 0;
    State->PendingLen      = 0;
    State->PayloadBytesRead = 0;
}

/*
 * stcpsock_intf_OpenPort - create and configure socket
 */
static int32 stcpsock_intf_OpenPort(stcpsock_intf_State_t *State, uint32 Instance)
{
    int32 Result = CFE_PSP_ERROR;
    int SockOptVal = 1;

    State->LocalAddr.sin_family = AF_INET;

    if (State->ConfigDir == CFE_PSP_IODriver_Direction_INPUT_ONLY)
    {
        /* INPUT: create a TCP listening socket */
        int listenFd = socket(AF_INET, SOCK_STREAM, 0);
        if (listenFd < 0)
        {
            perror("stcpsock_intf_OpenPort: socket()");
            return CFE_PSP_ERROR;
        }

        setsockopt(listenFd, SOL_SOCKET, SO_REUSEADDR, &SockOptVal, sizeof(SockOptVal));

        OS_printf("%s(): Listening on %s:%d\n", __func__,
                  inet_ntoa(State->LocalAddr.sin_addr), ntohs(State->LocalAddr.sin_port));

        if (bind(listenFd, (struct sockaddr *)&State->LocalAddr, sizeof(State->LocalAddr)) < 0)
        {
            perror("stcpsock_intf_OpenPort: bind()");
            close(listenFd);
            return CFE_PSP_ERROR;
        }

        if (listen(listenFd, 8) < 0)
        {
            perror("stcpsock_intf_OpenPort: listen()");
            close(listenFd);
            return CFE_PSP_ERROR;
        }

        /* Set non-blocking so accept() doesn't block the CLA task */
        fcntl(listenFd, F_SETFL, fcntl(listenFd, F_GETFL) | O_NONBLOCK);

        State->ListenFd = listenFd;
        State->DeviceFd = -1;
        State->LenBytesRead = 0;
        State->PendingLen = 0;
        State->PayloadBytesRead = 0;

        OS_printf("CFE_PSP: STCP listening socket ready: %s:%d\n",
                  inet_ntoa(State->LocalAddr.sin_addr), ntohs(State->LocalAddr.sin_port));
        Result = CFE_PSP_SUCCESS;
    }
    else
    {
        /* OUTPUT: socket created lazily on first write (peer may not be up yet) */
        OS_printf("%s(): Output configured for %s:%d (lazy connect)\n", __func__,
                  inet_ntoa(State->LocalAddr.sin_addr), ntohs(State->LocalAddr.sin_port));
        State->ListenFd = -1;
        State->DeviceFd = -1;
        Result = CFE_PSP_SUCCESS;
    }

    return Result;
}

/*
 * stcpsock_intf_Configure - parse "name=", "port=", "IpAddr=" strings
 * (same interface as udpsock_intf)
 */
static int32 stcpsock_intf_Configure(stcpsock_intf_State_t *State, uint32 Instance,
                                     const char *ConfigString)
{
    if (strncmp(ConfigString, "name=", 5) == 0)
    {
        strncpy(State->IntfName, ConfigString + 5, sizeof(State->IntfName) - 1);
        State->IntfName[sizeof(State->IntfName) - 1] = 0;
        return CFE_PSP_SUCCESS;
    }
    else if (strncmp(ConfigString, "port=", 5) == 0)
    {
        State->LocalAddr.sin_port = htons(atoi(ConfigString + 5));
        return CFE_PSP_SUCCESS;
    }
    else if (strncmp(ConfigString, "IpAddr=", 7) == 0)
    {
        State->LocalAddr.sin_addr.s_addr = inet_addr(ConfigString + 7);
        return CFE_PSP_SUCCESS;
    }

    return CFE_PSP_ERROR;
}

/*
 * stcpsock_intf_Init - module initialization
 */
void stcpsock_intf_Init(uint32 PspModuleId)
{
    uint32 i;

    memset(&StcpSock_Global, 0, sizeof(StcpSock_Global));
    OS_printf("CFE_PSP: Initializing stcpsock_intf (STCP) interface\n");
    StcpSock_Global.MyPspModuleId = PspModuleId;

    for (i = 0; i < STCPSOCK_MAX_INTERFACE_DEVS; ++i)
    {
        StcpSock_Global.State[i].DeviceFd  = -1;
        StcpSock_Global.State[i].ListenFd  = -1;
    }
}

/*
 * stcpsock_intf_ReadPacket - read one STCP-framed bundle from the TCP stream
 *
 * STCP framing: [4-byte BE u32 length] [bundle bytes]
 * A zero-length frame is a keepalive and is skipped.
 *
 * Uses a state machine to handle partial reads across calls:
 *   1. Accept connection if needed (non-blocking)
 *   2. Read/accumulate 4-byte length prefix
 *   3. Read/accumulate payload bytes into caller's buffer
 *   4. Return SUCCESS when a complete bundle is assembled
 */
static int32 stcpsock_intf_ReadPacket(stcpsock_intf_State_t *State,
                                      CFE_PSP_IODriver_ReadPacketBuffer_t *Dest)
{
    struct pollfd pfd;
    int rc;

    /* Accept a connection if we don't have one */
    if (State->DeviceFd < 0)
    {
        if (State->ListenFd < 0)
            return CFE_PSP_ERROR;

        int fd = accept(State->ListenFd, NULL, NULL);
        if (fd < 0)
        {
            if (errno == EAGAIN || errno == EWOULDBLOCK)
                return CFE_PSP_ERROR_TIMEOUT;
            return CFE_PSP_ERROR;
        }

        /* Set TCP_NODELAY for low latency */
        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

        /* Set a short recv timeout so we don't block the CLA task */
        struct timeval tv;
        tv.tv_sec  = 0;
        tv.tv_usec = STCPSOCK_RECV_TIMEOUT_MS * 1000;
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));

        State->DeviceFd = fd;
        State->LenBytesRead = 0;
        State->PendingLen = 0;
        State->PayloadBytesRead = 0;

        OS_printf("CFE_PSP: STCP accepted connection\n");
    }

    /* Poll for readability */
    pfd.fd = State->DeviceFd;
    pfd.events = POLLIN;
    rc = poll(&pfd, 1, STCPSOCK_RECV_TIMEOUT_MS);
    if (rc < 0)
    {
        perror("stcpsock_intf_ReadPacket: poll()");
        return CFE_PSP_ERROR;
    }
    if (rc == 0)
        return CFE_PSP_ERROR_TIMEOUT;

    /* Read/accumulate length prefix */
    while (State->LenBytesRead < 4)
    {
        ssize_t n = recv(State->DeviceFd,
                         State->LenBuf + State->LenBytesRead,
                         4 - State->LenBytesRead, 0);
        if (n <= 0)
        {
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
                return CFE_PSP_ERROR_TIMEOUT;
            /* Connection closed or error — reset and wait for new connection */
            stcpsock_resetConnection(State);
            return CFE_PSP_ERROR_TIMEOUT;
        }
        State->LenBytesRead += n;
    }

    /* Parse length prefix (first time after header complete) */
    if (State->PendingLen == 0)
    {
        State->PendingLen = ntohl(*(uint32_t *)State->LenBuf);

        /* Zero-length = keepalive, skip and reset for next frame */
        if (State->PendingLen == 0)
        {
            State->LenBytesRead = 0;
            return CFE_PSP_ERROR_TIMEOUT;
        }

        if (State->PendingLen > Dest->BufferSize)
        {
            OS_printf("CFE_PSP: STCP bundle too large: %u > %lu\n",
                      State->PendingLen, (unsigned long)Dest->BufferSize);
            stcpsock_resetConnection(State);
            return CFE_PSP_ERROR;
        }

        State->PayloadBytesRead = 0;
    }

    /* Read/accumulate payload into caller's buffer */
    while (State->PayloadBytesRead < State->PendingLen)
    {
        ssize_t n = recv(State->DeviceFd,
                         (uint8 *)Dest->BufferMem + State->PayloadBytesRead,
                         State->PendingLen - State->PayloadBytesRead, 0);
        if (n <= 0)
        {
            if (n < 0 && (errno == EAGAIN || errno == EWOULDBLOCK))
                return CFE_PSP_ERROR_TIMEOUT;
            stcpsock_resetConnection(State);
            return CFE_PSP_ERROR_TIMEOUT;
        }
        State->PayloadBytesRead += n;
    }

    /* Complete bundle received — log is checked by test script */
    OS_printf("CFE_PSP: STCP received complete bundle: %u bytes\n", (unsigned)State->PendingLen);
    Dest->BufferSize = State->PendingLen;

    /* Reset for next frame */
    State->LenBytesRead = 0;
    State->PendingLen = 0;
    State->PayloadBytesRead = 0;

    return CFE_PSP_SUCCESS;
}

/*
 * stcpsock_intf_WritePacket - write one STCP-framed bundle to the TCP stream
 *
 * Lazily connects on first write. Writes 4-byte BE u32 length prefix
 * followed by the bundle payload.
 */
static int32 stcpsock_intf_WritePacket(stcpsock_intf_State_t *State,
                                       CFE_PSP_IODriver_WritePacketBuffer_t *Source)
{
    /* Lazy connect if not yet connected */
    if (State->DeviceFd < 0)
    {
        int fd = socket(AF_INET, SOCK_STREAM, 0);
        if (fd < 0)
        {
            perror("stcpsock_intf_WritePacket: socket()");
            return CFE_PSP_ERROR;
        }

        int one = 1;
        setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &one, sizeof(one));

        OS_printf("CFE_PSP: STCP connecting to %s:%d\n",
                  inet_ntoa(State->LocalAddr.sin_addr), ntohs(State->LocalAddr.sin_port));

        if (connect(fd, (struct sockaddr *)&State->LocalAddr,
                    sizeof(State->LocalAddr)) < 0)
        {
            perror("stcpsock_intf_WritePacket: connect()");
            close(fd);
            return CFE_PSP_ERROR;
        }

        State->DeviceFd = fd;
        OS_printf("CFE_PSP: STCP output connected\n");
    }

    /* Write 4-byte big-endian length prefix */
    uint32_t netLen = htonl((uint32_t)Source->OutputSize);
    if (stcpsock_writeFull(State->DeviceFd, &netLen, 4) != 4)
    {
        OS_printf("CFE_PSP: STCP write error (length prefix)\n");
        stcpsock_resetConnection(State);
        return CFE_PSP_ERROR;
    }

    /* Write bundle payload */
    if (stcpsock_writeFull(State->DeviceFd, Source->BufferMem, Source->OutputSize)
        != (ssize_t)Source->OutputSize)
    {
        OS_printf("CFE_PSP: STCP write error (payload)\n");
        stcpsock_resetConnection(State);
        return CFE_PSP_ERROR;
    }

    return CFE_PSP_SUCCESS;
}


/*
 * stcpsockDevCmd - IODriver command dispatcher
 * (same structure as udpsock_intf)
 */
static int32 stcpsockDevCmd(uint32 CommandCode, uint16 Instance, uint16 SubChannel,
                            CFE_PSP_IODriver_Arg_t Arg)
{
    int32 ReturnCode = CFE_PSP_ERROR_NOT_IMPLEMENTED;
    stcpsock_intf_State_t *InstPtr;

    if (Instance > 0 && Instance <= STCPSOCK_MAX_INTERFACE_DEVS)
    {
        InstPtr = &StcpSock_Global.State[Instance - 1];

        switch(CommandCode)
        {
            case CFE_PSP_IODriver_NOOP:
            case CFE_PSP_IODriver_PACKET_IO_NOOP:
                ReturnCode = CFE_PSP_SUCCESS;
                break;

            case CFE_PSP_IODriver_SET_RUNNING:
                if (Arg.U32)
                {
                    if (InstPtr->DeviceFd >= 0 || InstPtr->ListenFd >= 0)
                        ReturnCode = CFE_PSP_SUCCESS;
                    else
                        ReturnCode = stcpsock_intf_OpenPort(InstPtr, Instance);
                }
                else
                {
                    stcpsock_resetConnection(InstPtr);
                    if (InstPtr->ListenFd >= 0)
                    {
                        close(InstPtr->ListenFd);
                        InstPtr->ListenFd = -1;
                    }
                    ReturnCode = CFE_PSP_SUCCESS;
                }
                break;

            case CFE_PSP_IODriver_GET_RUNNING:
                ReturnCode = (InstPtr->DeviceFd >= 0 || InstPtr->ListenFd >= 0) ? 1 : 0;
                break;

            case CFE_PSP_IODriver_SET_CONFIGURATION:
                ReturnCode = stcpsock_intf_Configure(InstPtr, Instance,
                                                     (const char *)Arg.ConstVptr);
                break;

            case CFE_PSP_IODriver_GET_CONFIGURATION:
                break;

            case CFE_PSP_IODriver_LOOKUP_SUBCHANNEL:
                ReturnCode = 0;
                break;

            case CFE_PSP_IODriver_SET_DIRECTION:
            {
                CFE_PSP_IODriver_Direction_t Dir = (CFE_PSP_IODriver_Direction_t)Arg.U32;
                if (Dir == CFE_PSP_IODriver_Direction_INPUT_ONLY ||
                    Dir == CFE_PSP_IODriver_Direction_OUTPUT_ONLY)
                {
                    InstPtr->ConfigDir = Dir;
                    ReturnCode = CFE_PSP_SUCCESS;
                }
                break;
            }

            case CFE_PSP_IODriver_QUERY_DIRECTION:
            {
                CFE_PSP_IODriver_Direction_t *DirPtr = (CFE_PSP_IODriver_Direction_t *)Arg.Vptr;
                if (DirPtr != NULL)
                {
                    *DirPtr = InstPtr->ConfigDir;
                    ReturnCode = CFE_PSP_SUCCESS;
                }
                break;
            }

            case CFE_PSP_IODriver_PACKET_IO_READ:
                if (InstPtr->ConfigDir == CFE_PSP_IODriver_Direction_INPUT_ONLY)
                    ReturnCode = stcpsock_intf_ReadPacket(InstPtr,
                                    (CFE_PSP_IODriver_ReadPacketBuffer_t *)Arg.Vptr);
                break;

            case CFE_PSP_IODriver_PACKET_IO_WRITE:
                if (InstPtr->ConfigDir == CFE_PSP_IODriver_Direction_OUTPUT_ONLY)
                    ReturnCode = stcpsock_intf_WritePacket(InstPtr,
                                    (CFE_PSP_IODriver_WritePacketBuffer_t *)Arg.Vptr);
                break;

            default:
                break;
        }
    }

    return ReturnCode;
}
