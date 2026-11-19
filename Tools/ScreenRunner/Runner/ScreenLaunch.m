#import "ScreenLaunch.h"
#import <dlfcn.h>
#import <objc/message.h>
#import <UIKit/UIKit.h>

BOOL DSHLeaveForeground(void) {
    UIApplication *runner = UIApplication.sharedApplication;
    SEL suspend = NSSelectorFromString(@"suspend");
    if (runner.applicationState != UIApplicationStateActive || ![runner respondsToSelector:suspend]) return NO;
    ((void (*)(id, SEL))objc_msgSend)(runner, suspend);
    return YES;
}

// On iOS 26 XCTest can acknowledge its Home event while the runner stays
// foreground. Yield once during bootstrap, before XCTest enters the test case.
__attribute__((constructor)) static void DSHYieldRunnerDuringBootstrap(void) {
    if (NSProcessInfo.processInfo.operatingSystemVersion.majorVersion != 26) return;
    dispatch_after(dispatch_time(DISPATCH_TIME_NOW, 2 * NSEC_PER_SEC), dispatch_get_main_queue(), ^{
        DSHLeaveForeground();
    });
}

// XCTest records the screen and keeps screenshots for its own failure reports. This runner
// serves for hours and reports nothing, so the recordings would only pile up on the phone.
__attribute__((constructor)) static void DSHKeepNoDiagnostics(void) {
    [NSUserDefaults.standardUserDefaults registerDefaults:@{
        @"DisableDiagnosticScreenRecordings": @YES, @"DisableDiagnosticScreenshots": @YES,
    }];
}

BOOL DSHRequestScreenApp(XCUIApplication *app, NSString *bundleID) {
    @try {
        Class cls = NSClassFromString(@"LSApplicationWorkspace");
        SEL shared = NSSelectorFromString(@"defaultWorkspace");
        SEL open = NSSelectorFromString(@"openApplicationWithBundleID:");
        if ([cls respondsToSelector:shared]) {
            id workspace = ((id (*)(id, SEL))objc_msgSend)(cls, shared);
            if ([workspace respondsToSelector:open]) {
                return ((BOOL (*)(id, SEL, NSString *))objc_msgSend)(workspace, open, bundleID);
            }
        }
        [app activate];
        return YES;
    } @catch (NSException *exception) {
        return NO;
    }
}

void DSHLeaveSystemAlerts(XCUIApplication *app) {
    SEL set = NSSelectorFromString(@"setDoesNotHandleUIInterruptions:");
    if ([app respondsToSelector:set]) ((void (*)(id, SEL, BOOL))objc_msgSend)(app, set, YES);
}

void DSHPerformWithoutIdleWait(XCUIApplication *app, void (NS_NOESCAPE ^block)(void)) {
    // XCUIAutomation in Xcode 27 keeps these as XCUIApplicationInteractionOptionSkipPreEventQuiescence (1)
    // and ...SkipPostEventQuiescence (2). Other versions fall back to the ordinary, waiting event.
    SEL perform = NSSelectorFromString(@"_performWithInteractionOptions:block:");
    if ([app respondsToSelector:perform]) {
        ((void (*)(id, SEL, unsigned int, void (^)(void)))objc_msgSend)(app, perform, 3, block);
    } else {
        block();
    }
}

static id DSHGet(id object, NSString *name) {
    SEL sel = NSSelectorFromString(name);
    return [object respondsToSelector:sel] ? ((id (*)(id, SEL))objc_msgSend)(object, sel) : nil;
}

static BOOL DSHResponds(id object, NSArray<NSString *> *names) {
    for (NSString *name in names) {
        if (![object respondsToSelector:NSSelectorFromString(name)]) return NO;
    }
    return object != nil;
}

/// XCAXClient_iOS, which hands requests to testmanagerd.
static id DSHAXClient(void) {
    static id client;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        id found = DSHGet(XCUIDevice.sharedDevice, @"accessibilityInterface");
        if (DSHResponds(found, @[@"requestSnapshotForElement:attributes:parameters:error:",
                                 @"attributesForElement:attributes:error:", @"hitTestElement:withPoint:error:"])) {
            client = found;
        }
    });
    return client;
}

/// The attributes XCUIElement.snapshot() asks for, so that a snapshot taken here reads the same.
static NSArray<NSString *> *DSHAXSnapshotAttributes(void) {
    static NSArray<NSString *> *names;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Class snapshot = NSClassFromString(@"XCElementSnapshot");
        SEL standard = NSSelectorFromString(@"standardAttributeNames");
        SEL translate = NSSelectorFromString(@"axAttributesForSnapshotAttributes:isMacOS:");
        if (![XCUIElement respondsToSelector:standard] || ![snapshot respondsToSelector:translate]) return;
        id keys = ((id (*)(id, SEL))objc_msgSend)(XCUIElement.class, standard);
        id translated = ((id (*)(id, SEL, id, BOOL))objc_msgSend)(snapshot, translate, keys, NO);
        if ([translated isKindOfClass:NSSet.class] && [translated count]) names = [translated allObjects];
    });
    return names;
}

static double (*DSHDragSpeed(void))(double) {
    return dlsym(RTLD_DEFAULT, "XCUIPixelsPerSecondForDragGestureVelocity");
}

BOOL DSHDirectAvailable(void) {
    static BOOL available;
    static dispatch_once_t once;
    dispatch_once(&once, ^{
        Class record = NSClassFromString(@"XCSynthesizedEventRecord"), path = NSClassFromString(@"XCPointerEventPath");
        available = DSHAXClient() && DSHAXSnapshotAttributes() && DSHDragSpeed()
            && DSHResponds(DSHGet(XCUIDevice.sharedDevice, @"eventSynthesizer"), @[@"synthesizeEvent:completion:"])
            && [record instancesRespondToSelector:NSSelectorFromString(@"initWithName:interfaceOrientation:")]
            && [record instancesRespondToSelector:NSSelectorFromString(@"initWithName:")]
            && [record instancesRespondToSelector:NSSelectorFromString(@"addPointerEventPath:")]
            && [path instancesRespondToSelector:NSSelectorFromString(@"initForTouchAtPoint:offset:")]
            && [path instancesRespondToSelector:NSSelectorFromString(@"moveToPoint:atOffset:")]
            && [path instancesRespondToSelector:NSSelectorFromString(@"liftUpAtOffset:")]
            && [path instancesRespondToSelector:NSSelectorFromString(@"initForTextInput")]
            && [path instancesRespondToSelector:NSSelectorFromString(@"typeText:atOffset:typingSpeed:shouldRedact:")];
    });
    return available;
}

static pid_t DSHAXProcess(id element) {
    SEL sel = NSSelectorFromString(@"processIdentifier");
    return [element respondsToSelector:sel] ? ((pid_t (*)(id, SEL))objc_msgSend)(element, sel) : -1;
}

NSArray<NSString *> *DSHActiveApps(void) {
    @try {
        id client = DSHGet(XCUIDevice.sharedDevice, @"accessibilityInterface");
        id tracker = DSHGet(client, @"applicationProcessTracker"), active = DSHGet(client, @"activeApplications");
        SEL find = NSSelectorFromString(@"monitoredApplicationWithProcessIdentifier:");
        if (![tracker respondsToSelector:find] || ![active isKindOfClass:NSArray.class]) return nil;
        NSMutableArray<NSString *> *ids = [NSMutableArray array];
        for (id element in active) {
            pid_t pid = DSHAXProcess(element);
            if (pid <= 0) continue;
            NSString *bundle = DSHGet(((id (*)(id, SEL, pid_t))objc_msgSend)(tracker, find, pid), @"bundleID");
            if ([bundle isKindOfClass:NSString.class] && bundle.length) [ids addObject:bundle];
        }
        return ids;
    } @catch (NSException *exception) {
        return nil;
    }
}

id DSHAXApplication(XCUIApplication *app) {
    return DSHDirectAvailable() ? DSHGet(app, @"accessibilityElement") : nil;
}

BOOL DSHAXOwns(XCUIApplication *app, id element) {
    id root = DSHAXApplication(app);
    return root && DSHAXProcess(element) == DSHAXProcess(root);
}

static NSInteger DSHScreenOrientation = UIInterfaceOrientationPortrait;

void DSHSetScreenOrientation(NSInteger orientation) {
    if (orientation >= UIInterfaceOrientationPortrait && orientation <= UIInterfaceOrientationLandscapeLeft) {
        DSHScreenOrientation = orientation;
    }
}

/// A point of the screen as it is turned, in the points of the screen held in portrait.
static CGPoint DSHDevicePoint(CGPoint point) {
    CGSize pixels = UIScreen.mainScreen.nativeBounds.size;
    CGFloat scale = MAX(UIScreen.mainScreen.nativeScale, 1);
    CGFloat width = MIN(pixels.width, pixels.height) / scale, height = MAX(pixels.width, pixels.height) / scale;
    switch (DSHScreenOrientation) {
    case UIInterfaceOrientationLandscapeRight: return CGPointMake(width - point.y, point.x);
    case UIInterfaceOrientationLandscapeLeft: return CGPointMake(point.y, height - point.x);
    case UIInterfaceOrientationPortraitUpsideDown: return CGPointMake(width - point.x, height - point.y);
    default: return point;
    }
}

id DSHAXElementAtPoint(CGPoint point, XCUIApplication *app) {
    point = DSHDevicePoint(point);
    id root = DSHAXApplication(app);
    if (!root) return nil;
    @try {
        NSError *error = nil;
        id hit = ((id (*)(id, SEL, id, CGPoint, NSError **))objc_msgSend)(
            DSHAXClient(), NSSelectorFromString(@"hitTestElement:withPoint:error:"), root, point, &error);
        return !error && hit && DSHAXOwns(app, hit) ? hit : nil;
    } @catch (NSException *exception) {
        return nil;
    }
}

NSDictionary<NSString *, id> *DSHAXValues(id element, NSArray<NSString *> *names) {
    if (!DSHDirectAvailable()) return nil;
    @try {
        NSError *error = nil;
        id values = ((id (*)(id, SEL, id, id, NSError **))objc_msgSend)(
            DSHAXClient(), NSSelectorFromString(@"attributesForElement:attributes:error:"), element, names, &error);
        return !error && [values isKindOfClass:NSDictionary.class] ? values : nil;
    } @catch (NSException *exception) {
        return nil;
    }
}

BOOL DSHSetAXTimeout(NSTimeInterval seconds) {
    id client = DSHGet(XCUIDevice.sharedDevice, @"accessibilityInterface");
    SEL set = NSSelectorFromString(@"_setAXTimeout:error:");
    if (![client respondsToSelector:set]) return NO;
    @try {
        NSError *error = nil;
        return ((BOOL (*)(id, SEL, NSTimeInterval, NSError **))objc_msgSend)(client, set, seconds, &error) && !error;
    } @catch (NSException *exception) {
        return NO;
    }
}

id<XCUIElementSnapshot> DSHAXSnapshot(id element, NSInteger levels) {
    if (!DSHDirectAvailable()) return nil;
    @try {
        NSError *error = nil;
        id result = ((id (*)(id, SEL, id, id, id, NSError **))objc_msgSend)(
            DSHAXClient(), NSSelectorFromString(@"requestSnapshotForElement:attributes:parameters:error:"),
            element, DSHAXSnapshotAttributes(), levels > 0 ? @{@"maxDepth": @(levels)} : @{}, &error);
        id root = DSHGet(result, @"rootElementSnapshot");
        return !error && [root conformsToProtocol:@protocol(XCUIElementSnapshot)] ? root : nil;
    } @catch (NSException *exception) {
        return nil;
    }
}

id DSHAXElement(id<XCUIElementSnapshot> snapshot) { return DSHGet(snapshot, @"accessibilityElement"); }
id DSHAXParent(id<XCUIElementSnapshot> snapshot) { return DSHGet(snapshot, @"parentAccessibilityElement"); }

/// Hands a finished event to testmanagerd and waits until it has been played.
static BOOL DSHSend(id record) {
    if (!record) return NO;
    dispatch_semaphore_t played = dispatch_semaphore_create(0);
    __block BOOL sent = NO;
    @try {
        ((id (*)(id, SEL, id, void (^)(BOOL, NSError *)))objc_msgSend)(
            DSHGet(XCUIDevice.sharedDevice, @"eventSynthesizer"), NSSelectorFromString(@"synthesizeEvent:completion:"),
            record, ^(BOOL ok, NSError *error) {
                sent = ok && !error;
                dispatch_semaphore_signal(played);
            });
    } @catch (NSException *exception) {
        return NO;
    }
    // The reply can arrive on the main queue, so keep the run loop turning while waiting for it.
    NSDate *limit = [NSDate dateWithTimeIntervalSinceNow:30];
    while (dispatch_semaphore_wait(played, DISPATCH_TIME_NOW) != 0) {
        if (limit.timeIntervalSinceNow < 0) return NO;
        [NSRunLoop.currentRunLoop runMode:NSDefaultRunLoopMode beforeDate:[NSDate dateWithTimeIntervalSinceNow:0.01]];
    }
    return sent;
}

static id DSHTouchEvent(NSString *name, id path) {
    id record = ((id (*)(id, SEL, id, NSInteger))objc_msgSend)(
        [NSClassFromString(@"XCSynthesizedEventRecord") alloc], NSSelectorFromString(@"initWithName:interfaceOrientation:"),
        name, UIInterfaceOrientationPortrait);
    ((void (*)(id, SEL, id))objc_msgSend)(record, NSSelectorFromString(@"addPointerEventPath:"), path);
    return record;
}

static id DSHTouchPath(CGPoint start) {
    return ((id (*)(id, SEL, CGPoint, double))objc_msgSend)(
        [NSClassFromString(@"XCPointerEventPath") alloc], NSSelectorFromString(@"initForTouchAtPoint:offset:"), start, 0.0);
}

// The offsets below are the ones _XCUITapEvent and _XCUIPressHoldDragWithVelocityAndHoldEvent use.
BOOL DSHSendTap(CGPoint point) {
    if (!DSHDirectAvailable()) return NO;
    point = DSHDevicePoint(point);
    id path = DSHTouchPath(point);
    ((void (*)(id, SEL, double))objc_msgSend)(path, NSSelectorFromString(@"liftUpAtOffset:"), 0.05);
    return DSHSend(DSHTouchEvent(@"Tap", path));
}

BOOL DSHSendDrag(CGPoint start, CGPoint end, NSTimeInterval press) {
    if (!DSHDirectAvailable()) return NO;
    double speed = DSHDragSpeed()(XCUIGestureVelocityDefault);
    if (!(speed > 0)) return NO;
    start = DSHDevicePoint(start);
    end = DSHDevicePoint(end);
    id path = DSHTouchPath(start);
    SEL move = NSSelectorFromString(@"moveToPoint:atOffset:");
    if (press > 0) ((void (*)(id, SEL, CGPoint, double))objc_msgSend)(path, move, start, press);
    double arrival = press + hypot(end.x - start.x, end.y - start.y) / speed;
    ((void (*)(id, SEL, CGPoint, double))objc_msgSend)(path, move, end, arrival);
    ((void (*)(id, SEL, double))objc_msgSend)(path, NSSelectorFromString(@"liftUpAtOffset:"), arrival);
    return DSHSend(DSHTouchEvent(@"Press and drag", path));
}

// A finger's swipe: down, across in a fifth of a second, the last few points slowly, up.
// The press and drag above keeps the finger down for 0.7 s over a third of the screen, longer than
// a long press takes to begin (0.5 s). A chat whose bubbles open a menu on a long press that
// survives movement then opens it and stops the list at half the finger's travel. This one is over
// in a third of a second. Lifted at speed, or after a rest at the end, the content coasts on for
// twice the distance; slowing down over the last points is what stops it.
BOOL DSHSendSwipe(CGPoint start, CGPoint end) {
    if (!DSHDirectAvailable()) return NO;
    start = DSHDevicePoint(start);
    end = DSHDevicePoint(end);
    CGFloat far = hypot(end.x - start.x, end.y - start.y), tail = far > 0 ? MIN(6, far / 2) / far : 0;
    CGPoint slowing = CGPointMake(end.x - (end.x - start.x) * tail, end.y - (end.y - start.y) * tail);
    id path = DSHTouchPath(start);
    SEL move = NSSelectorFromString(@"moveToPoint:atOffset:");
    ((void (*)(id, SEL, CGPoint, double))objc_msgSend)(path, move, slowing, 0.22);
    ((void (*)(id, SEL, CGPoint, double))objc_msgSend)(path, move, end, 0.34);
    ((void (*)(id, SEL, double))objc_msgSend)(path, NSSelectorFromString(@"liftUpAtOffset:"), 0.34);
    return DSHSend(DSHTouchEvent(@"Swipe", path));
}

BOOL DSHSendText(NSString *text) {
    if (!DSHDirectAvailable()) return NO;
    id path = DSHGet([NSClassFromString(@"XCPointerEventPath") alloc], @"initForTextInput");
    // 60 keys a second is what XCUIElement.typeText uses unless a user default says otherwise.
    ((void (*)(id, SEL, id, double, NSUInteger, BOOL))objc_msgSend)(
        path, NSSelectorFromString(@"typeText:atOffset:typingSpeed:shouldRedact:"), text, 0.0, 60, NO);
    id record = ((id (*)(id, SEL, id))objc_msgSend)(
        [NSClassFromString(@"XCSynthesizedEventRecord") alloc], NSSelectorFromString(@"initWithName:"), @"Type text");
    ((void (*)(id, SEL, id))objc_msgSend)(record, NSSelectorFromString(@"addPointerEventPath:"), path);
    return DSHSend(record);
}
