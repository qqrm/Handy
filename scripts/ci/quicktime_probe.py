#!/usr/bin/env python3
"""Hosted-only real QuickTime test of the production Rust controller and AE shim.

Run on a fresh standard macos-15-intel or macos-26 GitHub VM:
  HANDY_RUN_ISOLATED_QUICKTIME_TESTS=1 python3 scripts/ci/quicktime_probe.py

Requires Xcode command-line tools. Writes quicktime-probe-result.json in
RUNNER_TEMP. No network media, private frameworks, input injection, global
play/pause keys, or TCC database changes. The exact QuickTime consent dialog
may be accepted on this isolated VM. Denial/timeouts fail, never skip.

QuickTime returns name-based document references. Playback targets here are
instead the unevaluated document property of a unique-ID window, guarded by
the original process launch and unchanged file URL. This is window identity,
not a universal media-session token; same-file replacement inside a surviving
window and unobserved user Play/Pause overrides are outside this diagnostic.
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
#import <signal.h>

extern int handy_fixture_permission(void);
extern void *handy_fixture_new(void);
extern void handy_fixture_begin(void *, uint64_t);
extern void handy_fixture_end(void *);
extern void handy_fixture_destroy(void *);
extern void handy_qt_fixture_stop_before_command(int);
extern int handy_qt_fixture_command_count(void);
extern void *handy_qt_fixture_permission_diagnostics(void);

static NSDictionary *cfg;
static NSMutableDictionary *report;
static NSMutableArray *checks;
static NSRunningApplication *player;
static NSDate *started;
static NSAppleEventDescriptor *target;
static NSString *receipt;
static BOOL ownsPlayer = NO;
static NSInteger playbackCommands = 0;
static void *controller = NULL;

@interface ProbeTarget : NSObject
@property(nonatomic, strong) NSAppleEventDescriptor *window;
@property(nonatomic, strong) NSAppleEventDescriptor *document;
@property(nonatomic, strong) NSAppleEventDescriptor *windowID;
@property(nonatomic, strong) NSURL *file;
@end
@implementation ProbeTarget
@end

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
static NSAppleEventDescriptor *objects(OSType objectClass) {
    OSType ordinal = kAEAll;
    NSAppleEventDescriptor *list = get(spec(objectClass, nil, formAbsolutePosition,
        [NSAppleEventDescriptor descriptorWithDescriptorType:typeAbsoluteOrdinal
            bytes:&ordinal length:sizeof(ordinal)]));
    probeRequire(list.descriptorType == typeAEList, @"Object enumeration reply is not a list");
    return list;
}
static NSAppleEventDescriptor *documents(void) { return objects(code(@"document")); }
static NSAppleEventDescriptor *windowIdentity(NSAppleEventDescriptor *window) {
    NSAppleEventDescriptor *resolved = get(window);
    report[@"last_window_reference"] = resolved.description ?: @"(no descriptor description)";
    probeRequire(resolved.descriptorType == typeObjectSpecifier,
        @"QuickTime did not return a window object specifier");
    NSAppleEventDescriptor *record = [resolved coerceToDescriptorType:typeAERecord];
    NSAppleEventDescriptor *kind = [record descriptorForKeyword:keyAEDesiredClass];
    NSAppleEventDescriptor *form = [record descriptorForKeyword:keyAEKeyForm];
    NSAppleEventDescriptor *container = [record descriptorForKeyword:keyAEContainer];
    NSAppleEventDescriptor *identity = [record descriptorForKeyword:keyAEKeyData];
    probeRequire(record && kind.descriptorType == typeType && kind.typeCodeValue == code(@"window") &&
        form.descriptorType == typeEnumerated && form.enumCodeValue == formUniqueID &&
        container && container.descriptorType == typeNull && identity &&
        identity.descriptorType != typeNull && identity.data.length > 0,
        @"QuickTime window lacks formUniqueID identity; no name/index fallback");
    return identity;
}
static BOOL sameDescriptor(NSAppleEventDescriptor *a, NSAppleEventDescriptor *b) {
    return a && b && a.descriptorType == b.descriptorType && [a.data isEqual:b.data];
}
static NSAppleEventDescriptor *canonicalInteger(NSAppleEventDescriptor *value) {
    // Do not accept strings, floats or lossy narrowing. Apple Events may encode
    // the same positive window ID using different signed integer widths.
    DescType kind = value.descriptorType;
    probeRequire(value && (kind == typeSInt16 || kind == typeSInt32 ||
        kind == typeUInt32 || kind == typeSInt64), @"Window ID is not a supported integral descriptor");
    NSAppleEventDescriptor *canonical = [value coerceToDescriptorType:typeSInt64];
    probeRequire(canonical && canonical.data.length == sizeof(SInt64), @"Window ID could not be normalized losslessly");
    return canonical;
}
static NSAppleEventDescriptor *windowDocument(NSAppleEventDescriptor *window) {
    // Keep the property specifier unevaluated. Resolving it and using the reply
    // as a command target would turn it back into a mutable document name.
    return spec(cProperty, window, formPropertyID,
        [NSAppleEventDescriptor descriptorWithTypeCode:code(@"window_document")]);
}
static NSURL *documentFile(NSAppleEventDescriptor *doc) {
    NSAppleEventDescriptor *value = [property(doc, code(@"file")) coerceToDescriptorType:typeFileURL];
    NSURL *url = value.fileURLValue;
    probeRequire(value && url && url.isFileURL, @"Document has no concrete local file URL");
    return url.URLByStandardizingPath;
}
static void guardTarget(ProbeTarget *target) {
    probeRequire(sameDescriptor(windowIdentity(target.window), target.windowID), @"Window identity changed");
    probeRequire([documentFile(target.document) isEqual:target.file], @"Window document/file association changed");
}
static ProbeTarget *freeze(NSAppleEventDescriptor *doc) {
    report[@"last_document_reference"] = get(doc).description ?: @"(no descriptor description)";
    NSURL *file = documentFile(doc); // Read-only discovery, never a playback target.
    NSAppleEventDescriptor *windows = objects(code(@"window"));
    ProbeTarget *found = nil;
    for (NSInteger index = 1; index <= windows.numberOfItems; ++index) {
        NSAppleEventDescriptor *window = [windows descriptorAtIndex:index];
        NSAppleEventDescriptor *windowID = windowIdentity(window);
        NSAppleEventDescriptor *declaredID = property(window, code(@"window_id"));
        report[@"window_id_reference"] = @{@"descriptor": windowID.description ?: @"nil",
            @"type": @(windowID.descriptorType)};
        report[@"window_id_property"] = @{@"descriptor": declaredID.description ?: @"nil",
            @"type": @(declaredID.descriptorType)};
        probeRequire(sameDescriptor(canonicalInteger(windowID), canonicalInteger(declaredID)),
            @"Window ID property disagrees with its returned unique-ID specifier");
        NSAppleEventDescriptor *frozen = spec(code(@"window"), nil, formUniqueID, windowID);
        NSAppleEventDescriptor *association = windowDocument(frozen);
        if ([documentFile(association) isEqual:file]) {
            probeRequire(found == nil, @"More than one window owns the fixture file; target is ambiguous");
            found = [ProbeTarget new];
            found.window = frozen;
            found.document = association;
            found.windowID = windowID;
            found.file = file;
            guardTarget(found);
        }
    }
    probeRequire(found != nil, @"No uniquely identified window owns the fixture document");
    return found;
}
static NSString *identity(ProbeTarget *target) {
    guardTarget(target);
    return [NSString stringWithFormat:@"window:%08x:%@|file:%@", (unsigned)target.windowID.descriptorType,
        [target.windowID.data base64EncodedStringWithOptions:0], target.file.absoluteString];
}
static NSAppleEventDescriptor *targetProperty(ProbeTarget *target, OSType propertyCode) {
    guardTarget(target);
    NSAppleEventDescriptor *value = property(target.document, propertyCode);
    guardTarget(target);
    return value;
}
static BOOL playing(ProbeTarget *doc) {
    NSAppleEventDescriptor *value = [targetProperty(doc, code(@"playing")) coerceToDescriptorType:typeBoolean];
    probeRequire(value != nil, @"Playing property is not boolean");
    return value.booleanValue;
}
static double position(ProbeTarget *doc) {
    NSAppleEventDescriptor *value = [targetProperty(doc, code(@"current_time"))
        coerceToDescriptorType:typeIEEE64BitFloatingPoint];
    probeRequire(value != nil && isfinite(value.doubleValue), @"Current time is not finite numeric data");
    return value.doubleValue;
}
static void command(NSString *name, ProbeTarget *doc) {
    guardTarget(doc);
    playbackCommands += 1;
    sendAppleEvent(code([name stringByAppendingString:@"_class"]), code([name stringByAppendingString:@"_id"]), doc.document, NO);
    guardTarget(doc);
}
static void waitState(ProbeTarget *doc, BOOL expected) {
    for (int i = 0; i < 30; ++i) {
        if (playing(doc) == expected) return;
        [NSThread sleepForTimeInterval:0.1];
    }
    probeRequire(NO, @"Player did not acknowledge requested playback state");
}
static double assertAdvancing(ProbeTarget *doc, NSString *name) {
    waitState(doc, YES);
    double began = NSProcessInfo.processInfo.systemUptime;
    double before = position(doc);
    probeRequire(before >= 0, @"Playback position is negative");
    double after = before, previous = before, elapsed = 0;
    NSUInteger samples = 0;
    do {
        [NSThread sleepForTimeInterval:0.1];
        after = position(doc);
        BOOL isPlaying = playing(doc);
        elapsed = NSProcessInfo.processInfo.systemUptime - began;
        ++samples;
        double delta = after - before;
        probeRequire(isPlaying && after >= previous && delta < 6.0 && delta <= 2.0 * elapsed + 0.25,
            [NSString stringWithFormat:@"%@: playback stopped, reversed, or jumped (%f -> %f in %fs)",
                name, before, after, elapsed]);
        // Startup after seeking may stall briefly. Require real clock progress
        // within a bounded deadline, not a particular instantaneous play rate.
        // Keep it observable for multiple controller polls in override cases.
        if (delta >= 0.35 && elapsed >= 0.75 && elapsed <= 5.0) {
            recordCheck(name, @{@"before_seconds": @(before), @"after_seconds": @(after),
                @"elapsed_seconds": @(elapsed), @"samples": @(samples)});
            return after;
        }
        previous = after;
    } while (elapsed < 5.0);
    probeRequire(NO, [NSString stringWithFormat:@"%@: playback did not advance by 0.35s within 5s (%f -> %f)",
        name, before, after]);
    return after;
}
static double assertPaused(ProbeTarget *doc, NSString *name) {
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
    int consent = handy_fixture_permission(); // Exact production settings function.
    report[@"consent_status"] = @(consent);
    report[@"consent_native_diagnostics"] = CFBridgingRelease(handy_qt_fixture_permission_diagnostics());
    probeRequire(consent == 1, [NSString stringWithFormat:@"QuickTime TCC consent unavailable: %d", consent]);
    probeRequire(AEDeterminePermissionToAutomateTarget(target.aeDesc,
        typeWildCard, typeWildCard, false) == noErr, @"Consent not retained");
    report[@"phase"] = @"measuring_real_player";
    save();

    probeRequire(documents().numberOfItems == 0, @"Unexpected preexisting QuickTime documents");
    controller = handy_fixture_new();
    probeRequire(controller != NULL, @"Production controller refused isolated fixture");
    int emptyCommands = handy_qt_fixture_command_count();
    handy_fixture_begin(controller, 1);
    [NSThread sleepForTimeInterval:1.0];
    handy_fixture_end(controller);
    probeRequire(handy_qt_fixture_command_count() == emptyCommands,
        @"Production controller sent a command without media");
    probeRequire(playbackCommands == 0, @"No-media probe unexpectedly sent a playback command");
    recordCheck(@"no_media", @{@"documents": @0, @"play_or_pause_commands_sent": @(playbackCommands)});
    ProbeTarget *a = freeze([openFile(cfg[@"file_a"], 1) descriptorAtIndex:1]);
    NSString *aID = identity(a);
    command(@"play", a);
    assertAdvancing(a, @"explicit_play_advances");
    command(@"pause", a);
    assertPaused(a, @"explicit_pause_stops");

    NSAppleEventDescriptor *both = openFile(cfg[@"file_b"], 2);
    ProbeTarget *b = nil;
    for (NSInteger index = 1; index <= both.numberOfItems; ++index) {
        ProbeTarget *candidate = freeze([both descriptorAtIndex:index]);
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
    command(@"play", a); // Document of frozen window ID, never document 1 or a global toggle.
    assertAdvancing(a, @"same_document_resume_advances");
    double bAfter = position(b);
    probeRequire(!playing(b) && fabs(bAfter - bBefore) < 0.10 &&
        [identity(a) isEqualToString:aID] && [identity(b) isEqualToString:bID],
        @"Resume affected a different document or document identity changed");
    recordCheck(@"same_window_file_association", @{@"resumed_id": aID, @"untouched_id": bID,
        @"other_before_seconds": @(bBefore), @"other_after_seconds": @(bAfter)});
    command(@"pause", a);
    assertPaused(a, @"cleanup_pause");
    // Also exercise a non-first document explicitly: a hard-coded document 1
    // implementation cannot satisfy this assertion and the untouched guard.
    NSAppleEventDescriptor *ordered = documents();
    probeRequire(ordered.numberOfItems == 2, @"Unexpected document count before index-isolation case");
    ProbeTarget *nonFirst = freeze([ordered descriptorAtIndex:2]);
    ProbeTarget *first = freeze([ordered descriptorAtIndex:1]);
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
    recordCheck(@"nonfirst_window_file_association", @{@"resumed_id": savedID, @"untouched_id": identity(first)});
    command(@"pause", nonFirst);
    // Closing and reopening the same file must not let the stale window target
    // resume its replacement, even though both the document name and URL match.
    sendAppleEvent(kAECoreSuite, kAEClose, nonFirst.window, NO);
    for (int i = 0; i < 30 && documents().numberOfItems != 1; ++i)
        [NSThread sleepForTimeInterval:0.1];
    probeRequire(documents().numberOfItems == 1, @"Old window did not close");
    NSAppleEventDescriptor *reopened = openFile(nonFirst.file.path, 2);
    ProbeTarget *replacement = nil;
    for (NSInteger index = 1; index <= reopened.numberOfItems; ++index) {
        ProbeTarget *candidate = freeze([reopened descriptorAtIndex:index]);
        if ([candidate.file isEqual:nonFirst.file]) replacement = candidate;
    }
    probeRequire(replacement && !sameDescriptor(replacement.windowID, nonFirst.windowID),
        @"Reopened fixture did not obtain a distinct window identity");
    NSInteger beforeRejectedCommand = playbackCommands;
    BOOL rejected = NO;
    NSString *rejection = nil;
    @try { command(@"play", nonFirst); }
    @catch (NSException *error) { rejected = YES; rejection = error.reason; }
    probeRequire(rejected && playbackCommands == beforeRejectedCommand,
        @"Stale target was not rejected before sending playback");
    assertPaused(replacement, @"replacement_remains_paused");
    recordCheck(@"closed_window_target_rejected", @{@"old_window_id": savedID,
        @"replacement_id": identity(replacement), @"reason": rejection ?: @"unknown"});

    // From this point all recording Pause/Resume operations use the actual Rust
    // controller+adapter linked into this app. Direct Apple Events only establish
    // independent fixture state, read clocks, or simulate a user override.
    ProbeTarget *active = replacement;
    ProbeTarget *untouched = first;
    probeRequire(![identity(active) isEqualToString:identity(untouched)], @"Fixture identities overlap");
    command(@"pause", untouched);
    double untouchedBefore = position(untouched);
    command(@"play", active);
    assertAdvancing(active, @"production_setup_playing");
    int before = handy_qt_fixture_command_count();
    handy_fixture_begin(controller, 2);
    assertPaused(active, @"production_recording_pauses_same_window");
    probeRequire(handy_qt_fixture_command_count() == before + 1 && !playing(untouched),
        @"Production pause did not target exactly the playing window");
    handy_fixture_end(controller);
    assertAdvancing(active, @"production_end_resumes_same_window");
    probeRequire(handy_qt_fixture_command_count() == before + 2 && !playing(untouched) &&
        fabs(position(untouched) - untouchedBefore) < 0.10, @"Production resume altered another document");
    recordCheck(@"production_other_document_untouched", @{@"id": identity(untouched)});

    handy_fixture_begin(controller, 3);
    assertPaused(active, @"production_override_initial_pause");
    command(@"play", active);
    assertAdvancing(active, @"user_resume_observable");
    command(@"pause", active);
    assertPaused(active, @"user_pause_after_resume");
    before = handy_qt_fixture_command_count();
    handy_fixture_end(controller);
    [NSThread sleepForTimeInterval:0.6];
    assertPaused(active, @"production_observed_override_not_resumed");
    probeRequire(handy_qt_fixture_command_count() == before, @"Production resumed after observed user override");

    command(@"play", active);
    assertAdvancing(active, @"seek_case_setup");
    handy_fixture_begin(controller, 4);
    assertPaused(active, @"seek_case_initial_pause");
    double seekTo = position(active) + 3.0;
    NSAppleEventDescriptor *setTime = [NSAppleEventDescriptor appleEventWithEventClass:kAECoreSuite
        eventID:kAESetData targetDescriptor:target returnID:kAutoGenerateReturnID transactionID:kAnyTransactionID];
    [setTime setParamDescriptor:spec(cProperty, active.document, formPropertyID,
        [NSAppleEventDescriptor descriptorWithTypeCode:code(@"current_time")]) forKeyword:keyDirectObject];
    [setTime setParamDescriptor:[NSAppleEventDescriptor descriptorWithDouble:seekTo] forKeyword:keyAEData];
    NSError *seekError = nil;
    NSAppleEventDescriptor *seekReply = [setTime sendEventWithOptions:NSAppleEventSendWaitForReply |
        NSAppleEventSendNeverInteract | (NSAppleEventSendOptions)kAEDoNotPromptForUserConsent timeout:4 error:&seekError];
    probeRequire(seekReply && !seekError && [seekReply paramDescriptorForKeyword:keyErrorNumber].int32Value == 0,
        @"Fixture user seek failed");
    [NSThread sleepForTimeInterval:0.7];
    probeRequire(fabs(position(active) - seekTo) < 0.2, @"Fixture seek was not applied");
    before = handy_qt_fixture_command_count();
    handy_fixture_end(controller);
    assertPaused(active, @"production_seek_override_not_resumed");
    probeRequire(handy_qt_fixture_command_count() == before, @"Production resumed after a paused seek");

    command(@"play", active);
    assertAdvancing(active, @"replacement_case_setup");
    handy_fixture_begin(controller, 5);
    assertPaused(active, @"replacement_case_initial_pause");
    NSString *oldID = identity(active);
    NSURL *sameFile = active.file;
    sendAppleEvent(kAECoreSuite, kAEClose, active.window, NO);
    probeRequire(documents().numberOfItems == 1, @"Production replacement window did not close");
    NSAppleEventDescriptor *newDocs = openFile(sameFile.path, 2);
    ProbeTarget *newWindow = nil;
    for (NSInteger i = 1; i <= newDocs.numberOfItems; ++i) {
        ProbeTarget *item = freeze([newDocs descriptorAtIndex:i]);
        if ([item.file isEqual:sameFile]) newWindow = item;
    }
    probeRequire(newWindow && ![identity(newWindow) isEqual:oldID], @"Replacement did not obtain new identity");
    before = handy_qt_fixture_command_count();
    handy_fixture_end(controller);
    assertPaused(newWindow, @"production_reopened_file_not_resumed");
    probeRequire(handy_qt_fixture_command_count() == before, @"Production retargeted a replacement window");

    // Deliberately stop THIS disposable QuickTime process after the production
    // preflight, immediately before its real AE send. The provider later handles
    // the queued Pause after its reply deadline. This hook is absent from the
    // application build; it does not replace transport or policy with a mock.
    active = newWindow;
    command(@"play", active);
    assertAdvancing(active, @"late_pause_case_setup");
    before = handy_qt_fixture_command_count();
    handy_qt_fixture_stop_before_command(player.processIdentifier);
    handy_fixture_begin(controller, 6);
    for (int i = 0; i < 60 && handy_qt_fixture_command_count() == before; ++i)
        [NSThread sleepForTimeInterval:0.05];
    probeRequire(handy_qt_fixture_command_count() == before + 1, @"Late Pause did not reach real transport");
    [NSThread sleepForTimeInterval:1.5];
    handy_fixture_end(controller);
    probeRequire(kill(player.processIdentifier, SIGCONT) == 0, @"Could not resume owned QuickTime fixture");
    assertPaused(active, @"production_late_pause_effect_observed");
    handy_fixture_begin(controller, 7);
    [NSThread sleepForTimeInterval:1.0];
    handy_fixture_end(controller);
    assertPaused(active, @"production_unknown_reply_not_retried_or_resumed");
    probeRequire(handy_qt_fixture_command_count() == before + 1, @"Uncertain operation was retried or compensated blindly");
    recordCheck(@"production_unknown_lease_retained", @{@"commands": @(handy_qt_fixture_command_count() - before),
        @"provider_delayed": @YES, @"expected_policy": @"quarantine until original process disappears"});
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
            @"scope": @"Actual production Rust controller, QuickTime adapter and public Apple Events against real QuickTime",
            @"identity_mode": @"PID + launch date + window unique ID + unchanged file URL",
            @"identity_limit": @"A guarded window/file association, not a universal media-session token; in-place replacement by the same file is not observable.",
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
                    if (ownsPlayer && player) (void)kill(player.processIdentifier, SIGCONT);
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
                    if (controller) { handy_fixture_destroy(controller); controller = NULL; }
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

    codes = {name: fourcc(unique("class", name)) for name in ("document", "window")}
    nodes = [node for tree in roots for node in tree.iter()
             if node.tag in ("class", "class-extension")]

    def class_properties(class_name):
        # Follow only this class's declared inheritance, never unrelated classes.
        classes = {class_name}
        while True:
            inherited = {node.attrib["inherits"] for node in nodes
                         if node.attrib.get("name") in classes and "inherits" in node.attrib}
            if inherited <= classes:
                break
            classes.update(inherited)
        relevant = [node for node in nodes
                    if (node.tag == "class" and node.attrib.get("name") in classes)
                    or (node.tag == "class-extension" and node.attrib.get("extends") in classes)]
        print(f"QuickTime {class_name} dictionary: " + json.dumps([
            {"tag": node.tag, "attributes": node.attrib,
             "properties": [prop.attrib for prop in node.findall("property")]}
            for node in relevant
        ], sort_keys=True), flush=True)
        properties = {}
        for node in relevant:
            for prop in node.findall("property"):
                if "name" in prop.attrib and "code" in prop.attrib:
                    properties.setdefault(prop.attrib["name"].casefold(), set()).add(prop.attrib["code"])
        return properties

    for class_name, requested in (
        ("document", (("playing", "playing"), ("current_time", "current time"), ("file", "file"))),
        ("window", (("window_id", "id"), ("window_document", "document"))),
    ):
        properties = class_properties(class_name)
        for out, name in requested:
            values = properties.get(name, set())
            if len(values) != 1:
                raise RuntimeError(f"Unsupported/ambiguous {class_name} property {name}: {values}")
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
    expected_codes = {"window": 1668770158, "document": 1685021557, "current_time": 1953066341,
                      "file": 1718185061, "playing": 1886151033, "window_id": 1229201440,
                      "window_document": 1685021557, "play_class": 1297504082,
                      "play_id": 1886151033, "pause_class": 1297504082, "pause_id": 1885435251}
    if codes != expected_codes:
        raise SystemExit("Installed QuickTime dictionary differs from the production adapter contract")
    repo = Path(__file__).resolve().parents[2]
    product_info = plistlib.loads((repo / "src-tauri/Info.plist").read_bytes())
    product_entitlements = plistlib.loads((repo / "src-tauri/Entitlements.plist").read_bytes())
    usage = product_info.get("NSAppleEventsUsageDescription")
    if not usage or product_entitlements.get("com.apple.security.automation.apple-events") is not True:
        raise SystemExit("Product Automation usage description or entitlement is absent")
    with tempfile.TemporaryDirectory(prefix="handy-quicktime-probe-", dir=runner_temp) as tmp:
        folder = Path(tmp)
        app = folder / "Handy Media Pause Probe.app"
        binary = app / "Contents/MacOS/Handy Media Pause Probe"
        binary.parent.mkdir(parents=True)
        plist = {"CFBundleIdentifier": "com.qqrm.handy.MediaPauseProbe",
                 "CFBundleExecutable": binary.name, "CFBundleName": "Handy Media Pause Probe",
                 "CFBundleDisplayName": "Handy Media Pause Probe", "CFBundlePackageType": "APPL",
                 "CFBundleVersion": "1", "LSUIElement": True,
                 "NSAppleEventsUsageDescription": usage}
        (app / "Contents/Info.plist").write_bytes(plistlib.dumps(plist))
        entitlements = folder / "entitlements.plist"
        entitlements.write_bytes(plistlib.dumps({"com.apple.security.automation.apple-events":
                                                product_entitlements["com.apple.security.automation.apple-events"]}))
        source = folder / "probe.m"
        source.write_text(NATIVE, encoding="utf-8")
        rust_target = folder / "rust-target"
        run(["cargo", "build", "--manifest-path", str(repo / "src-tauri/tests/media-pause/Cargo.toml"),
             "--locked", "--features", "quicktime-fixture", "--target-dir", str(rust_target)], timeout=300)
        production = rust_target / "debug/libhandy_media_pause_tests.a"
        if not production.is_file():
            raise SystemExit("Actual production controller fixture archive was not built")
        run(["/usr/bin/xcrun", "clang", "-fobjc-arc", "-Wall", "-Wextra",
             "-framework", "Cocoa", "-framework", "ApplicationServices", "-framework", "CoreServices", str(source),
             str(production),
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
        deadline = time.monotonic() + 160
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
            result["production_sources_sha256"] = {
                name: hashlib.sha256((repo / "src-tauri/src/managers" / name).read_bytes()).hexdigest()
                for name in ("media_pause.rs", "media_pause_macos.rs", "media_pause_macos.m")
            }
            result["fixture_injection"] = "Hosted-only SIGSTOP immediately before one real AE send; absent from product build"
            result["limitations"] = [
                "Actual production controller and QuickTime adapter; Tauri settings UI itself is not exercised here.",
                "Not proof of all-player support or lossless detection of user play-pause overrides.",
                "PID plus launch identity reduces restart retargeting; checks are not an atomic OS session capability.",
                "Window unique ID plus unchanged file URL is not a universal media-session token; same-file in-place replacement is not observable.",
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
                            if field == "player_pid":
                                os.kill(pid, signal.SIGCONT)
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

