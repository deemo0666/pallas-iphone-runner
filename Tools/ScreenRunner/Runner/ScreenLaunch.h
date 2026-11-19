#import <XCTest/XCTest.h>

/// Sends the runner to the background when it is the app in front, as the Home button would.
/// NO when it was not in front.
BOOL DSHLeaveForeground(void);

/// Requests activation without turning an unknown bundle ID into a fatal XCTest failure.
BOOL DSHRequestScreenApp(XCUIApplication *app, NSString *bundleID);

/// Leaves what iOS puts in front of `app` to the runner. Before an event for an app, XCTest
/// otherwise answers a system alert over it by itself, with the alert's cancelling button, and
/// swipes a notification banner away.
void DSHLeaveSystemAlerts(XCUIApplication *app);

/// Runs `block` without XCTest waiting for `app` to idle before and after each event it sends.
/// An app that never reports idle makes XCTest wait out its timeout around every touch.
void DSHPerformWithoutIdleWait(XCUIApplication *app, void (NS_NOESCAPE ^block)(void));

NS_ASSUME_NONNULL_BEGIN

// Every XCUIElement call starts by reading the whole accessibility tree of the front app, and a table
// answers that with one element per row, making each row's cell on the way. On a list of thousands
// of rows that is many seconds per call. The functions below ask the test daemon directly instead:
// for one element, a few levels, or a touch at a point. They use XCUIAutomation's own interfaces as
// found in Xcode 27 and return nil or NO where a version lacks them.

/// Whether the calls below are all present. The runner keeps to XCUIElement otherwise.
BOOL DSHDirectAvailable(void);
/// The bundle IDs of the apps on screen now, as the test daemon reports them. nil where it cannot say.
NSArray<NSString *> * _Nullable DSHActiveApps(void);
/// The accessibility element of `app`.
id _Nullable DSHAXApplication(XCUIApplication *app);
/// Whether `element` belongs to the process of `app`.
BOOL DSHAXOwns(XCUIApplication *app, id element);
/// The element of `app` a touch at `point` (screen points, as the screen is turned) would reach.
id _Nullable DSHAXElementAtPoint(CGPoint point, XCUIApplication *app);
/// Values of accessibility attributes such as XC_kAXXCAttributeVisibleCells.
NSDictionary<NSString *, id> * _Nullable DSHAXValues(id element, NSArray<NSString *> *names);
/// The subtree of `element`, `levels` deep (1 is the element alone), or all of it for 0.
id<XCUIElementSnapshot> _Nullable DSHAXSnapshot(id element, NSInteger levels);
/// The accessibility element a snapshot describes, and that of its parent.
id _Nullable DSHAXElement(id<XCUIElementSnapshot> snapshot);
id _Nullable DSHAXParent(id<XCUIElementSnapshot> snapshot);

/// How long a request to an app's accessibility may take before it fails. XCTest waits a minute by
/// default, per request, on an app that stands still. NO where this version cannot set it.
BOOL DSHSetAXTimeout(NSTimeInterval seconds);

/// Which way the screen is turned now, as a UIInterfaceOrientation raw value. The runner sets it
/// from each screenshot. Element frames and screenshots follow the screen, while the event
/// synthesizer and the hit test below take points of the screen held in portrait: the functions
/// that take a point turn it with this. Setting the event's own orientation does not do it.
void DSHSetScreenOrientation(NSInteger orientation);

/// A tap, a press followed by a drag, and typed text, shaped as XCUICoordinate and XCUIElement
/// shape theirs and sent without reading any tree. Points are screen points as the screen is turned.
BOOL DSHSendTap(CGPoint point);
BOOL DSHSendDrag(CGPoint start, CGPoint end, NSTimeInterval press);
BOOL DSHSendSwipe(CGPoint start, CGPoint end);
BOOL DSHSendText(NSString *text);

NS_ASSUME_NONNULL_END
