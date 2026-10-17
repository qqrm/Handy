// Public, PID-addressed QuickTime Apple Events only. No process launch, media
// keys, MediaRemote, browser scripting, or interactive consent during recording.
#import <Cocoa/Cocoa.h>
#import <CoreServices/CoreServices.h>
#import <libproc.h>
#import <errno.h>
#import <math.h>
#import <stdbool.h>
#import <signal.h>
#ifdef HANDY_QUICKTIME_FIXTURE
#import <stdatomic.h>
static _Atomic int fixtureStopPID = 0;
static _Atomic int fixtureCommands = 0;
void handy_qt_fixture_stop_before_command(int pid) { atomic_store(&fixtureStopPID, pid); }
int handy_qt_fixture_command_count(void) { return atomic_load(&fixtureCommands); }
#endif

// Codes from the installed QuickTime public dictionary, asserted by native CI.
enum { QTMovieSuite = 1297504082, QTPlay = 1886151033, QTPause = 1885435251,
       QTPlaying = 1886151033, QTTime = 1953066341 };
static const NSTimeInterval ReadDeadline = 0.3;
static const NSTimeInterval CommandDeadline = 1.0;

@interface HandyQuickTimeTarget : NSObject
@property(nonatomic) pid_t pid;
@property(nonatomic) uint64_t startSeconds;
@property(nonatomic) uint64_t startMicros;
@property(nonatomic, strong) NSDate *launchDate;
@property(nonatomic, strong) NSAppleEventDescriptor *address;
@property(nonatomic, strong) NSAppleEventDescriptor *window;
@property(nonatomic, strong) NSAppleEventDescriptor *windowID;
@property(nonatomic, strong) NSAppleEventDescriptor *document;
@property(nonatomic, strong) NSURL *file;
@end
@implementation HandyQuickTimeTarget
@end

static void qtRequire(BOOL valid) {
    if (!valid) @throw [NSException exceptionWithName:@"QuickTimeUnavailable" reason:nil userInfo:nil];
}
// NSRunningApplication's cached terminated flag alone is insufficient on a
// worker without an AppKit run loop. The kernel start timestamp fences PID reuse.
static int qtProcess(HandyQuickTimeTarget *item) {
    struct proc_bsdinfo info = {0};
    errno = 0;
    int size = proc_pidinfo(item.pid, PROC_PIDTBSDINFO, 0, &info, sizeof(info));
    if (size != (int)sizeof(info)) {
        if (errno == ESRCH || (kill(item.pid, 0) != 0 && errno == ESRCH)) return 0;
        return -1;
    }
    return info.pbi_start_tvsec == item.startSeconds && info.pbi_start_tvusec == item.startMicros ? 1 : 0;
}
static NSAppleEventDescriptor *qtSpec(OSType kind, NSAppleEventDescriptor *container,
                                    OSType form, NSAppleEventDescriptor *key) {
    NSAppleEventDescriptor *record = [NSAppleEventDescriptor recordDescriptor];
    [record setDescriptor:[NSAppleEventDescriptor descriptorWithTypeCode:kind] forKeyword:keyAEDesiredClass];
    [record setDescriptor:container ?: [NSAppleEventDescriptor nullDescriptor] forKeyword:keyAEContainer];
    [record setDescriptor:[NSAppleEventDescriptor descriptorWithEnumCode:form] forKeyword:keyAEKeyForm];
    [record setDescriptor:key forKeyword:keyAEKeyData];
    NSAppleEventDescriptor *result = [record coerceToDescriptorType:typeObjectSpecifier];
    qtRequire(result != nil);
    return result;
}
static NSAppleEventDescriptor *qtEvent(HandyQuickTimeTarget *item, OSType eventClass,
                                     OSType eventID, NSAppleEventDescriptor *object) {
    NSAppleEventDescriptor *event = [NSAppleEventDescriptor appleEventWithEventClass:eventClass
        eventID:eventID targetDescriptor:item.address returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
    [event setParamDescriptor:object forKeyword:keyDirectObject];
    return event;
}
static NSAppleEventDescriptor *qtSend(NSAppleEventDescriptor *event, NSTimeInterval timeout) {
    NSError *error = nil;
    NSAppleEventDescriptor *reply = [event sendEventWithOptions:
        NSAppleEventSendWaitForReply | NSAppleEventSendNeverInteract |
        (NSAppleEventSendOptions)kAEDoNotPromptForUserConsent timeout:timeout error:&error];
    qtRequire(reply && !error);
    NSAppleEventDescriptor *number = [reply paramDescriptorForKeyword:keyErrorNumber];
    qtRequire(!number || number.int32Value == 0);
    return reply;
}
static NSAppleEventDescriptor *qtGet(HandyQuickTimeTarget *item, NSAppleEventDescriptor *object) {
    qtRequire(qtProcess(item) == 1);
    NSAppleEventDescriptor *reply = qtSend(qtEvent(item, kAECoreSuite, kAEGetData, object), ReadDeadline);
    NSAppleEventDescriptor *value = [reply paramDescriptorForKeyword:keyDirectObject];
    qtRequire(value && qtProcess(item) == 1);
    return value;
}
static NSAppleEventDescriptor *qtProperty(HandyQuickTimeTarget *item, NSAppleEventDescriptor *object, OSType code) {
    return qtGet(item, qtSpec(cProperty, object, formPropertyID,
        [NSAppleEventDescriptor descriptorWithTypeCode:code]));
}
static NSAppleEventDescriptor *qtInteger(NSAppleEventDescriptor *value) {
    DescType kind = value.descriptorType;
    qtRequire(value && (kind == typeSInt16 || kind == typeSInt32 || kind == typeUInt32 || kind == typeSInt64));
    NSAppleEventDescriptor *result = [value coerceToDescriptorType:typeSInt64];
    qtRequire(result && result.data.length == sizeof(SInt64));
    return result;
}
static NSAppleEventDescriptor *qtWindowID(HandyQuickTimeTarget *item, NSAppleEventDescriptor *window) {
    NSAppleEventDescriptor *resolved = qtGet(item, window);
    qtRequire(resolved.descriptorType == typeObjectSpecifier);
    NSAppleEventDescriptor *record = [resolved coerceToDescriptorType:typeAERecord];
    NSAppleEventDescriptor *kind = [record descriptorForKeyword:keyAEDesiredClass];
    NSAppleEventDescriptor *form = [record descriptorForKeyword:keyAEKeyForm];
    NSAppleEventDescriptor *container = [record descriptorForKeyword:keyAEContainer];
    qtRequire(kind.descriptorType == typeType && kind.typeCodeValue == cWindow &&
        form.descriptorType == typeEnumerated && form.enumCodeValue == formUniqueID &&
        container && container.descriptorType == typeNull);
    return qtInteger([record descriptorForKeyword:keyAEKeyData]);
}
static NSURL *qtFile(HandyQuickTimeTarget *item) {
    NSAppleEventDescriptor *file = [qtProperty(item, item.document, 'file') coerceToDescriptorType:typeFileURL];
    NSURL *url = file.fileURLValue;
    qtRequire(url && url.isFileURL);
    return url.URLByStandardizingPath;
}
static void qtGuard(HandyQuickTimeTarget *item) {
    qtRequire(qtProcess(item) == 1);
    qtRequire([qtWindowID(item, item.window).data isEqual:item.windowID.data]);
    qtRequire([qtFile(item) isEqual:item.file]);
}
static int qtSnapshot(HandyQuickTimeTarget *item, double *position) {
    qtGuard(item);
    NSAppleEventDescriptor *playing = [qtProperty(item, item.document, QTPlaying) coerceToDescriptorType:typeBoolean];
    NSAppleEventDescriptor *time = [qtProperty(item, item.document, QTTime) coerceToDescriptorType:typeIEEE64BitFloatingPoint];
    qtRequire(playing && time && isfinite(time.doubleValue) && time.doubleValue >= 0);
    qtGuard(item);
    *position = time.doubleValue;
    return playing.booleanValue ? 1 : 0;
}
static HandyQuickTimeTarget *qtProcessTarget(NSRunningApplication *app) {
    qtRequire(app && !app.terminated && app.launchDate &&
        [app.bundleIdentifier isEqualToString:@"com.apple.QuickTimePlayerX"]);
    struct proc_bsdinfo info = {0};
    qtRequire(proc_pidinfo(app.processIdentifier, PROC_PIDTBSDINFO, 0, &info, sizeof(info)) == (int)sizeof(info));
    HandyQuickTimeTarget *item = [HandyQuickTimeTarget new];
    item.pid = app.processIdentifier;
    item.startSeconds = info.pbi_start_tvsec;
    item.startMicros = info.pbi_start_tvusec;
    item.launchDate = app.launchDate;
    // Fence discovery racing a restart: Cocoa and kernel timestamps must agree.
    double kernelStart = item.startSeconds + item.startMicros / 1000000.0;
    qtRequire(fabs(item.launchDate.timeIntervalSince1970 - kernelStart) < 0.01);
    item.address = [NSAppleEventDescriptor descriptorWithProcessIdentifier:item.pid];
    return item;
}
// Ownership of every nonnull pointer crosses the C ABI at +1 and has one release.
void handy_qt_release(void *value) { if (value) { @autoreleasepool { (void)CFBridgingRelease(value); } } }
void *handy_qt_discover(size_t *count, int *status) {
    @autoreleasepool {
        *count = 0; *status = -1;
        @try {
            NSMutableArray *items = [NSMutableArray array];
            NSArray *apps = [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.QuickTimePlayerX"];
            qtRequire(apps.count <= 1); // Ambiguous ownership: do nothing.
            for (NSRunningApplication *app in apps) {
                HandyQuickTimeTarget *process = qtProcessTarget(app);
                qtRequire(AEDeterminePermissionToAutomateTarget(process.address.aeDesc, typeWildCard, typeWildCard, false) == noErr);
                OSType ordinal = kAEAll;
                NSAppleEventDescriptor *windows = qtGet(process, qtSpec(cWindow, nil, formAbsolutePosition,
                    [NSAppleEventDescriptor descriptorWithDescriptorType:typeAbsoluteOrdinal bytes:&ordinal length:sizeof(ordinal)]));
                qtRequire(windows.descriptorType == typeAEList && windows.numberOfItems <= 32);
                for (NSInteger index = 1; index <= windows.numberOfItems; ++index) {
                    // Non-document windows are excluded; never target by index/name.
                    @try {
                        HandyQuickTimeTarget *item = qtProcessTarget(app);
                        NSAppleEventDescriptor *window = [windows descriptorAtIndex:index];
                        item.windowID = qtWindowID(item, window);
                        qtRequire([qtInteger(qtProperty(item, window, 'ID  ')).data isEqual:item.windowID.data]);
                        item.window = qtSpec(cWindow, nil, formUniqueID, item.windowID);
                        item.document = qtSpec(cProperty, item.window, formPropertyID,
                            [NSAppleEventDescriptor descriptorWithTypeCode:'docu']);
                        item.file = qtFile(item);
                        double position = 0;
                        (void)qtSnapshot(item, &position);
                        [items addObject:item];
                    } @catch (__unused NSException *error) { /* No usable stable target. */ }
                }
            }
            *count = items.count; *status = 0;
            return (void *)CFBridgingRetain([items copy]);
        } @catch (__unused NSException *error) { return NULL; }
    }
}
void *handy_qt_session(void *list, size_t index) {
    @autoreleasepool {
        NSArray *items = (__bridge NSArray *)list;
        return index < items.count ? (void *)CFBridgingRetain(items[index]) : NULL;
    }
}
int handy_qt_live(void *raw) {
    @autoreleasepool { return qtProcess((__bridge HandyQuickTimeTarget *)raw); }
}
int handy_qt_state(void *raw, double *position) {
    @autoreleasepool {
        @try { return qtSnapshot((__bridge HandyQuickTimeTarget *)raw, position); }
        @catch (__unused NSException *error) { return -1; }
    }
}
// 0=no submission; 1=acknowledged; 2=unknown AFTER submission; -1=preflight error.
// A timeout is not cancellation: the original target must remain quarantined.
int handy_qt_command(void *raw, int play, bool (*allowed)(void *, double), void *context) {
    @autoreleasepool {
        BOOL submitted = NO;
        @try {
            HandyQuickTimeTarget *item = (__bridge HandyQuickTimeTarget *)raw;
            double position = 0;
            int state = qtSnapshot(item, &position);
            if (state != (play ? 0 : 1)) return 0;
            NSAppleEventDescriptor *event = qtEvent(item, QTMovieSuite, play ? QTPlay : QTPause, item.document);
            // Last check follows ALL provider IPC; the callback does not block or
            // send events. The residual read/command race is documented best effort.
            if (qtProcess(item) != 1 || !allowed(context, position)) return 0;
#ifdef HANDY_QUICKTIME_FIXTURE
            int stopped = atomic_exchange(&fixtureStopPID, 0);
            if (stopped == item.pid) qtRequire(kill(stopped, SIGSTOP) == 0);
            atomic_fetch_add(&fixtureCommands, 1);
#endif
            submitted = YES;
            (void)qtSend(event, CommandDeadline);
            return 1;
        } @catch (__unused NSException *error) { return submitted ? 2 : -1; }
    }
}
int handy_qt_request_permission(void) {
    @autoreleasepool {
        @try {
            NSArray *apps = [NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.QuickTimePlayerX"];
            if (apps.count != 1) return 0;
            HandyQuickTimeTarget *item = qtProcessTarget(apps.firstObject);
            OSStatus result = AEDeterminePermissionToAutomateTarget(item.address.aeDesc, typeWildCard, typeWildCard, true);
            return result == noErr && qtProcess(item) == 1 ? 1 : -1;
        } @catch (__unused NSException *error) { return -1; }
    }
}
