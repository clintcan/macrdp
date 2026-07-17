// USB redirection (MS-RDPEUSB) macOS presenting side: drive a user-space virtual
// USB host controller (PUBLIC IOUSBHost.framework `IOUSBHostControllerInterface`,
// entitlement com.apple.developer.usb.host-controller-interface) so a device
// enumerates in `ioreg -p IOUSB`.
//
// Phase 1b proved the controller instantiates in a signed+provisioned build.
// Phase 2 drives the full UserHCI command protocol — controller / port / device /
// endpoint state machines + EP0 GET_DESCRIPTOR handling.
//
// Phase 3.1b(2b) makes the EP0 data path ASYNCHRONOUS + client-sourced. A
// control-IN data stage no longer completes inline: it is raised to Rust via a
// C callback (the kernel wants these descriptor bytes), left OUTSTANDING in the
// transfer ring, and completed later — out-of-band — when the answer arrives
// (from the RDP client over URBDRC, or, for the standalone `--usb-spike` path,
// from a built-in hardcoded callback). The completion hops back onto the
// interface's serial queue via `dispatch_async` (`self.interface.queue`), so the
// `memcpy` + `enqueueTransferCompletionForMessage:` + ring re-walk all run on the
// one thread that owns the ring — the queue<->tokio boundary. SETUP / STATUS /
// host-to-device stages still complete synchronously on the queue.
//
// This stays the maintenance boundary for the Apple USB SPI: every IOUSBHost
// touch lives here (mirrors src/virtual_display/private_api.rs). Built only on
// macOS (build.rs compiles it + links IOUSBHost). The C ABI at the bottom is the
// bidirectional FFI src/usb_redirect/mod.rs drives.

#import <Foundation/Foundation.h>
#import <IOUSBHost/IOUSBHostControllerInterface.h>
#import <IOUSBHost/IOUSBHostCIControllerStateMachine.h>
#import <IOUSBHost/IOUSBHostCIPortStateMachine.h>
#import <IOUSBHost/IOUSBHostCIDeviceStateMachine.h>
#import <IOUSBHost/IOUSBHostCIEndpointStateMachine.h>
#import <mach/mach_time.h>

// ---- bidirectional C ABI (see the bottom of the file for the functions) ------
// ObjC -> Rust: the kernel wants a control-IN data stage serviced. The Rust side
// must eventually call macrdp_usb_complete_transfer(handle, token, ...). The
// callback MUST NOT retain `setup8` (it points at stack memory) — copy it and
// return immediately (non-blocking). `max_len` is the data-stage buffer capacity.
typedef void (*MacrdpUsbControlInFn)(void *ctx, uint64_t token, const uint8_t *setup8, uint32_t max_len);
// EP0 control-OUT (host->device) request raised to Rust for forwarding to the real
// device: setup8 = the 8-byte SETUP, out_data/out_len = any payload (empty for the
// no-data requests, e.g. mass-storage reset / Clear-Feature).
typedef void (*MacrdpUsbControlOutFn)(void *ctx, uint64_t token, const uint8_t *setup8,
                                      const uint8_t *out_data, uint32_t out_len);
// Bulk/interrupt transfer raised to Rust: is_in != 0 = device->host read of out_len
// bytes; else host->device write of the out_len bytes at out_data.
typedef void (*MacrdpUsbBulkFn)(void *ctx, uint64_t token, uint32_t endpoint_address, int is_in,
                                const uint8_t *out_data, uint32_t out_len);

// ---- helpers to pull a bit-field out of an IOUSBHostCIMessage word ----------
// The SDK phase macros expand to the low bit index; mask is (range) already
// shifted, so extract = (word & range) >> phase.
static inline uint32_t msg_type(const IOUSBHostCIMessage *m) {
    return (uint32_t)((m->control & IOUSBHostCIMessageControlType) >> IOUSBHostCIMessageControlTypePhase);
}
static inline BOOL msg_valid(const IOUSBHostCIMessage *m) {
    return (m->control & IOUSBHostCIMessageControlValid) != 0;
}
static inline BOOL msg_no_response(const IOUSBHostCIMessage *m) {
    return (m->control & IOUSBHostCIMessageControlNoResponse) != 0;
}

// Monotonic nanoseconds (for observing isoch service cadence). Cheap; the
// timebase is cached after the first call.
static inline uint64_t macrdp_now_ns(void) {
    static mach_timebase_info_data_t tb;
    if (tb.denom == 0) {
        mach_timebase_info(&tb);
    }
    return mach_absolute_time() * tb.numer / tb.denom;
}

// USB standard request / descriptor-type constants.
enum {
    kUsbDirDeviceToHost   = 0x80,
    kUsbReqTypeMask       = 0x60, // bmRequestType bits 6:5 (0=standard,1=class,2=vendor)
    kUsbReqTypeStandard   = 0x00,
    kUsbReqGetDescriptor  = 0x06,
    kUsbReqSetAddress     = 0x05,
    kUsbReqSetConfig      = 0x09,
    kUsbReqSetInterface   = 0x0b,
    kUsbDescDevice        = 0x01,
    kUsbDescConfiguration = 0x02,
    kUsbDescString        = 0x03,
    kUsbDescEndpoint      = 0x05,
    kUsbEpTypeMask        = 0x03, // bmAttributes bits 1:0 (0=ctrl,1=isoch,2=bulk,3=intr)
    kUsbEpTypeIsoch       = 0x01,
};

// Read an endpoint descriptor's transfer-type bits from the descriptor pointer a
// UserHCI EndpointCreate command hands us (data1). It may point at a single
// ENDPOINT descriptor or a synthetic container of descriptors, so scan bounded for
// the first ENDPOINT (0x05) and return its bmAttributes&0x03; -1 if none/invalid.
// Observe-only helper (isoch go/no-go) — bounded so a bad pointer can't run away.
static int macrdp_ep_transfer_type(uint64_t descAddr) {
    const uint8_t *p = (const uint8_t *)(uintptr_t)descAddr;
    if (p == NULL) {
        return -1;
    }
    for (int i = 0; i < 32; i++) {
        uint8_t bLength = p[0];
        uint8_t bType = p[1];
        if (bLength < 2) {
            return -1; // malformed / terminator
        }
        if (bType == kUsbDescEndpoint) {
            return bLength >= 4 ? (p[3] & kUsbEpTypeMask) : -1;
        }
        p += bLength; // next descriptor in the container
    }
    return -1;
}

static const NSUInteger kSyntheticDeviceAddress = 1;

// -----------------------------------------------------------------------------
// One outstanding (raised-but-not-yet-completed) forwarded transfer — control-IN,
// control-OUT, or bulk — keyed by a monotonic token. The raw `msg` pointer stays
// valid because the ring slot is pinned until we enqueue its completion (and the
// queue is serial, so no concurrent ring mutation).
@interface MacrdpPendingTransfer : NSObject
@property(nonatomic, assign) const IOUSBHostCIMessage *msg;
@property(nonatomic, strong) NSNumber *endpointKey;
// The endpoint object this transfer was raised on. A device reset destroys and
// RECREATES the endpoint at the same key (a new, Active object whose ring is
// fresh) while `msg` still points into the OLD, freed ring — so on completion we
// require endpoints[key] to still be THIS same object before touching `msg`. Held
// strong so a reused address can't alias a genuinely-different recreated endpoint.
@property(nonatomic, strong) IOUSBHostCIEndpointStateMachine *ep;
@end
@implementation MacrdpPendingTransfer
@end

// -----------------------------------------------------------------------------
// Bulk-IN read-ahead (streaming endpoints, e.g. a UVC webcam). macOS double/
// triple-buffers a streaming bulk-IN endpoint — it queues several concurrent
// reads on the ring so the device's pipe never runs dry. The UserHCI ring only
// exposes ONE TRB (the head) at a time (currentTransferMessage advances only on
// completion), so we cannot forward those queued TRBs individually. Instead we
// keep DEPTH concurrent bulk_transfer_in reads in flight to the CLIENT
// (decoupled from the ring), buffer their results in SEQUENCE order, and hand one
// buffered chunk to each ring TRB as it becomes the head. This restores the
// device-side URB depth (no underrun) WITHOUT the data loss the old
// forward-the-same-head-twice behaviour caused. Engaged lazily only when macOS
// demonstrates it wants concurrency (a re-doorbell of an already-outstanding
// bulk-IN head) AND the read is large (streaming, not an interrupt-status poll),
// so strictly request/response endpoints (mass storage) and small interrupt
// endpoints never enter this path and stay serial.
@interface MacrdpPrefetchRead : NSObject
@property(nonatomic, strong) NSNumber *endpointKey;
@property(nonatomic, assign) uint64_t seq;   // stream position, for in-order delivery
@end
@implementation MacrdpPrefetchRead
@end

@interface MacrdpBulkStream : NSObject
@property(nonatomic, assign) uint32_t readLen;          // per-read length (from the ring TRB)
@property(nonatomic, assign) NSInteger inFlight;        // client reads outstanding
@property(nonatomic, assign) uint64_t nextIssueSeq;     // next read's sequence
@property(nonatomic, assign) uint64_t nextDeliverSeq;   // next sequence to deliver (reorder cursor)
@property(nonatomic, strong) NSMutableArray<NSData *> *fifo;              // in-order buffers awaiting a ring TRB
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSData *> *reorder; // seq -> out-of-order arrival
@property(nonatomic, assign) const IOUSBHostCIMessage *pendingHead;      // ring TRB presented but not yet fed (or NULL)
@property(nonatomic, strong) IOUSBHostCIEndpointStateMachine *pendingEp; // identity guard for pendingHead
@property(nonatomic, assign) uint64_t lastProgressNs;                    // ns of the last in-order delivery (stall watchdog)
@end
@implementation MacrdpBulkStream
@end

// -----------------------------------------------------------------------------

@interface MacrdpUsbController : NSObject
@property(nonatomic, strong) IOUSBHostControllerInterface *interface;
// deviceAddress -> device state machine
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, IOUSBHostCIDeviceStateMachine *> *devices;
// (deviceAddress<<8 | endpointAddress) -> endpoint state machine
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, IOUSBHostCIEndpointStateMachine *> *endpoints;
// endpoint key -> pending 8-byte SETUP packet for the in-flight control transfer
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSData *> *pendingSetup;
// token -> outstanding forwarded transfer awaiting an out-of-band completion
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, MacrdpPendingTransfer *> *tokenMap;
// endpoint key -> the token of ITS one outstanding forwarded transfer. The ring
// walk raises at most one forwarded transfer per endpoint, so a NEW raise on an
// endpoint with an uncompleted pending means the kernel timed out / aborted and
// RE-ISSUED the slot — the old pending's msg pointer is retired ring memory, and
// a late completion through it corrupts the ring (observed: SIGBUS in the
// completion memcpy when a slow Get Max LUN answered right after the kernel's
// ~5 s EP0 timeout re-raised the transfer). Registration supersedes the old one.
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSNumber *> *pendingByEndpoint;
// endpoint key -> bulk-IN read-ahead stream state (streaming endpoints only)
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, MacrdpBulkStream *> *bulkStreams;
// M0 isoch observe-only: endpoint key -> TRB count / last-service ns (cadence).
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSNumber *> *isochCount;
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSNumber *> *isochLastNs;
// prefetch-read token -> its (endpoint, seq); routed AHEAD of tokenMap on completion
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, MacrdpPrefetchRead *> *prefetchReads;
// target concurrent client reads per streaming endpoint (MACRDP_USB_PREFETCH_DEPTH)
@property(nonatomic, assign) NSInteger prefetchDepth;
// Bulk-IN stall watchdog: a dispatch timer on the interface queue that recovers a
// read-ahead stream whose client completions have silently stopped (see
// -checkStreamStalls). streamStallNs == 0 disables it (MACRDP_USB_STREAM_STALL_MS=0).
@property(nonatomic, strong) dispatch_source_t streamWatchdog;
@property(nonatomic, assign) uint64_t streamStallNs;
@property(nonatomic, assign) uint64_t nextToken;
// ObjC -> Rust control-IN callback + its opaque context (set at create).
@property(nonatomic, assign) MacrdpUsbControlInFn controlInCb;
@property(nonatomic, assign) MacrdpUsbControlOutFn controlOutCb;
@property(nonatomic, assign) MacrdpUsbBulkFn bulkCb;
// Host->device data stashed from a control-OUT DATA stage, keyed by endpoint, to
// include when the STATUS stage forwards the request. Empty for no-data requests.
@property(nonatomic, strong) NSMutableDictionary<NSNumber *, NSData *> *pendingOutData;
@property(nonatomic, assign) void *cbCtx;
@property(nonatomic, assign) BOOL portConnected;
@end

@implementation MacrdpUsbController

- (instancetype)init {
    if ((self = [super init])) {
        _devices = [NSMutableDictionary dictionary];
        _endpoints = [NSMutableDictionary dictionary];
        _pendingSetup = [NSMutableDictionary dictionary];
        _pendingOutData = [NSMutableDictionary dictionary];
        _tokenMap = [NSMutableDictionary dictionary];
        _pendingByEndpoint = [NSMutableDictionary dictionary];
        _bulkStreams = [NSMutableDictionary dictionary];
        _isochCount = [NSMutableDictionary dictionary];
        _isochLastNs = [NSMutableDictionary dictionary];
        _prefetchReads = [NSMutableDictionary dictionary];
        const char *pd = getenv("MACRDP_USB_PREFETCH_DEPTH");
        _prefetchDepth = pd ? atoi(pd) : 4;
        if (_prefetchDepth < 1) _prefetchDepth = 1;
        if (_prefetchDepth > 16) _prefetchDepth = 16;
        const char *ss = getenv("MACRDP_USB_STREAM_STALL_MS");
        long ssMs = ss ? atol(ss) : 3000;   // 0 disables the bulk-IN stall watchdog
        if (ssMs < 0) ssMs = 0;
        _streamStallNs = (uint64_t)ssMs * 1000000ull;
    }
    return self;
}

// Bring the root port to "connected" once the controller is Active and the port
// is powered. Writing the SM's `connected` property auto-sends a PortEvent with
// ConnectChange, which makes the kernel issue a PortReset next.
- (void)maybeConnectPort {
    if (self.portConnected) {
        return;
    }
    NSError *e = nil;
    IOUSBHostCIControllerStateMachine *csm = self.interface.controllerStateMachine;
    if (csm.controllerState != IOUSBHostCIControllerStateActive) {
        return;
    }
    IOUSBHostCIPortStateMachine *port = [self.interface getPortStateMachineForPort:1 error:&e];
    if (port == nil) {
        NSLog(@"[usb2] getPortStateMachineForPort:1 failed: %@", e);
        return;
    }
    if (!port.powered) {
        return;
    }
    port.connected = YES;
    self.portConnected = YES;
    NSLog(@"[usb2] root port connect asserted — expecting PortReset from kernel");
}

- (void)handleControllerCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    IOUSBHostCIControllerStateMachine *csm = self.interface.controllerStateMachine;
    if (![csm inspectCommand:command error:&e]) {
        NSLog(@"[usb2] controller inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    if (type == IOUSBHostCIMessageTypeControllerFrameNumber) {
        // Provide a synthetic 1ms frame counter derived from mach time.
        static mach_timebase_info_data_t tb;
        if (tb.denom == 0) {
            mach_timebase_info(&tb);
        }
        uint64_t now = mach_absolute_time();
        uint64_t ns = now * tb.numer / tb.denom;
        uint64_t frame = ns / 1000000ull; // 1ms frames
        [csm respondToCommand:command status:IOUSBHostCIMessageStatusSuccess frame:frame timestamp:now error:&e];
    } else {
        [csm respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    }
    if (e) {
        NSLog(@"[usb2] controller respond type=0x%02x error: %@", type, e);
    }
    NSLog(@"[usb2] controller cmd type=0x%02x -> state=%ld", type, (long)csm.controllerState);
    [self maybeConnectPort];
}

- (void)handlePortCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    IOUSBHostCIPortStateMachine *port = [self.interface getPortStateMachineForCommand:command error:&e];
    if (port == nil) {
        NSLog(@"[usb2] getPortStateMachineForCommand type=0x%02x failed: %@", type, e);
        return;
    }
    if (![port inspectCommand:command error:&e]) {
        NSLog(@"[usb2] port inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    switch (type) {
        case IOUSBHostCIMessageTypePortPowerOn:
            port.powered = YES;
            break;
        case IOUSBHostCIMessageTypePortPowerOff:
            port.powered = NO;
            self.portConnected = NO;
            break;
        case IOUSBHostCIMessageTypePortReset:
            // Reset drives the link to U0 (operational) at our device speed.
            // Full-speed avoids the host asking for a device_qualifier.
            if (![port updateLinkState:IOUSBHostCILinkStateU0
                                 speed:IOUSBHostCIDeviceSpeedFull
                inhibitLinkStateChange:NO
                                 error:&e]) {
                NSLog(@"[usb2] updateLinkState on reset failed: %@", e);
            }
            break;
        default:
            break; // Resume/Suspend/Disable/Status: just ack.
    }
    [port respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (e) {
        NSLog(@"[usb2] port respond type=0x%02x error: %@", type, e);
    }
    NSLog(@"[usb2] port cmd type=0x%02x -> state=%ld powered=%d connected=%d",
          type, (long)port.portState, port.powered, port.connected);
    [self maybeConnectPort];
}

- (void)handleDeviceCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    if (type == IOUSBHostCIMessageTypeDeviceCreate) {
        IOUSBHostCIDeviceStateMachine *dev =
            [[IOUSBHostCIDeviceStateMachine alloc] initWithInterface:self.interface command:command error:&e];
        if (dev == nil) {
            NSLog(@"[usb2] DeviceCreate state machine init failed: %@", e);
            return;
        }
        [dev respondToCommand:command
                       status:IOUSBHostCIMessageStatusSuccess
                deviceAddress:kSyntheticDeviceAddress
                        error:&e];
        if (e) {
            NSLog(@"[usb2] DeviceCreate respond error: %@", e);
            return;
        }
        self.devices[@(kSyntheticDeviceAddress)] = dev;
        NSLog(@"[usb2] device created addr=%lu route=%lu",
              (unsigned long)dev.deviceAddress, (unsigned long)dev.completeRoute);
        return;
    }

    // Non-create device command: target existing device by address in data0.
    NSUInteger addr = (command->data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase;
    IOUSBHostCIDeviceStateMachine *dev = self.devices[@(addr)];
    if (dev == nil) {
        NSLog(@"[usb2] device cmd type=0x%02x for unknown addr=%lu", type, (unsigned long)addr);
        return;
    }
    if (![dev inspectCommand:command error:&e]) {
        NSLog(@"[usb2] device inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    [dev respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (type == IOUSBHostCIMessageTypeDeviceDestroy) {
        [self.devices removeObjectForKey:@(addr)];
    }
    NSLog(@"[usb2] device cmd type=0x%02x addr=%lu state=%ld",
          type, (unsigned long)addr, (long)dev.deviceState);
}

- (NSNumber *)endpointKeyForDevice:(NSUInteger)dev endpoint:(NSUInteger)ep {
    return @((dev << 8) | (ep & 0xff));
}

- (void)handleEndpointCommand:(const IOUSBHostCIMessage *)command type:(uint32_t)type {
    NSError *e = nil;
    NSUInteger addr = (command->data0 & IOUSBHostCICommandMessageData0DeviceAddress)
                    >> IOUSBHostCICommandMessageData0DeviceAddressPhase;
    NSUInteger epAddr = (command->data0 & IOUSBHostCICommandMessageData0EndpointAddress)
                      >> IOUSBHostCICommandMessageData0EndpointAddressPhase;

    if (type == IOUSBHostCIMessageTypeEndpointCreate) {
        IOUSBHostCIEndpointStateMachine *ep =
            [[IOUSBHostCIEndpointStateMachine alloc] initWithInterface:self.interface command:command error:&e];
        if (ep == nil) {
            NSLog(@"[usb2] EndpointCreate state machine init failed: %@", e);
            return;
        }
        [ep respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
        if (e) {
            NSLog(@"[usb2] EndpointCreate respond error: %@", e);
            return;
        }
        NSNumber *key = [self endpointKeyForDevice:ep.deviceAddress endpoint:ep.endpointAddress];
        self.endpoints[key] = ep;
        // M0 isoch observe-only: note whether this is an isochronous endpoint so a
        // later 0x3b TRB is expected (not "unexpected"). Detection only — no path change.
        uint64_t descAddr = (command->data1 & IOUSBHostCIEndpointCreateCommandData1Descriptor)
                          >> IOUSBHostCIEndpointCreateCommandData1DescriptorPhase;
        int epType = macrdp_ep_transfer_type(descAddr);
        NSLog(@"[usb2] endpoint created dev=%lu ep=0x%02lx state=%ld type=%d%@",
              (unsigned long)ep.deviceAddress, (unsigned long)ep.endpointAddress, (long)ep.endpointState,
              epType, epType == kUsbEpTypeIsoch ? @" (ISOCHRONOUS)" : @"");
        return;
    }

    NSNumber *key = [self endpointKeyForDevice:addr endpoint:epAddr];
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil) {
        NSLog(@"[usb2] endpoint cmd type=0x%02x for unknown dev=%lu ep=0x%02lx",
              type, (unsigned long)addr, (unsigned long)epAddr);
        return;
    }
    if (![ep inspectCommand:command error:&e]) {
        NSLog(@"[usb2] endpoint inspectCommand rejected type=0x%02x: %@", type, e);
        return;
    }
    [ep respondToCommand:command status:IOUSBHostCIMessageStatusSuccess error:&e];
    if (type == IOUSBHostCIMessageTypeEndpointDestroy) {
        [self.endpoints removeObjectForKey:key];
        [self.pendingSetup removeObjectForKey:key];
        [self.pendingOutData removeObjectForKey:key];
        // Invalidate the endpoint's outstanding forwarded transfer outright — its
        // msg points into the ring being destroyed (belt to the identity guard's
        // suspenders: the guard only helps if the endpoint is later recreated).
        NSNumber *stale = self.pendingByEndpoint[key];
        if (stale != nil) {
            [self.tokenMap removeObjectForKey:stale];
            [self.pendingByEndpoint removeObjectForKey:key];
        }
        // Drop any bulk-IN read-ahead state — its pendingHead msg + in-flight
        // reads point into the ring being destroyed.
        [self tearDownStream:key];
        // Drop M0 isoch observe counters for this endpoint.
        [self.isochCount removeObjectForKey:key];
        [self.isochLastNs removeObjectForKey:key];
    }
    NSLog(@"[usb2] endpoint cmd type=0x%02x dev=%lu ep=0x%02lx state=%ld",
          type, (unsigned long)addr, (unsigned long)epAddr, (long)ep.endpointState);
}

- (void)handleCommand:(const IOUSBHostCIMessage *)command {
    uint32_t type = msg_type(command);
    if (type >= IOUSBHostCIMessageTypeControllerPowerOn && type <= IOUSBHostCIMessageTypeControllerFrameNumber) {
        [self handleControllerCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypePortPowerOn && type <= IOUSBHostCIMessageTypePortStatus) {
        [self handlePortCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypeDeviceCreate && type <= IOUSBHostCIMessageTypeDeviceUpdate) {
        [self handleDeviceCommand:command type:type];
    } else if (type >= IOUSBHostCIMessageTypeEndpointCreate && type <= IOUSBHostCIMessageTypeEndpointSetNextTransfer) {
        [self handleEndpointCommand:command type:type];
    } else {
        NSLog(@"[usb2] unhandled command type=0x%02x control=0x%08x", type, command->control);
    }
}

// ---- control-transfer (EP0) handling ----------------------------------------

// Is the current transfer a control-IN data stage we forward to the client? ANY
// device-to-host data stage forwards — the bytes must come from the real device
// whether it's a standard GET_DESCRIPTOR, a class request (mass-storage Get Max
// LUN), or a vendor one; the Rust side routes by bmRequestType. Those go async;
// everything else (SETUP, STATUS, host-to-device data) completes synchronously on
// the queue in `processTransfer:`.
- (BOOL)transferIsForwardedControlIn:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    if (msg_type(msg) != IOUSBHostCIMessageTypeNormalTransfer) {
        return NO;
    }
    NSData *setupData = self.pendingSetup[key];
    if (setupData == nil || setupData.length < 8) {
        return NO;
    }
    const uint8_t *s = setupData.bytes;
    return (s[0] & kUsbDirDeviceToHost) != 0;
}

// Is `msg` (the current ring head) already the endpoint's outstanding forwarded
// transfer? Pointer-compare only — never dereferences `msg` or `p.msg`, and `msg`
// was validated by msg_valid() before this is called. Used by walkEndpoint: to
// skip re-raising a transfer that's already in flight (double-buffered reads)
// while still letting a genuine re-issue on a DIFFERENT slot through to supersede.
- (BOOL)ringHeadAlreadyOutstanding:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    NSNumber *tok = self.pendingByEndpoint[key];
    if (tok == nil) {
        return NO;
    }
    MacrdpPendingTransfer *p = self.tokenMap[tok];
    return (p != nil && p.msg == msg);
}

// Register a newly-raised forwarded transfer as THE outstanding one for its
// endpoint, superseding (invalidating) any prior uncompleted raise on the same
// endpoint — the kernel timed out / aborted and re-issued the slot, so the old
// pending's msg points into retired ring memory and its late completion must be
// dropped, not delivered (see the pendingByEndpoint comment).
- (uint64_t)registerPendingForMsg:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    NSNumber *stale = self.pendingByEndpoint[key];
    if (stale != nil && self.tokenMap[stale] != nil) {
        [self.tokenMap removeObjectForKey:stale];
        NSLog(@"[usb2] superseding stale outstanding transfer token=%llu (kernel re-raised the endpoint's slot)",
              stale.unsignedLongLongValue);
    }
    uint64_t token = ++self.nextToken;
    MacrdpPendingTransfer *p = [MacrdpPendingTransfer new];
    p.msg = msg;
    p.endpointKey = key;
    p.ep = self.endpoints[key];
    self.tokenMap[@(token)] = p;
    self.pendingByEndpoint[key] = @(token);
    return token;
}

// Raise a control-IN data stage to the Rust side and leave it outstanding. The
// completion arrives later via macrdp_usb_complete_transfer(token, ...).
- (void)raiseControlIn:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    NSData *setupData = self.pendingSetup[key];
    uint8_t setup[8] = {0};
    if (setupData && setupData.length >= 8) {
        memcpy(setup, setupData.bytes, 8);
    }
    NSUInteger bufLen = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                      >> IOUSBHostCINormalTransferData0LengthPhase;

    uint64_t token = [self registerPendingForMsg:msg endpointKey:key];

    uint16_t wValue = (uint16_t)setup[2] | ((uint16_t)setup[3] << 8);
    NSLog(@"[usb2] EP0 control-IN forwarded token=%llu wValue=0x%04x bufLen=%lu",
          token, wValue, (unsigned long)bufLen);

    if (self.controlInCb) {
        self.controlInCb(self.cbCtx, token, setup, (uint32_t)bufLen);
    } else {
        // No presenting side wired — we can't source the bytes, so stall.
        [self completeTransferToken:token bytes:NULL len:0 status:IOUSBHostCIMessageStatusStallError];
    }
}

// Is this the STATUS stage of an EP0 control-OUT we should FORWARD to the real
// device? A control transfer arrives as SETUP -> [DATA] -> STATUS; the no-data
// host->device requests we care about (mass-storage Bulk-Only Reset, Clear-Feature)
// have no DATA stage, so we forward on STATUS. We forward every host->device request
// EXCEPT the standard ones the host controller / our SelectConfiguration path already
// own (SET_ADDRESS / SET_CONFIGURATION / SET_INTERFACE) — those stay a local ACK.
- (BOOL)transferIsForwardedControlOut:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    if (msg_type(msg) != IOUSBHostCIMessageTypeStatusTransfer || !self.controlOutCb) {
        return NO;
    }
    NSData *setupData = self.pendingSetup[key];
    if (setupData == nil || setupData.length < 8) {
        return NO;
    }
    const uint8_t *s = setupData.bytes;
    if (s[0] & kUsbDirDeviceToHost) {
        return NO; // device->host: its STATUS is host-side, nothing to forward
    }
    if ((s[0] & kUsbReqTypeMask) == kUsbReqTypeStandard &&
        (s[1] == kUsbReqSetAddress || s[1] == kUsbReqSetConfig || s[1] == kUsbReqSetInterface)) {
        return NO; // owned by the host controller / SelectConfiguration
    }
    return YES;
}

// Raise a control-OUT to the Rust side (forward to the real device) and leave the
// STATUS stage outstanding; the completion arrives via macrdp_usb_complete_transfer.
// Any host->device DATA bytes were stashed on the DATA stage (pendingOutData).
- (void)raiseControlOut:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    NSData *setupData = self.pendingSetup[key];
    uint8_t setup[8] = {0};
    if (setupData && setupData.length >= 8) {
        memcpy(setup, setupData.bytes, 8);
    }
    NSData *outData = self.pendingOutData[key] ?: [NSData data];

    uint64_t token = [self registerPendingForMsg:msg endpointKey:key];

    // The setup + data are captured into the callback now; drop the per-key state so
    // the next control transfer on EP0 starts clean.
    [self.pendingSetup removeObjectForKey:key];
    [self.pendingOutData removeObjectForKey:key];

    NSLog(@"[usb2] EP0 control-OUT forwarded token=%llu bmReq=0x%02x bReq=0x%02x len=%lu",
          token, setup[0], setup[1], (unsigned long)outData.length);
    self.controlOutCb(self.cbCtx, token, setup, outData.bytes, (uint32_t)outData.length);
}

// Is this a bulk/interrupt transfer (a NormalTransfer on a non-control endpoint)?
// Those forward to the client — a device->host read (IN) or host->device write
// (OUT) — mirroring the async control-IN path.
- (BOOL)transferIsBulk:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    if (msg_type(msg) != IOUSBHostCIMessageTypeNormalTransfer) {
        return NO;
    }
    NSUInteger epAddr = [key unsignedIntegerValue] & 0xff;
    return (epAddr & 0x7f) != 0; // endpoint number != 0 => not the control endpoint
}

// Raise a bulk/interrupt transfer to the Rust side and leave it outstanding. The
// completion arrives later via macrdp_usb_complete_transfer(token, ...). For OUT
// the buffer bytes are snapshotted now (the endpoint is Active mid-walk) since the
// ring slot may be recycled before the client round-trip returns.
- (void)raiseBulk:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key {
    NSUInteger epAddr = [key unsignedIntegerValue] & 0xff;
    BOOL isIn = (epAddr & 0x80) != 0;
    NSUInteger bufLen = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                      >> IOUSBHostCINormalTransferData0LengthPhase;

    uint64_t token = [self registerPendingForMsg:msg endpointKey:key];

    if (!self.bulkCb) {
        [self completeTransferToken:token bytes:NULL len:0 status:IOUSBHostCIMessageStatusStallError];
        return;
    }
    if (isIn) {
        NSLog(@"[usb2] bulk IN forwarded token=%llu ep=0x%02lx len=%lu",
              token, (unsigned long)epAddr, (unsigned long)bufLen);
        self.bulkCb(self.cbCtx, token, (uint32_t)epAddr, 1, NULL, (uint32_t)bufLen);
    } else {
        const void *buf = (const void *)(uintptr_t)msg->data1;
        NSData *data = (buf != NULL && bufLen > 0) ? [NSData dataWithBytes:buf length:bufLen] : [NSData data];
        NSLog(@"[usb2] bulk OUT forwarded token=%llu ep=0x%02lx len=%lu",
              token, (unsigned long)epAddr, (unsigned long)bufLen);
        self.bulkCb(self.cbCtx, token, (uint32_t)epAddr, 0, data.bytes, (uint32_t)data.length);
    }
}

// Complete a previously-raised control-IN transfer, hopping onto the interface's
// serial queue so the memcpy + ring enqueue + re-walk run where the ring lives.
// Safe to call from any thread (tokio, or the built-in spike callback).
- (void)completeTransferToken:(uint64_t)token
                        bytes:(const uint8_t *)bytes
                          len:(uint32_t)len
                       status:(IOUSBHostCIMessageStatus)status {
    // Copy the answer now — the caller's buffer may be transient.
    NSData *data = (bytes != NULL && len > 0) ? [NSData dataWithBytes:bytes length:len] : nil;
    dispatch_queue_t queue = self.interface.queue;
    if (queue == nil) {
        NSLog(@"[usb2] completion token=%llu: no interface queue (torn down?)", token);
        return;
    }
    dispatch_async(queue, ^{
        // Read-ahead prefetch completion? Route it ahead of the tied tokenMap —
        // these reads are not bound to a specific ring TRB; their bytes are
        // buffered in sequence order and fed to ring TRBs as they present.
        MacrdpPrefetchRead *pr = self.prefetchReads[@(token)];
        if (pr != nil) {
            [self.prefetchReads removeObjectForKey:@(token)];
            [self handlePrefetchCompletion:pr data:data status:status];
            return;
        }
        MacrdpPendingTransfer *p = self.tokenMap[@(token)];
        if (p == nil) {
            NSLog(@"[usb2] completion for unknown/expired token=%llu", token);
            return;
        }
        [self.tokenMap removeObjectForKey:@(token)];
        // This token is the endpoint's outstanding transfer (a superseded one was
        // already evicted from tokenMap); clear the slot so the next raise starts clean.
        NSNumber *cur = self.pendingByEndpoint[p.endpointKey];
        if (cur != nil && cur.unsignedLongLongValue == token) {
            [self.pendingByEndpoint removeObjectForKey:p.endpointKey];
        }
        IOUSBHostCIEndpointStateMachine *ep = self.endpoints[p.endpointKey];
        if (ep == nil) {
            NSLog(@"[usb2] completion token=%llu: endpoint gone", token);
            return;
        }
        // A device reset destroys+recreates the endpoint at the same key: the new
        // object is Active with a fresh ring, but `p.msg` still points into the OLD,
        // freed ring. The Active check below can't catch that (the NEW endpoint is
        // Active), so require the endpoint to be the SAME object this transfer was
        // raised on — else drop it (the reset re-drives fresh transfers). This is
        // the actual fix for the reset-during-bulk SIGSEGV; the Active check handles
        // the same-object-but-paused case.
        if (ep != p.ep) {
            NSLog(@"[usb2] completion token=%llu: endpoint replaced by a reset — dropping stale transfer", token);
            return;
        }
        // Per IOUSBHost: only an Active endpoint may inspect transfer structures,
        // read/modify IO buffers, or generate completions. If the device reset or
        // was suspended between the client round-trip and now, the endpoint is
        // Paused/Halted/Destroyed and `p.msg` points into a recycled ring slot —
        // touching msg->data1 there is a use-after-free (observed: SIGSEGV in the
        // memcpy below when a drive reset during interface-claim re-enumeration).
        // Drop the stale completion; the reset re-drives fresh transfers.
        if (ep.endpointState != IOUSBHostCIEndpointStateActive) {
            NSLog(@"[usb2] completion token=%llu: endpoint not Active (state=%ld) — dropping stale transfer",
                  token, (long)ep.endpointState);
            return;
        }
        const IOUSBHostCIMessage *msg = p.msg;
        NSUInteger moved = 0;
        if (status == IOUSBHostCIMessageStatusSuccess) {
            NSUInteger bufLen = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                              >> IOUSBHostCINormalTransferData0LengthPhase;
            if (data != nil) {
                // IN (control-IN or bulk-IN): copy the client's bytes into the buffer.
                void *buf = (void *)(uintptr_t)msg->data1;
                if (buf != NULL) {
                    NSUInteger n = data.length < bufLen ? data.length : bufLen;
                    memcpy(buf, data.bytes, n);
                    moved = n;
                }
            } else {
                // OUT (or a zero-length IN): no copy; `len` is the bytes accepted.
                moved = (NSUInteger)len < bufLen ? (NSUInteger)len : bufLen;
            }
        }
        NSError *e = nil;
        if (![ep enqueueTransferCompletionForMessage:msg status:status transferLength:moved error:&e]) {
            NSLog(@"[usb2] async enqueueTransferCompletion token=%llu failed: %@", token, e);
            return;
        }
        NSLog(@"[usb2] transfer completed token=%llu moved=%lu status=%ld",
              token, (unsigned long)moved, (long)status);
        // Drain the STATUS stage + any further transfers now that the ring advanced.
        [self walkEndpoint:p.endpointKey];
    });
}

// Synchronously service SETUP / STATUS / host-to-device / non-forwarded stages,
// returning the completion status + bytes moved. Control-IN data stages that need
// client bytes are handled out-of-band by raiseControlIn: and never reach here.
- (IOUSBHostCIMessageStatus)processTransfer:(const IOUSBHostCIMessage *)msg
                                endpointKey:(NSNumber *)key
                             transferLength:(NSUInteger *)outLen {
    *outLen = 0;
    uint32_t type = msg_type(msg);
    switch (type) {
        case IOUSBHostCIMessageTypeSetupTransfer: {
            // data1 packs the 8-byte setup packet. Stash it for the data stage.
            uint64_t d1 = msg->data1;
            uint8_t setup[8];
            setup[0] = (d1 >> IOUSBHostCISetupTransferData1bmRequestTypePhase) & 0xff;
            setup[1] = (d1 >> IOUSBHostCISetupTransferData1bRequestPhase) & 0xff;
            uint16_t wValue = (d1 >> IOUSBHostCISetupTransferData1wValuePhase) & 0xffff;
            uint16_t wIndex = (d1 >> IOUSBHostCISetupTransferData1wIndexPhase) & 0xffff;
            uint16_t wLength = (d1 >> IOUSBHostCISetupTransferData1wLengthPhase) & 0xffff;
            setup[2] = wValue & 0xff;       setup[3] = wValue >> 8;
            setup[4] = wIndex & 0xff;       setup[5] = wIndex >> 8;
            setup[6] = wLength & 0xff;      setup[7] = wLength >> 8;
            self.pendingSetup[key] = [NSData dataWithBytes:setup length:8];
            NSLog(@"[usb2] EP0 SETUP bmReq=0x%02x bReq=0x%02x wValue=0x%04x wIndex=0x%04x wLength=%u",
                  setup[0], setup[1], wValue, wIndex, wLength);
            return IOUSBHostCIMessageStatusSuccess;
        }
        case IOUSBHostCIMessageTypeNormalTransfer: {
            // A data stage that reached the sync path is host->device (every
            // device-to-host data stage forwards async). Stash the bytes so the
            // STATUS stage can forward them with the request, and report the full
            // length as accepted — completing an OUT data stage with 0 would tell
            // the kernel the device took none of it.
            NSData *setupData = self.pendingSetup[key];
            if (setupData && setupData.length >= 8) {
                const uint8_t *s = setupData.bytes;
                NSUInteger len = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                               >> IOUSBHostCINormalTransferData0LengthPhase;
                const void *buf = (const void *)(uintptr_t)msg->data1;
                if (!(s[0] & kUsbDirDeviceToHost) && buf != NULL && len > 0) {
                    self.pendingOutData[key] = [NSData dataWithBytes:buf length:len];
                    *outLen = len;
                }
            }
            return IOUSBHostCIMessageStatusSuccess;
        }
        case IOUSBHostCIMessageTypeStatusTransfer:
            // STATUS of a locally-ACKed control transfer (a forwarded control-OUT is
            // intercepted before this in walkEndpoint). Clear the per-key state.
            [self.pendingSetup removeObjectForKey:key];
            [self.pendingOutData removeObjectForKey:key];
            return IOUSBHostCIMessageStatusSuccess;
        default:
            NSLog(@"[usb2] unexpected transfer type=0x%02x on EP0", type);
            return IOUSBHostCIMessageStatusSuccess;
    }
}

// ---- bulk-IN read-ahead engine (streaming endpoints) ------------------------
// Below this line: only reads with length >= this engage read-ahead — large
// enough to be a video/streaming bulk endpoint, not a 16-byte interrupt poll.
static const uint32_t kStreamMinReadLen = 512;

// Keep DEPTH chunks in flight-or-buffered by issuing more speculative client
// reads. Bounded by (inFlight + fifo) so a stalled consumer can't over-read the
// device past DEPTH; refills as the FIFO drains into ring TRBs.
- (void)topUpPrefetch:(NSNumber *)key {
    MacrdpBulkStream *s = self.bulkStreams[key];
    if (s == nil || !self.bulkCb) {
        return;
    }
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil || ep.endpointState != IOUSBHostCIEndpointStateActive) {
        return;
    }
    NSUInteger epAddr = [key unsignedIntegerValue] & 0xff;
    while (s.inFlight + (NSInteger)s.fifo.count < self.prefetchDepth) {
        uint64_t token = ++self.nextToken;
        MacrdpPrefetchRead *pr = [MacrdpPrefetchRead new];
        pr.endpointKey = key;
        pr.seq = s.nextIssueSeq++;
        self.prefetchReads[@(token)] = pr;
        s.inFlight++;
        self.bulkCb(self.cbCtx, token, (uint32_t)epAddr, 1, NULL, s.readLen);
    }
}

// Move any consecutive-sequence buffers out of the reorder map into the FIFO, so
// the FIFO is always in stream order regardless of client completion order.
- (void)drainReorder:(MacrdpBulkStream *)s {
    NSData *next;
    BOOL advanced = NO;
    while ((next = s.reorder[@(s.nextDeliverSeq)]) != nil) {
        [s.reorder removeObjectForKey:@(s.nextDeliverSeq)];
        [s.fifo addObject:next];
        s.nextDeliverSeq++;
        advanced = YES;
    }
    // Forward progress for the stall watchdog (-checkStreamStalls): the client is
    // still completing reads in order. If completions go silent this stops
    // updating and the watchdog recovers the stream.
    if (advanced) {
        s.lastProgressNs = macrdp_now_ns();
    }
}

// Deliver `data` into ring TRB `msg`, guarded exactly like the async completion
// path (the endpoint must still be the SAME Active object, or `msg` is retired
// ring memory). Returns YES if the completion was enqueued.
- (BOOL)deliverStreamData:(NSData *)data toMsg:(const IOUSBHostCIMessage *)msg
                      key:(NSNumber *)key expectedEp:(IOUSBHostCIEndpointStateMachine *)expectedEp {
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil || ep != expectedEp || ep.endpointState != IOUSBHostCIEndpointStateActive) {
        return NO;
    }
    NSUInteger bufLen = (msg->data0 & IOUSBHostCINormalTransferData0Length)
                        >> IOUSBHostCINormalTransferData0LengthPhase;
    NSUInteger moved = 0;
    if (data.length > 0) {
        void *buf = (void *)(uintptr_t)msg->data1;
        if (buf != NULL) {
            moved = data.length < bufLen ? data.length : bufLen;
            memcpy(buf, data.bytes, moved);
        }
    }
    NSError *e = nil;
    if (![ep enqueueTransferCompletionForMessage:msg status:IOUSBHostCIMessageStatusSuccess
                                  transferLength:moved error:&e]) {
        NSLog(@"[usb2] stream enqueueTransferCompletion failed: %@", e);
        return NO;
    }
    return YES;
}

// Engage read-ahead on a bulk-IN endpoint whose head macOS just re-queued (it
// wants URB depth). Reclassify the in-flight tied read as prefetch seq 0, mark
// the head as awaiting its buffered chunk, and fill the pipe to DEPTH.
- (void)engageStream:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key
                  ep:(IOUSBHostCIEndpointStateMachine *)ep {
    uint32_t readLen = (uint32_t)((msg->data0 & IOUSBHostCINormalTransferData0Length)
                                  >> IOUSBHostCINormalTransferData0LengthPhase);
    if (self.prefetchDepth <= 1 || readLen < kStreamMinReadLen) {
        // Escape hatch / interrupt-status endpoint: stay serial, just skip the dup.
        NSLog(@"[usb2] ring head already outstanding — skipping re-raise (not streaming)");
        return;
    }
    MacrdpBulkStream *s = [MacrdpBulkStream new];
    s.readLen = readLen;
    s.fifo = [NSMutableArray array];
    s.reorder = [NSMutableDictionary dictionary];
    s.pendingHead = msg;
    s.pendingEp = ep;
    s.lastProgressNs = macrdp_now_ns();   // arm the stall watchdog from creation
    self.bulkStreams[key] = s;
    // Reclassify the outstanding tied read as prefetch seq 0 so its bytes feed the
    // read-ahead FIFO (and complete the head) instead of the tied completion path.
    NSNumber *tied = self.pendingByEndpoint[key];
    if (tied != nil && self.tokenMap[tied] != nil) {
        [self.tokenMap removeObjectForKey:tied];
        [self.pendingByEndpoint removeObjectForKey:key];
        MacrdpPrefetchRead *pr = [MacrdpPrefetchRead new];
        pr.endpointKey = key;
        pr.seq = s.nextIssueSeq++;   // seq 0
        self.prefetchReads[tied] = pr;
        s.inFlight++;
    }
    NSLog(@"[usb2] bulk-IN read-ahead engaged ep=0x%02lx depth=%ld readLen=%u",
          (unsigned long)([key unsignedIntegerValue] & 0xff), (long)self.prefetchDepth, readLen);
    [self topUpPrefetch:key];
}

// Serve a bulk-IN ring head while in read-ahead mode. Returns YES if it fed the
// head (ring advanced — keep walking), NO if it's now awaiting data (pendingHead)
// or the endpoint went stale.
- (BOOL)serveStreamHead:(const IOUSBHostCIMessage *)msg endpointKey:(NSNumber *)key
                     ep:(IOUSBHostCIEndpointStateMachine *)ep {
    MacrdpBulkStream *s = self.bulkStreams[key];
    [self topUpPrefetch:key];
    if (s.pendingHead == msg) {
        return NO;                       // already awaiting this exact head
    }
    if (s.fifo.count > 0) {
        NSData *data = s.fifo.firstObject;
        if ([self deliverStreamData:data toMsg:msg key:key expectedEp:ep]) {
            [s.fifo removeObjectAtIndex:0];
            return YES;                  // ring advanced
        }
        return NO;                       // stale endpoint — stop
    }
    s.pendingHead = msg;
    s.pendingEp = ep;
    return NO;
}

// A prefetch read returned. Buffer it in sequence order, feed a waiting head if we
// now have the next in-order chunk, and keep the device fed.
- (void)handlePrefetchCompletion:(MacrdpPrefetchRead *)pr data:(NSData *)data
                          status:(IOUSBHostCIMessageStatus)status {
    NSNumber *key = pr.endpointKey;
    MacrdpBulkStream *s = self.bulkStreams[key];
    if (s == nil) {
        return;   // stream torn down (endpoint destroyed) — drop
    }
    s.inFlight--;
    // Success with bytes → buffer them; a stall/zero read → an empty chunk so the
    // ring keeps advancing (macOS handles short/zero bulk reads) rather than hangs.
    NSData *buf = (status == IOUSBHostCIMessageStatusSuccess && data != nil) ? data : [NSData data];
    s.reorder[@(pr.seq)] = buf;
    [self drainReorder:s];
    if (s.pendingHead != NULL && s.fifo.count > 0) {
        NSData *chunk = s.fifo.firstObject;
        const IOUSBHostCIMessage *head = s.pendingHead;
        IOUSBHostCIEndpointStateMachine *headEp = s.pendingEp;
        s.pendingHead = NULL;
        s.pendingEp = nil;
        if ([self deliverStreamData:chunk toMsg:head key:key expectedEp:headEp]) {
            [s.fifo removeObjectAtIndex:0];
            [self walkEndpoint:key];   // serve subsequent TRBs from the FIFO + top up
            return;
        }
        // stale endpoint — the head was dropped; fall through to keep the pipe fed
    }
    [self topUpPrefetch:key];
}

// Drop a stream's state (endpoint destroyed). In-flight prefetch reads are
// orphaned — their late completions find no stream and are dropped.
- (void)tearDownStream:(NSNumber *)key {
    if (self.bulkStreams[key] == nil) {
        return;
    }
    [self.bulkStreams removeObjectForKey:key];
    NSMutableArray<NSNumber *> *orphans = [NSMutableArray array];
    for (NSNumber *tok in self.prefetchReads) {
        if ([self.prefetchReads[tok].endpointKey isEqual:key]) {
            [orphans addObject:tok];
        }
    }
    [self.prefetchReads removeObjectsForKeys:orphans];
}

// Bulk-IN stall watchdog. The read-ahead engine delivers strictly in sequence
// order, and an errored/short completion still fills its seq slot (empty chunk)
// so the reorder cursor advances — but a completion that NEVER comes back is a
// permanent gap: -drainReorder can't advance past the missing seq, so a ring head
// macOS presented sits unfed forever and the video freezes while the rest of the
// session (a separate channel) keeps working. This happens when the client's
// camera stalls on the host (USB autosuspend, a uvcvideo timeout, bandwidth
// starvation) so its pending bulk-IN URBs hang and never complete. There is no
// timeout on a missing seq, so without this the freeze is permanent until the
// device is re-attached. This timer detects the wedge and forces macOS to
// re-COMMIT, so a transient host-side stall becomes a self-recovering hiccup.
- (void)startStreamWatchdog {
    if (self.streamStallNs == 0) {
        return;   // disabled (MACRDP_USB_STREAM_STALL_MS=0)
    }
    dispatch_queue_t q = self.interface.queue;
    if (q == nil) {
        return;
    }
    __weak MacrdpUsbController *weakSelf = self;
    dispatch_source_t t = dispatch_source_create(DISPATCH_SOURCE_TYPE_TIMER, 0, 0, q);
    // ~1 s cadence (0.5 s leeway) — the threshold, not the tick, sets responsiveness.
    dispatch_source_set_timer(t, dispatch_time(DISPATCH_TIME_NOW, NSEC_PER_SEC),
                              NSEC_PER_SEC, NSEC_PER_SEC / 2);
    dispatch_source_set_event_handler(t, ^{
        [weakSelf checkStreamStalls];
    });
    self.streamWatchdog = t;
    dispatch_resume(t);
}

// Runs on the interface's serial queue (the timer targets it), so it can touch
// stream state directly — same serialization as the completion path. A stream is
// wedged when macOS is waiting on a ring head (pendingHead != NULL) but no
// in-order data has been delivered for >= the threshold. A quiescent stream
// (macOS not currently asking) has pendingHead == NULL and is left alone. Even a
// false positive is self-correcting: the recovery just forces a re-COMMIT, which
// re-establishes a healthy stream.
- (void)checkStreamStalls {
    if (self.bulkStreams.count == 0) {
        return;
    }
    uint64_t now = macrdp_now_ns();
    NSMutableArray<NSNumber *> *wedged = nil;
    for (NSNumber *key in self.bulkStreams) {
        MacrdpBulkStream *s = self.bulkStreams[key];
        if (s.pendingHead != NULL && (now - s.lastProgressNs) >= self.streamStallNs) {
            if (wedged == nil) {
                wedged = [NSMutableArray array];
            }
            [wedged addObject:key];
        }
    }
    // Recover AFTER enumeration — -recoverStalledStream mutates bulkStreams.
    for (NSNumber *key in wedged) {
        [self recoverStalledStream:key now:now];
    }
}

- (void)recoverStalledStream:(NSNumber *)key now:(uint64_t)now {
    MacrdpBulkStream *s = self.bulkStreams[key];
    if (s == nil) {
        return;
    }
    double stalledS = (double)(now - s.lastProgressNs) / 1.0e9;
    NSLog(@"[usb2] bulk-IN read-ahead STALL ep=0x%02lx — no in-order data for %.1fs "
          @"(inFlight=%ld); completing the waiting head empty + tearing down so macOS re-COMMITs",
          (unsigned long)([key unsignedIntegerValue] & 0xff), stalledS, (long)s.inFlight);
    // Unblock the ring head macOS is waiting on with a zero-length read. macOS
    // treats moved=0 on a streaming bulk-IN as a starved endpoint and destroys +
    // re-COMMITs it. Guarded by endpoint identity, so a stale endpoint is a no-op.
    const IOUSBHostCIMessage *head = s.pendingHead;
    IOUSBHostCIEndpointStateMachine *headEp = s.pendingEp;
    s.pendingHead = NULL;
    s.pendingEp = nil;
    if (head != NULL) {
        [self deliverStreamData:[NSData data] toMsg:head key:key expectedEp:headEp];
    }
    // Drop the wedged state + orphan the hung client reads (their late completions
    // find no stream and are dropped). The re-COMMIT re-engages read-ahead fresh
    // via -engageStream, so the stream recovers once the client resumes.
    [self tearDownStream:key];
}

// M0 isoch observe-only: record + log an isochronous TRB's shape and the service
// cadence macOS maintains, WITHOUT forwarding it to the client. Answers the
// go/no-go question "does macOS submit forwardable isoch TRBs to our virtual
// controller, and at what rate/depth?" before we build the isoch forward path.
- (void)observeIsochTransfer:(const IOUSBHostCIMessage *)msg
                 endpointKey:(NSNumber *)key
                          ep:(IOUSBHostCIEndpointStateMachine *)ep {
    uint32_t frame = (uint32_t)((msg->control & IOUSBHostCIIsochronousTransferControlFrameNumber)
                                >> IOUSBHostCIIsochronousTransferControlFrameNumberPhase);
    BOOL asap = (msg->control & IOUSBHostCIIsochronousTransferControlASAP) != 0;
    NSUInteger len = (msg->data0 & IOUSBHostCIIsochronousTransferData0Length)
                   >> IOUSBHostCIIsochronousTransferData0LengthPhase;
    uint64_t buf = msg->data1 & IOUSBHostCIIsochronousTransferData1Buffer;

    uint64_t nowNs = macrdp_now_ns();
    uint64_t count = self.isochCount[key].unsignedLongLongValue;
    NSNumber *lastNsObj = self.isochLastNs[key];
    self.isochCount[key] = @(count + 1);
    self.isochLastNs[key] = @(nowNs);

    // Full detail for the first 32 TRBs (shape + cadence), then a throttled summary
    // every 256th so a steady stream doesn't flood the log.
    if (count < 32 || (count % 256) == 0) {
        double gapMs = lastNsObj ? (double)(nowNs - lastNsObj.unsignedLongLongValue) / 1.0e6 : 0.0;
        NSLog(@"[usb2] ISOCH observe ep=0x%02lx trb#=%llu frame=%u asap=%d len=%lu buf=0x%llx gap=%.3fms (NOT forwarded — M0 go/no-go)",
              (unsigned long)([key unsignedIntegerValue] & 0xff), count, frame, asap ? 1 : 0,
              (unsigned long)len, buf, gapMs);
    }
}

// Walk an endpoint's transfer ring, completing sync stages inline and stopping at
// the first stage that must be forwarded to the client (which is raised async and
// re-enters this walk from its completion). Runs on the serial queue.
- (void)walkEndpoint:(NSNumber *)key {
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil) {
        return;
    }
    NSError *e = nil;
    for (int guard = 0; guard < 64; guard++) {
        if (ep.endpointState != IOUSBHostCIEndpointStateActive) {
            break;
        }
        const IOUSBHostCIMessage *msg = ep.currentTransferMessage;
        if (msg == NULL || !msg_valid(msg)) {
            break;
        }
        NSUInteger epNum = [key unsignedIntegerValue] & 0xff;
        // A read-ahead candidate is a device->host (IN) NormalTransfer whose read
        // is large enough to be a streaming transfer. The SIZE test is the
        // load-bearing discriminator: a webcam's bulk video read is tens of KB,
        // while an HID interrupt-IN poll (a gamepad input report) is tiny (<=
        // wMaxPacketSize, e.g. 64 B). Without it the gamepad's interrupt pipe was
        // pulled into the streaming re-doorbell branch below and starved — macOS
        // double-buffers interrupt endpoints too, so its re-doorbell hit
        // -engageStream: instead of the plain "skip the dup, the outstanding
        // completion re-walks" path, and the pipe was only serviced when something
        // else forced a re-walk (buttons went dead / the device re-enumerated).
        // Do NOT gate on the endpoint's DECLARED bulk/interrupt type instead: a UVC
        // video endpoint is frequently reported over the wire with is_bulk=false
        // (measured 69/81 on a redirected webcam), so a type gate wrongly excludes
        // the webcam and it shows no image. Read length is reliable; the declared
        // transfer type is not. (engageStream: re-checks the same threshold.)
        uint32_t normalReadLen = msg_type(msg) == IOUSBHostCIMessageTypeNormalTransfer
            ? (uint32_t)((msg->data0 & IOUSBHostCINormalTransferData0Length)
                         >> IOUSBHostCINormalTransferData0LengthPhase)
            : 0;
        BOOL isBulkIn = (epNum & 0x80) != 0 && (epNum & 0x7f) != 0
                        && msg_type(msg) == IOUSBHostCIMessageTypeNormalTransfer
                        && normalReadLen >= kStreamMinReadLen;
        // Bulk-IN read-ahead: once a streaming endpoint is engaged, serve its ring
        // TRBs from the prefetch FIFO (never a fresh tied raise).
        if (isBulkIn && self.bulkStreams[key] != nil) {
            if ([self serveStreamHead:msg endpointKey:key ep:ep]) {
                continue;   // fed the head from read-ahead; ring advanced
            }
            return;         // awaiting data — a prefetch completion re-walks
        }
        // If this exact ring head is ALREADY forwarded and awaiting a client
        // completion, do NOT re-raise it. The ring only advances on
        // enqueueTransferCompletionForMessage: (from the completion path), so a
        // doorbell re-entering the walk while a transfer is outstanding re-reads
        // the SAME unadvanced head. For a streaming bulk-IN endpoint that
        // re-doorbell means macOS wants URB depth (double/triple buffering), so we
        // ENGAGE the read-ahead engine — the old serial one-read-at-a-time path
        // starved the device's pipe (it produced zero bytes and macOS tore the
        // stream down). For control / bulk-OUT we just skip the duplicate: the
        // outstanding transfer's completion re-walks. A genuine kernel timeout/
        // abort re-issues a DIFFERENT ring slot (different msg), so this returns NO
        // and registerPendingForMsg: supersedes it as designed.
        if ([self ringHeadAlreadyOutstanding:msg endpointKey:key]) {
            if (isBulkIn) {
                [self engageStream:msg endpointKey:key ep:ep];
                return;
            }
            NSLog(@"[usb2] ring head already outstanding — skipping re-raise (double-buffered)");
            return;
        }
        // M0 isoch observe-only: an isochronous TRB (type 0x3b) is NOT forwarded
        // yet — log its shape/cadence and complete it as a zero-length success so
        // the ring advances and we can watch how macOS schedules the stream. This
        // is the go/no-go gate before building the isoch forward path (M1).
        if (msg_type(msg) == IOUSBHostCIMessageTypeIsochronousTransfer) {
            [self observeIsochTransfer:msg endpointKey:key ep:ep];
            if (![ep enqueueTransferCompletionForMessage:msg
                                                 status:IOUSBHostCIMessageStatusSuccess
                                         transferLength:0
                                                  error:&e]) {
                NSLog(@"[usb2] isoch observe enqueueTransferCompletion failed: %@", e);
                break;
            }
            continue; // ring advanced; keep walking queued isoch TRBs
        }
        if ([self transferIsForwardedControlIn:msg endpointKey:key]) {
            [self raiseControlIn:msg endpointKey:key];
            return; // async; completion re-walks
        }
        if ([self transferIsForwardedControlOut:msg endpointKey:key]) {
            [self raiseControlOut:msg endpointKey:key];
            return; // async; completion re-walks
        }
        if ([self transferIsBulk:msg endpointKey:key]) {
            [self raiseBulk:msg endpointKey:key];
            return; // async; completion re-walks
        }
        NSUInteger moved = 0;
        IOUSBHostCIMessageStatus status = [self processTransfer:msg endpointKey:key transferLength:&moved];
        if (msg_no_response(msg)) {
            NSLog(@"[usb2] EP0 transfer with NoResponse set — stopping ring walk");
            break;
        }
        if (![ep enqueueTransferCompletionForMessage:msg status:status transferLength:moved error:&e]) {
            NSLog(@"[usb2] enqueueTransferCompletion failed: %@", e);
            break;
        }
    }
}

- (void)handleDoorbell:(IOUSBHostCIDoorbell)doorbell {
    NSUInteger addr = (doorbell & IOUSBHostCIDoorbellDeviceAddress) >> IOUSBHostCIDoorbellDeviceAddressPhase;
    NSUInteger epAddr = (doorbell & IOUSBHostCIDoorbellEndpointAddress) >> IOUSBHostCIDoorbellEndpointAddressPhase;
    NSNumber *key = [self endpointKeyForDevice:addr endpoint:epAddr];
    IOUSBHostCIEndpointStateMachine *ep = self.endpoints[key];
    if (ep == nil) {
        NSLog(@"[usb2] doorbell for unknown dev=%lu ep=0x%02lx", (unsigned long)addr, (unsigned long)epAddr);
        return;
    }
    NSError *e = nil;
    if (![ep processDoorbell:doorbell error:&e]) {
        NSLog(@"[usb2] processDoorbell dev=%lu ep=0x%02lx failed: %@", (unsigned long)addr, (unsigned long)epAddr, e);
        return;
    }
    [self walkEndpoint:key];
}

- (BOOL)startWithError:(NSError **)error {
    // Controller capabilities: one root port.
    IOUSBHostCIMessage controllerCaps = {
        .control = (IOUSBHostCIMessageTypeControllerCapabilities << IOUSBHostCIMessageControlTypePhase)
                 | IOUSBHostCIMessageControlNoResponse
                 | IOUSBHostCIMessageControlValid
                 | (1u << IOUSBHostCICapabilitiesMessageControlPortCountPhase),
        .data0   = (1u << IOUSBHostCICapabilitiesMessageData0CommandTimeoutThresholdPhase)
                 | (2u << IOUSBHostCICapabilitiesMessageData0ConnectionLatencyPhase),
        .data1   = 0,
    };
    // Port capabilities: port #1, ACPI Type-A connector.
    IOUSBHostCIMessage portCaps = {
        .control = (IOUSBHostCIMessageTypePortCapabilities << IOUSBHostCIMessageControlTypePhase)
                 | IOUSBHostCIMessageControlNoResponse
                 | IOUSBHostCIMessageControlValid
                 | (1u << IOUSBHostCIPortCapabilitiesMessageControlPortNumberPhase)
                 | (0u << IOUSBHostCIPortCapabilitiesMessageControlConnectorTypePhase),
        .data0   = ((907 / 8) << IOUSBHostCIPortCapabilitiesMessageData0MaxPowerPhase),
        .data1   = 0,
    };
    NSMutableData *caps = [[NSMutableData alloc] initWithBytes:&controllerCaps length:sizeof(IOUSBHostCIMessage)];
    [caps appendBytes:&portCaps length:sizeof(IOUSBHostCIMessage)];

    __weak MacrdpUsbController *weakSelf = self;
    IOUSBHostControllerInterface *iface =
        [[IOUSBHostControllerInterface alloc]
            initWithCapabilities:caps
                           queue:nil
                 interruptRateHz:1000
                           error:error
                  commandHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIMessage command) {
                      (void)c;
                      [weakSelf handleCommand:&command];
                  }
                 doorbellHandler:^(IOUSBHostControllerInterface *c, IOUSBHostCIDoorbell *doorbells, uint32_t count) {
                      (void)c;
                      for (uint32_t i = 0; i < count; i++) {
                          [weakSelf handleDoorbell:doorbells[i]];
                      }
                  }
                 interestHandler:NULL];
    if (iface == nil) {
        return NO;
    }
    self.interface = iface;
    [self startStreamWatchdog];   // recovers a bulk-IN stream whose completions go silent
    return YES;
}

- (void)stop {
    if (self.streamWatchdog != nil) {
        dispatch_source_cancel(self.streamWatchdog);
        self.streamWatchdog = nil;
    }
    [self.interface destroy];
    self.interface = nil;
}

// Safety net: a resumed dispatch source released without a prior cancel crashes.
// -stop is the normal teardown (called by macrdp_usb_controller_destroy) and
// already cancels; this covers any path that releases the controller without it.
- (void)dealloc {
    if (_streamWatchdog != nil) {
        dispatch_source_cancel(_streamWatchdog);
        _streamWatchdog = nil;
    }
}

@end

// ---- hardcoded synthetic descriptors (standalone --usb-spike path only) ------
// Full-speed vendor-specific device with only the default control endpoint.
// Full-speed so the host never asks for a device_qualifier (which we'd stall).
static const uint8_t kDeviceDescriptor[] = {
    0x12,       // bLength
    0x01,       // bDescriptorType = DEVICE
    0x00, 0x02, // bcdUSB 2.00
    0xFF,       // bDeviceClass = vendor-specific
    0x00,       // bDeviceSubClass
    0x00,       // bDeviceProtocol
    0x40,       // bMaxPacketSize0 = 64
    0x09, 0x12, // idVendor  = 0x1209 (pid.codes)
    0x01, 0x00, // idProduct = 0x0001 (test)
    0x00, 0x01, // bcdDevice 1.00
    0x01,       // iManufacturer -> string 1
    0x02,       // iProduct      -> string 2
    0x00,       // iSerialNumber -> none
    0x01,       // bNumConfigurations
};
static const uint8_t kConfigDescriptor[] = {
    0x09, 0x02, 0x12, 0x00, 0x01, 0x01, 0x00, 0x80, 0x32,  // configuration
    0x09, 0x04, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00,  // interface (EP0 only)
};
static const uint8_t kStringLangIDs[] = { 0x04, 0x03, 0x09, 0x04 }; // 0x0409 en-US
static const uint8_t kStringManufacturer[] = {
    0x0E, 0x03, 'm', 0, 'a', 0, 'c', 0, 'r', 0, 'd', 0, 'p', 0,
};
static const uint8_t kStringProduct[] = {
    0x16, 0x03, 'm', 0, 'a', 0, 'c', 0, 'r', 0, 'd', 0, 'p', 0,
    ' ', 0, 'U', 0, 'S', 0, 'B', 0,
};

static BOOL spike_descriptor_for_value(uint16_t wValue, const uint8_t **outBytes, size_t *outLen) {
    uint8_t descType = (wValue >> 8) & 0xff;
    uint8_t descIndex = wValue & 0xff;
    switch (descType) {
        case kUsbDescDevice:        *outBytes = kDeviceDescriptor; *outLen = sizeof(kDeviceDescriptor); return YES;
        case kUsbDescConfiguration: *outBytes = kConfigDescriptor; *outLen = sizeof(kConfigDescriptor); return YES;
        case kUsbDescString:
            switch (descIndex) {
                case 0: *outBytes = kStringLangIDs;      *outLen = sizeof(kStringLangIDs);      return YES;
                case 1: *outBytes = kStringManufacturer; *outLen = sizeof(kStringManufacturer); return YES;
                case 2: *outBytes = kStringProduct;      *outLen = sizeof(kStringProduct);      return YES;
                default: return NO;
            }
        default:
            return NO; // device_qualifier, BOS, etc. — stall.
    }
}

// =============================================================================
// C ABI — the bidirectional FFI src/usb_redirect/mod.rs drives.
// =============================================================================

// Create the controller. `cb`/`ctx` receive each EP0 control-IN forward; may be
// NULL (then such transfers stall). Returns an opaque, retained handle (destroy
// releases it), or NULL on init failure with *err set.
void *macrdp_usb_controller_create(MacrdpUsbControlInFn controlCb, MacrdpUsbControlOutFn controlOutCb,
                                   MacrdpUsbBulkFn bulkCb, void *ctx, int *err) {
    @autoreleasepool {
        MacrdpUsbController *c = [[MacrdpUsbController alloc] init];
        c.controlInCb = controlCb;
        c.controlOutCb = controlOutCb;
        c.bulkCb = bulkCb;
        c.cbCtx = ctx;
        NSError *e = nil;
        if (![c startWithError:&e] || (e != nil && e.code != KERN_SUCCESS)) {
            NSLog(@"[usb2] NO-GO: IOUSBHostControllerInterface init failed: %@", e);
            if (err) {
                *err = e ? (int)e.code : -1;
            }
            return NULL;
        }
        if (err) {
            *err = 0;
        }
        return (void *)CFBridgingRetain(c);
    }
}

// Complete a previously-raised control-IN or bulk transfer. status 0 = success;
// for IN `bytes`/`len` are copied into the buffer, for OUT pass bytes=NULL and
// len=bytes-accepted. Nonzero status = stall. Safe from any thread; hops to queue.
void macrdp_usb_complete_transfer(void *handle, uint64_t token,
                                  const uint8_t *bytes, uint32_t len, int32_t status) {
    if (handle == NULL) {
        return;
    }
    MacrdpUsbController *c = (__bridge MacrdpUsbController *)handle;
    IOUSBHostCIMessageStatus st = (status == 0) ? IOUSBHostCIMessageStatusSuccess : IOUSBHostCIMessageStatusStallError;
    [c completeTransferToken:token bytes:bytes len:len status:st];
}

// Stop + release a controller created by macrdp_usb_controller_create.
void macrdp_usb_controller_destroy(void *handle) {
    if (handle == NULL) {
        return;
    }
    @autoreleasepool {
        MacrdpUsbController *c = (MacrdpUsbController *)CFBridgingRelease(handle);
        NSLog(@"[usb2] controller destroy: stopping interface");
        [c stop];
        NSLog(@"[usb2] controller destroy: interface destroyed");
    }
}

// ---- standalone --usb-spike: synthetic device via the async path -------------
// Proves the async completion boundary end-to-end with no RDP client: the kernel
// raises each EP0 control-IN through the same callback the real driver uses, and
// this built-in handler answers from the hardcoded descriptors above.
static void *g_spike_handle = NULL;

static void spike_control_in_cb(void *ctx, uint64_t token, const uint8_t *setup8, uint32_t max_len) {
    (void)ctx;
    (void)max_len;
    uint8_t bRequest = setup8[1];
    uint16_t wValue = (uint16_t)setup8[2] | ((uint16_t)setup8[3] << 8);
    if (bRequest != kUsbReqGetDescriptor) {
        macrdp_usb_complete_transfer(g_spike_handle, token, NULL, 0, 1); // stall
        return;
    }
    const uint8_t *desc = NULL;
    size_t descLen = 0;
    if (!spike_descriptor_for_value(wValue, &desc, &descLen)) {
        NSLog(@"[usb2] spike: unsupported GET_DESCRIPTOR wValue=0x%04x -> STALL", wValue);
        macrdp_usb_complete_transfer(g_spike_handle, token, NULL, 0, 1);
        return;
    }
    macrdp_usb_complete_transfer(g_spike_handle, token, desc, (uint32_t)descLen, 0);
}

int macrdp_usb_spike_run(void) {
    @autoreleasepool {
        int err = 0;
        void *handle = macrdp_usb_controller_create(spike_control_in_cb, NULL, NULL, NULL, &err);
        if (handle == NULL) {
            return err != 0 ? err : -1;
        }
        g_spike_handle = handle;
        NSLog(@"[usb2] controller created (async path) — driving enumeration. Watch for the "
              @"synthetic device (VID 0x1209/PID 0x0001) in `ioreg -p IOUSB` / "
              @"`system_profiler SPUSBDataType`. Holding 20s...");
        [NSThread sleepForTimeInterval:20.0];
        g_spike_handle = NULL;
        macrdp_usb_controller_destroy(handle);
        NSLog(@"[usb2] controller destroyed; exiting");
        // Teardown testbed: MACRDP_USB_SPIKE_LINGER_SECS keeps the process alive
        // after destroy so `ioreg -p IOUSB` can verify the synthetic device is
        // actually gone (process exit would reap the controller and mask a broken
        // destroy — the exact bug class this exists to diagnose).
        const char *linger = getenv("MACRDP_USB_SPIKE_LINGER_SECS");
        if (linger != NULL) {
            double secs = atof(linger);
            NSLog(@"[usb2] lingering %.0fs post-destroy (check ioreg for the synthetic device)", secs);
            [NSThread sleepForTimeInterval:secs];
        }
        return 0;
    }
}
