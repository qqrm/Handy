#!/usr/bin/env python3
"""Hosted-only real QuickTime Apple Events diagnostic. Never a product backend.

Run on a fresh standard macos-15-intel or macos-26 GitHub VM:
  HANDY_RUN_ISOLATED_QUICKTIME_TESTS=1 python3 scripts/quicktime_probe.py

Requires Xcode command-line tools. Writes quicktime-probe-result.json in
RUNNER_TEMP. No network media, private frameworks, input injection, global
play/pause keys, or TCC database changes. The exact QuickTime consent dialog
may be accepted on this isolated VM. Denial/timeouts fail, never skip.
"""
import hashlib
import json
import os
from pathlib import Path
import plistlib
import signal
import struct
import subprocess
import sys
import tempfile
import time
import wave
import xml.etree.ElementTree as ET

NATIVE = r'''
#import <Cocoa/Cocoa.h>
#import <ApplicationServices/ApplicationServices.h>
#import <CoreServices/CoreServices.h>
#import <math.h>
#import <unistd.h>

static NSDictionary *cfg;
static NSMutableDictionary *report;
static NSMutableArray *checks;
static NSRunningApplication *player;
static NSDate *started;
static NSAppleEventDescriptor *target;
static NSString *receipt;
static BOOL ownsPlayer = NO;
static NSInteger playbackCommands = 0;

static void probeRequire(BOOL ok, NSString *why) {
    if (!ok) @throw [NSException exceptionWithName:@"ProbeFailure" reason:why userInfo:nil];
}
static void save(void) {
    report[@"checks"] = checks;
    NSError *error = nil;
    NSData *data = [NSJSONSerialization dataWithJSONObject:report
        options:NSJSONWritingPrettyPrinted error:&error];
    probeRequire(data != nil, [NSString stringWithFormat:@"JSON receipt: %@", error]);
    probeRequire([data writeToFile:receipt options:NSDataWritingAtomic error:&error],
        [NSString stringWithFormat:@"Receipt write: %@", error]);
}
static void recordCheck(NSString *name, NSDictionary *facts) {
    [checks addObject:@{@"name": name, @"status": @"passed", @"facts": facts}];
    save();
}
static OSType code(NSString *key) { return (OSType)[cfg[@"codes"][key] unsignedIntValue]; }
static void live(void) {
    NSRunningApplication *now = [NSRunningApplication runningApplicationWithProcessIdentifier:player.processIdentifier];
    probeRequire(!player.terminated && now && !now.terminated &&
        [now.bundleIdentifier isEqualToString:@"com.apple.QuickTimePlayerX"] &&
        started && [now.launchDate isEqualToDate:started], @"QuickTime process generation changed");
}
static NSAppleEventDescriptor *spec(OSType kind, NSAppleEventDescriptor *container,
                                    OSType form, NSAppleEventDescriptor *key) {
    NSAppleEventDescriptor *record = [NSAppleEventDescriptor recordDescriptor];
    [record setDescriptor:[NSAppleEventDescriptor descriptorWithTypeCode:kind] forKeyword:keyAEDesiredClass];
    [record setDescriptor:container ?: [NSAppleEventDescriptor nullDescriptor] forKeyword:keyAEContainer];
    [record setDescriptor:[NSAppleEventDescriptor descriptorWithEnumCode:form] forKeyword:keyAEKeyForm];
    [record setDescriptor:key forKeyword:keyAEKeyData];
    NSAppleEventDescriptor *out = [record coerceToDescriptorType:typeObjectSpecifier];
    probeRequire(out != nil, @"Could not construct Apple Event object specifier");
    return out;
}
static NSAppleEventDescriptor *sendAppleEvent(OSType eventClass, OSType eventID,
                                    NSAppleEventDescriptor *object, BOOL wantsValue) {
    live();
    NSAppleEventDescriptor *event = [NSAppleEventDescriptor appleEventWithEventClass:eventClass
        eventID:eventID targetDescriptor:target returnID:kAutoGenerateReturnID
        transactionID:kAnyTransactionID];
    if (object) [event setParamDescriptor:object forKeyword:keyDirectObject];
    NSError *error = nil;
    NSAppleEventSendOptions mode = NSAppleEventSendWaitForReply | NSAppleEventSendNeverInteract |
        (NSAppleEventSendOptions)kAEDoNotPromptForUserConsent;
    NSAppleEventDescriptor *reply = [event sendEventWithOptions:mode timeout:4.0 error:&error];
    probeRequire(reply != nil, [NSString stringWithFormat:@"Apple Event transport failed: %@", error]);
    NSAppleEventDescriptor *number = [reply paramDescriptorForKeyword:keyErrorNumber];
    probeRequire(!number || number.int32Value == 0, [NSString stringWithFormat:
        @"Apple Event error %d: %@", number.int32Value,
        [reply paramDescriptorForKeyword:keyErrorString].stringValue]);
    NSAppleEventDescriptor *value = [reply paramDescriptorForKeyword:keyDirectObject];
    probeRequire(!wantsValue || value != nil, @"Apple Event omitted required result");
    live();
    return value;
}
static NSAppleEventDescriptor *get(NSAppleEventDescriptor *object) {
    return sendAppleEvent(kAECoreSuite, kAEGetData, object, YES);
}
static NSAppleEventDescriptor *property(NSAppleEventDescriptor *doc, OSType propertyCode) {
    return get(spec(cProperty, doc, formPropertyID,
        [NSAppleEventDescriptor descriptorWithTypeCode:propertyCode]));
}
static NSAppleEventDescriptor *documents(void) {
    OSType ordinal = kAEAll;
    NSAppleEventDescriptor *list = get(spec(code(@"document"), nil, formAbsolutePosition,
        [NSAppleEventDescriptor descriptorWithDescriptorType:typeAbsoluteOrdinal
            bytes:&ordinal length:sizeof(ordinal)]));
    probeRequire(list.descriptorType == typeAEList, @"Documents reply is not a list");
    return list;
}
static NSAppleEventDescriptor *documentIdentity(NSAppleEventDescriptor *doc) {
    // Cocoa may return an ID-based reference without declaring an `id`
    // property in the app's scripting dictionary. Inspect the actual reference;
    // never treat an index or display name as a durable document identity.
    NSAppleEventDescriptor *resolved = get(doc);
    report[@"last_document_reference"] = resolved.description ?: @"(no descriptor description)";
    probeRequire(resolved.descriptorType == typeObjectSpecifier,
        @"QuickTime did not return a document object specifier");
    NSAppleEventDescriptor *record = [resolved coerceToDescriptorType:typeAERecord];
    NSAppleEventDescriptor *kind = [record descriptorForKeyword:keyAEDesiredClass];
    NSAppleEventDescriptor *form = [record descriptorForKeyword:keyAEKeyForm];
    NSAppleEventDescriptor *container = [record descriptorForKeyword:keyAEContainer];
    NSAppleEventDescriptor *identity = [record descriptorForKeyword:keyAEKeyData];
    probeRequire(record && kind.descriptorType == typeType && kind.typeCodeValue == code(@"document") &&
        form.descriptorType == typeEnumerated && form.enumCodeValue == formUniqueID &&
        container && container.descriptorType == typeNull && identity &&
        identity.descriptorType != typeNull && identity.data.length > 0,
        @"QuickTime reference lacks a stable formUniqueID document identity; no name/index fallback");
    return identity;
}
static NSAppleEventDescriptor *freeze(NSAppleEventDescriptor *doc) {
    NSAppleEventDescriptor *identity = documentIdentity(doc);
    NSAppleEventDescriptor *frozen = spec(code(@"document"), nil, formUniqueID, identity);
    NSAppleEventDescriptor *roundtrip = documentIdentity(frozen);
    probeRequire(identity.descriptorType == roundtrip.descriptorType && [identity.data isEqual:roundtrip.data],
        @"Document ID did not roundtrip through formUniqueID");
    return frozen;
}
static NSString *identity(NSAppleEventDescriptor *doc) {
    NSAppleEventDescriptor *value = documentIdentity(doc);
    return [NSString stringWithFormat:@"%08x:%@", (unsigned)value.descriptorType,
        [value.data base64EncodedStringWithOptions:0]];
}
static BOOL playing(NSAppleEventDescriptor *doc) {
    NSAppleEventDescriptor *value = [property(doc, code(@"playing")) coerceToDescriptorType:typeBoolean];
    probeRequire(value != nil, @"Playing property is not boolean");
    return value.booleanValue;
}
static double position(NSAppleEventDescriptor *doc) {
    NSAppleEventDescriptor *value = [property(doc, code(@"current_time"))
        coerceToDescriptorType:typeIEEE64BitFloatingPoint];
    probeRequire(value != nil && isfinite(value.doubleValue), @"Current time is not finite numeric data");
    return value.doubleValue;
}
static void command(NSString *name, NSAppleEventDescriptor *doc) {
    playbackCommands += 1;
    sendAppleEvent(code([name stringByAppendingString:@"_class"]), code([name stringByAppendingString:@"_id"]), doc, NO);
}
static void waitState(NSAppleEventDescriptor *doc, BOOL expected) {
    for (int i = 0; i < 30; ++i) {
        if (playing(doc) == expected) return;
        [NSThread sleepForTimeInterval:0.1];
    }
    probeRequire(NO, @"Player did not acknowledge requested playback state");
}
static double assertAdvancing(NSAppleEventDescriptor *doc, NSString *name) {
    waitState(doc, YES);
    double before = position(doc);
    [NSThread sleepForTimeInterval:1.25];
    double after = position(doc);
    probeRequire(playing(doc) && after - before >= 0.35 && after - before < 5.0,
        [NSString stringWithFormat:@"%@: playback did not advance plausibly (%f -> %f)", name, before, after]);
    recordCheck(name, @{@"before_seconds": @(before), @"after_seconds": @(after)});
    return after;
}
static double assertPaused(NSAppleEventDescriptor *doc, NSString *name) {
    waitState(doc, NO);
    [NSThread sleepForTimeInterval:0.15];
    double before = position(doc);
    [NSThread sleepForTimeInterval:0.8];
    double after = position(doc);
    probeRequire(!playing(doc) && fabs(after - before) < 0.10,
        [NSString stringWithFormat:@"%@: paused position moved (%f -> %f)", name, before, after]);
    recordCheck(name, @{@"before_seconds": @(before), @"after_seconds": @(after)});
    return after;
}
static NSAppleEventDescriptor *openFile(NSString *path, NSInteger expectedCount) {
    NSAppleEventDescriptor *files = [NSAppleEventDescriptor listDescriptor];
    [files insertDescriptor:[NSAppleEventDescriptor descriptorWithFileURL:[NSURL fileURLWithPath:path]] atIndex:1];
    sendAppleEvent(kCoreEventClass, kAEOpenDocuments, files, NO);
    for (int i = 0; i < 30; ++i) {
        NSAppleEventDescriptor *docs = documents();
        if (docs.numberOfItems == expectedCount) return docs;
        [NSThread sleepForTimeInterval:0.1];
    }
    probeRequire(NO, @"Expected document did not open");
    return nil;
}
static void runProbe(void) {
    probeRequire([NSRunningApplication runningApplicationsWithBundleIdentifier:@"com.apple.QuickTimePlayerX"].count == 0,
        @"Fresh-VM requirement failed: QuickTime was already running; nothing was altered");
    dispatch_semaphore_t launched = dispatch_semaphore_create(0);
    __block NSError *launchError = nil;
    NSWorkspaceOpenConfiguration *launch = [NSWorkspaceOpenConfiguration configuration];
    launch.activates = NO;
    dispatch_async(dispatch_get_main_queue(), ^{
        [[NSWorkspace sharedWorkspace] openApplicationAtURL:
            [NSURL fileURLWithPath:@"/System/Applications/QuickTime Player.app"]
            configuration:launch completionHandler:^(NSRunningApplication *app, NSError *error) {
                player = app; launchError = error; dispatch_semaphore_signal(launched);
            }];
    });
    probeRequire(dispatch_semaphore_wait(launched, dispatch_time(DISPATCH_TIME_NOW, 15 * NSEC_PER_SEC)) == 0,
        @"QuickTime launch timeout");
    probeRequire(player != nil, [NSString stringWithFormat:@"QuickTime launch failed: %@", launchError]);
    ownsPlayer = YES;
    started = player.launchDate;
    live();
    target = [NSAppleEventDescriptor descriptorWithProcessIdentifier:player.processIdentifier];
    report[@"player_pid"] = @(player.processIdentifier);
    report[@"player_launch_epoch"] = @(started.timeIntervalSince1970);
    report[@"phase"] = @"requesting_exact_quicktime_consent";
    save();

    // This opt-in hosted diagnostic alone may ask. Production must not infer
    // that asking during recording is acceptable from this fixture.
    OSStatus consent = AEDeterminePermissionToAutomateTarget(target.aeDesc,
        typeWildCard, typeWildCard, true);
    report[@"consent_status"] = @(consent);
    probeRequire(consent == noErr, [NSString stringWithFormat:@"QuickTime TCC consent unavailable: %d", (int)consent]);
    probeRequire(AEDeterminePermissionToAutomateTarget(target.aeDesc,
        typeWildCard, typeWildCard, false) == noErr, @"Consent not retained");
    report[@"phase"] = @"measuring_real_player";
    save();

    probeRequire(documents().numberOfItems == 0, @"Unexpected preexisting QuickTime documents");
    probeRequire(playbackCommands == 0, @"No-media probe unexpectedly sent a playback command");
    recordCheck(@"no_media", @{@"documents": @0, @"play_or_pause_commands_sent": @(playbackCommands)});
    NSAppleEventDescriptor *a = freeze([openFile(cfg[@"file_a"], 1) descriptorAtIndex:1]);
    NSString *aID = identity(a);
    command(@"play", a);
    assertAdvancing(a, @"explicit_play_advances");
    command(@"pause", a);
    assertPaused(a, @"explicit_pause_stops");

    NSAppleEventDescriptor *both = openFile(cfg[@"file_b"], 2);
    NSAppleEventDescriptor *b = nil;
    for (NSInteger index = 1; index <= both.numberOfItems; ++index) {
        NSAppleEventDescriptor *candidate = freeze([both descriptorAtIndex:index]);
        if (![identity(candidate) isEqualToString:aID]) {
            probeRequire(b == nil, @"More than one unexpected document");
            b = candidate;
        }
    }
    probeRequire(b != nil, @"Second document did not have distinct identity");
    NSString *bID = identity(b);
    command(@"pause", b);
    assertPaused(b, @"second_document_paused");
    probeRequire(!playing(a), @"Opening the second document unexpectedly resumed the first");
    double bBefore = position(b);
    command(@"play", a); // Frozen formUniqueID, never document 1 or global toggle.
    assertAdvancing(a, @"same_document_resume_advances");
    double bAfter = position(b);
    probeRequire(!playing(b) && fabs(bAfter - bBefore) < 0.10 &&
        [identity(a) isEqualToString:aID] && [identity(b) isEqualToString:bID],
        @"Resume affected a different document or document identity changed");
    recordCheck(@"same_document_identity", @{@"resumed_id": aID, @"untouched_id": bID,
        @"other_before_seconds": @(bBefore), @"other_after_seconds": @(bAfter)});
    command(@"pause", a);
    assertPaused(a, @"cleanup_pause");
    // Also exercise a non-first document explicitly: a hard-coded document 1
    // implementation cannot satisfy this assertion and the untouched guard.
    NSAppleEventDescriptor *ordered = documents();
    probeRequire(ordered.numberOfItems == 2, @"Unexpected document count before index-isolation case");
    NSAppleEventDescriptor *nonFirst = freeze([ordered descriptorAtIndex:2]);
    NSAppleEventDescriptor *first = freeze([ordered descriptorAtIndex:1]);
    NSString *savedID = identity(nonFirst);
    probeRequire(!playing(first) && !playing(nonFirst), @"Index-isolation setup was not paused");
    double firstBefore = position(first);
    command(@"play", nonFirst);
    assertAdvancing(nonFirst, @"nonfirst_document_play_advances");
    command(@"pause", nonFirst);
    assertPaused(nonFirst, @"nonfirst_document_pause_stops");
    probeRequire(![identity(freeze([documents() descriptorAtIndex:1])) isEqualToString:savedID],
        @"Fixture ordering changed: non-first identity case is not established");
    command(@"play", nonFirst);
    assertAdvancing(nonFirst, @"nonfirst_document_resume_advances");
    probeRequire([identity(nonFirst) isEqualToString:savedID] && !playing(first) &&
        fabs(position(first) - firstBefore) < 0.10, @"Resume affected document 1");
    recordCheck(@"nonfirst_document_identity", @{@"resumed_id": savedID, @"untouched_id": identity(first)});
    command(@"pause", nonFirst);
    report[@"status"] = @"passed";
    report[@"phase"] = @"complete";
}
int main(int argc, const char **argv) {
    @autoreleasepool {
        if (argc != 2) return 64;
        NSDictionary *env = NSProcessInfo.processInfo.environment;
        if (![env[@"GITHUB_ACTIONS"] isEqual:@"true"] ||
            ![env[@"RUNNER_ENVIRONMENT"] isEqual:@"github-hosted"] ||
            ![env[@"RUNNER_OS"] isEqual:@"macOS"] ||
            ![env[@"HANDY_RUN_ISOLATED_QUICKTIME_TESTS"] isEqual:@"1"]) return 65;
        NSData *input = [NSData dataWithContentsOfFile:@(argv[1])];
        cfg = [NSJSONSerialization JSONObjectWithData:input options:0 error:NULL];
        if (![cfg isKindOfClass:NSDictionary.class]) return 66;
        receipt = cfg[@"receipt"];
        checks = [NSMutableArray array];
        report = [@{@"status": @"running", @"phase": @"initializing",
            @"scope": @"Public PID-addressed Apple Events and actual QuickTime only; not Handy backend coverage",
            @"probe_pid": @(getpid()), @"os": NSProcessInfo.processInfo.operatingSystemVersionString,
            @"codes": cfg[@"codes"], @"sdef_sha256": cfg[@"sdef_sha256"]} mutableCopy];
        [NSApplication sharedApplication];
        [NSApp setActivationPolicy:NSApplicationActivationPolicyAccessory];
        dispatch_async(dispatch_get_global_queue(QOS_CLASS_USER_INITIATED, 0), ^{
            @autoreleasepool {
                @try { save(); runProbe(); }
                @catch (NSException *error) {
                    report[@"status"] = @"failed";
                    report[@"error"] = error.reason ?: error.name;
                }
                @finally {
                    if (ownsPlayer && player && !player.terminated) {
                        [player terminate];
                        for (int i = 0; i < 20 && !player.terminated; ++i)
                            [NSThread sleepForTimeInterval:0.1];
                        if (!player.terminated) [player forceTerminate];
                        for (int i = 0; i < 20 && !player.terminated; ++i)
                            [NSThread sleepForTimeInterval:0.1];
                        report[@"cleanup_player_terminated"] = @(player.terminated);
                        if (!player.terminated) {
                            report[@"status"] = @"failed";
                            report[@"cleanup_error"] = @"Owned QuickTime process did not terminate";
                        }
                    }
                    @try { save(); } @catch (NSException *error) { NSLog(@"%@", error); }
                    dispatch_async(dispatch_get_main_queue(), ^{ [NSApp terminate:nil]; });
                }
            }
        });
        [NSApp run];
    }
    return 0; // Python checks the durable receipt, never just this exit code.
}
'''

CONSENT_SCRIPT = r'''
with timeout of 4 seconds
    tell application "System Events"
        repeat with proc in application processes
            if name of proc is in {"CoreServicesUIAgent", "UserNotificationCenter", "Handy Media Pause Probe", "QuickTime Player", "NotificationCenter"} then
                repeat with w in windows of proc
                    set allText to ""
                    set candidates to {}
                    try
                        set contentsList to entire contents of w
                        repeat with itemRef in contentsList
                            try
                                if role of itemRef is "AXStaticText" then set allText to allText & " " & (value of itemRef as text)
                                if role of itemRef is "AXButton" then
                                    if name of itemRef is in {"Allow", "OK"} then set end of candidates to itemRef
                                end if
                            end try
                        end repeat
                    end try
                    if allText contains "Handy Media Pause Probe" and allText contains "QuickTime Player" then
                        if (count of candidates) is 1 then
                            click item 1 of candidates
                            return "clicked_exact_quicktime_consent"
                        end if
                        return "exact_dialog_found_but_no_unique_allow_button"
                    end if
                end repeat
            end if
        end repeat
    end tell
end timeout
return "no_exact_dialog"
'''


def run(args, *, timeout=30):
    try:
        return subprocess.run(args, check=True, capture_output=True, text=True, timeout=timeout)
    except subprocess.CalledProcessError as error:
        # All commands here are compiler/OS fixture commands without credentials.
        # Preserve diagnostics instead of hiding clang errors in captured stderr.
        if error.stdout:
            print(error.stdout, file=sys.stderr, end="")
        if error.stderr:
            print(error.stderr, file=sys.stderr, end="")
        raise


def fourcc(value):
    raw = value.encode("mac_roman")
    if len(raw) != 4:
        raise RuntimeError(f"Expected four-byte dictionary code, got {value!r}")
    return int.from_bytes(raw, "big")


def dictionary_codes():
    data = run(["/usr/bin/sdef", "/System/Applications/QuickTime Player.app"]).stdout
    root = ET.fromstring(data)
    # sdef may preserve XInclude references to the OS-owned standard dictionary.
    roots = [root]
    standard = Path("/System/Library/ScriptingDefinitions/CocoaStandard.sdef")
    if standard.exists():
        roots.append(ET.parse(standard).getroot())

    def unique(tag, name):
        values = {e.attrib["code"] for tree in roots for e in tree.iter(tag)
                  if e.attrib.get("name", "").casefold() == name and "code" in e.attrib}
        if len(values) != 1:
            raise RuntimeError(f"Unsupported/ambiguous installed dictionary {tag} {name}: {values}")
        return values.pop()

    codes = {"document": fourcc(unique("class", "document"))}
    # Follow only document definitions, extensions and their declared ancestors.
    # Do not borrow a same-named property from an unrelated window/player class.
    document_classes = {"document"}
    nodes = [node for tree in roots for node in tree.iter()
             if node.tag in ("class", "class-extension")]
    while True:
        inherited = {node.attrib["inherits"] for node in nodes
                     if node.attrib.get("name") in document_classes and "inherits" in node.attrib}
        if inherited <= document_classes:
            break
        document_classes.update(inherited)
    relevant = [node for node in nodes
                if (node.tag == "class" and node.attrib.get("name") in document_classes)
                or (node.tag == "class-extension" and node.attrib.get("extends") in document_classes)]
    print("QuickTime document dictionary: " + json.dumps([
        {"tag": node.tag, "attributes": node.attrib,
         "properties": [prop.attrib for prop in node.findall("property")]}
        for node in relevant
    ], sort_keys=True), flush=True)
    properties = {}
    for node in relevant:
        for prop in node.findall("property"):
            if "name" in prop.attrib and "code" in prop.attrib:
                properties.setdefault(prop.attrib["name"].casefold(), set()).add(prop.attrib["code"])
    for out, name in (("playing", "playing"), ("current_time", "current time")):
        values = properties.get(name, set())
        if len(values) != 1:
            raise RuntimeError(f"Unsupported/ambiguous document property {name}: {values}")
        codes[out] = fourcc(values.pop())
    for command in ("play", "pause"):
        event = unique("command", command)
        if len(event) != 8:
            raise RuntimeError(f"Unsupported event code {command}: {event!r}")
        codes[command + "_class"], codes[command + "_id"] = fourcc(event[:4]), fourcc(event[4:])
    raw = data.encode() + (standard.read_bytes() if standard.exists() else b"")
    return codes, hashlib.sha256(raw).hexdigest()


def main():
    expected = {"GITHUB_ACTIONS": "true", "RUNNER_ENVIRONMENT": "github-hosted",
                "RUNNER_OS": "macOS", "HANDY_RUN_ISOLATED_QUICKTIME_TESTS": "1"}
    if sys.platform != "darwin" or any(os.environ.get(k) != v for k, v in expected.items()):
        raise SystemExit("REFUSED: fresh GitHub-hosted macOS VM and explicit opt-in required")
    runner_temp = Path(os.environ["RUNNER_TEMP"]).resolve(strict=True)
    receipt = runner_temp / "quicktime-probe-result.json"
    if receipt.exists():
        raise SystemExit(f"REFUSED: receipt already exists, choose a fresh VM/job: {receipt}")
    codes, digest = dictionary_codes()
    with tempfile.TemporaryDirectory(prefix="handy-quicktime-probe-", dir=runner_temp) as tmp:
        folder = Path(tmp)
        app = folder / "Handy Media Pause Probe.app"
        binary = app / "Contents/MacOS/Handy Media Pause Probe"
        binary.parent.mkdir(parents=True)
        plist = {"CFBundleIdentifier": "com.qqrm.handy.MediaPauseProbe",
                 "CFBundleExecutable": binary.name, "CFBundleName": "Handy Media Pause Probe",
                 "CFBundleDisplayName": "Handy Media Pause Probe", "CFBundlePackageType": "APPL",
                 "CFBundleVersion": "1", "LSUIElement": True,
                 "NSAppleEventsUsageDescription": "Validate QuickTime playback on this isolated GitHub runner."}
        (app / "Contents/Info.plist").write_bytes(plistlib.dumps(plist))
        entitlements = folder / "entitlements.plist"
        entitlements.write_bytes(plistlib.dumps({"com.apple.security.automation.apple-events": True}))
        source = folder / "probe.m"
        source.write_text(NATIVE, encoding="utf-8")
        run(["/usr/bin/xcrun", "clang", "-fobjc-arc", "-Wall", "-Wextra",
             "-framework", "Cocoa", "-framework", "ApplicationServices", "-framework", "CoreServices", str(source),
             "-o", str(binary)], timeout=120)
        run(["/usr/bin/codesign", "--force", "--sign", "-", "--options", "runtime",
             "--entitlements", str(entitlements), str(app)])
        for name, sample in (("a.wav", 1000), ("b.wav", -1000)):
            with wave.open(str(folder / name), "wb") as audio:
                audio.setnchannels(1)
                audio.setsampwidth(2)
                audio.setframerate(8000)
                # Quiet alternating PCM, 90 seconds; no external file/download.
                audio.writeframes((struct.pack("<hh", sample, -sample) * 4000) * 90)
        config = folder / "config.json"
        config.write_text(json.dumps({"file_a": str(folder / "a.wav"), "file_b": str(folder / "b.wav"),
                                      "codes": codes, "sdef_sha256": digest, "receipt": str(receipt)}))
        consent_path = folder / "consent.applescript"
        consent_path.write_text(CONSENT_SCRIPT, encoding="utf-8")
        # LaunchServices does not reliably inherit the workflow shell environment.
        launch_args = ["/usr/bin/open", "-n", "-W"]
        for key, value in expected.items():
            launch_args.extend(["--env", f"{key}={value}"])
        launch_args.extend([str(app), "--args", str(config)])
        launcher = subprocess.Popen(launch_args, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        deadline = time.monotonic() + 110
        consent_deadline = time.monotonic() + 45
        clicks = []
        result = {}
        process_starts = {}
        try:
            while time.monotonic() < deadline:
                if receipt.exists():
                    result = json.loads(receipt.read_text())
                    for field in ("probe_pid", "player_pid"):
                        pid = result.get(field)
                        if isinstance(pid, int) and pid > 1 and pid not in process_starts:
                            stamp = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                                   capture_output=True, text=True, timeout=4)
                            if stamp.returncode == 0 and stamp.stdout.strip():
                                process_starts[pid] = stamp.stdout.strip()
                    if result.get("status") in ("passed", "failed"):
                        break
                    if (result.get("phase") == "requesting_exact_quicktime_consent"
                            and time.monotonic() < consent_deadline):
                        try:
                            outcome = run(["/usr/bin/osascript", str(consent_path)], timeout=6).stdout.strip()
                            if outcome != "no_exact_dialog":
                                clicks.append(outcome)
                        except (subprocess.SubprocessError, OSError) as exc:
                            clicks.append(f"consent_ui_unavailable:{type(exc).__name__}")
                if launcher.poll() is not None:
                    # Receipt is authoritative; successful open(1) is not proof.
                    if receipt.exists():
                        result = json.loads(receipt.read_text())
                    break
                time.sleep(0.4)
            if result.get("status") not in ("passed", "failed"):
                result.update(status="failed", error="Bounded probe/consent timeout or missing final receipt")
            result["consent_ui_diagnostics"] = clicks
            result["machine_architecture"] = run(["/usr/bin/uname", "-m"]).stdout.strip()
            result["limitations"] = [
                "Real QuickTime transport fixture only; no Handy product backend executed.",
                "Not proof of all-player support or lossless detection of user play-pause overrides.",
                "PID plus launch identity reduces restart retargeting; checks are not an atomic OS session capability.",
            ]
        finally:
            try:
                out, err = launcher.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                out, err = "", ""
                if result.get("status") == "passed":
                    result.update(status="failed", error="Probe did not exit after its final receipt")
            # On a hung permission dialog kill only the recorded, exact executable
            # PID and observed start time owned by this fixture. No name-wide kill.
            for field, expected_path in (("probe_pid", str(binary)),
                                         ("player_pid", "/System/Applications/QuickTime Player.app/Contents/MacOS/QuickTime Player")):
                pid = result.get(field)
                if isinstance(pid, int) and pid > 1:
                    ps = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "comm="],
                                        capture_output=True, text=True, timeout=4)
                    stamp = subprocess.run(["/bin/ps", "-p", str(pid), "-o", "lstart="],
                                           capture_output=True, text=True, timeout=4)
                    if (ps.returncode == 0 and ps.stdout.strip() == expected_path
                            and stamp.returncode == 0 and pid in process_starts
                            and stamp.stdout.strip() == process_starts[pid]):
                        try:
                            os.kill(pid, signal.SIGTERM)
                        except ProcessLookupError:
                            pass
            if launcher.poll() is None:
                launcher.terminate()
            try:
                out, err = launcher.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                launcher.kill()
                out, err = launcher.communicate(timeout=5)
            result["launcher_exit_code"] = launcher.returncode
            if err.strip():
                result["launcher_stderr"] = err.strip()[-3000:]
            if result.get("status") == "passed" and launcher.returncode != 0:
                result.update(status="failed", error="Probe launcher exited unsuccessfully")
            receipt.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(result, indent=2))
        if result.get("status") != "passed":
            raise SystemExit(1)


if __name__ == "__main__":
    main()

