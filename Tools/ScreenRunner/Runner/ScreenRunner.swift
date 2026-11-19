import XCTest
import UIKit
import os

final class ScreenRunner: XCTestCase {
    private let issueLock = NSLock()
    private var issueCount = 0
    /// One line per command saying where its time went. Durations and stage names only.
    private static let log = Logger(subsystem: "dsh.screen.runner", category: "timing")
    override func record(_ issue: XCTIssue) {
        issueLock.withLock { issueCount += 1 }
        super.record(issue)
    }

    @MainActor
    func testServePhoneScreen() async throws {
        continueAfterFailure = true
        // The host of a runner signed apart from it cannot reach the App Group's socket.
        // Such a runner can serve several hosts, each with its own key; it answers the first to connect.
        let links = ScreenSocket.foreignLinks
        if ScreenSocket.foreignHost != nil || Bundle.main.object(forInfoDictionaryKey: "DSHScreenHosts") != nil {
            guard !links.isEmpty else { throw RunnerFailure.unavailable }
        }
        let leave = Task { await Self.leaveForegroundWhileWaiting() }
        let accepted: (client: ScreenSocket, host: String)? = try await Task.detached(operation: {
            links.isEmpty ? try ScreenSocket.acceptClient().map { ($0, ScreenSocket.hostID) } : try ScreenSocket.acceptClient(links: links)
        }).value
        leave.cancel()
        guard let (client, host) = accepted else { return }
        defer { client.stop() }
        // An app that stands still (one waiting for a paste question, one that hangs) otherwise holds
        // every request to it for a minute. Ten seconds is twenty times the slowest tree read so far
        // (a chat list of thousands, 0.5 s on wode16).
        Self.log.notice("ax timeout 10 s: \(DSHSetAXTimeout(10) ? "set" : "unavailable", privacy: .public)")
        await serve(client, host: host, hosts: links.map(\.host))
    }

    /// XCTest presses Home once, right after it has turned on Automation Mode. When iOS first asks
    /// for the passcode to allow that, the press is spent under the passcode sheet, and once the
    /// sheet goes the runner's empty black window is left in front. Leave it from here: still in
    /// front two seconds into the wait means XCTest's own press did not do it (it takes about one).
    @MainActor
    private static func leaveForegroundWhileWaiting() async {
        var inFront = 0
        while !Task.isCancelled {
            try? await Task.sleep(for: .milliseconds(500))
            inFront = UIApplication.shared.applicationState == .active ? inFront + 1 : 0
            if inFront >= 4 {
                log.notice("still in front while waiting: \(DSHLeaveForeground() ? "left" : "could not leave", privacy: .public)")
                inFront = 0
            }
        }
    }

    /// What an observation leaves for the command that follows it.
    private struct Seen {
        let id: String
        let pixels: [UInt8]
        /// The screen in points when the page held a long list: the next command must then not go
        /// through XCUIElement, and places its touches itself.
        let screen: CGRect?
        /// The bars floating over the bottom of the page, in points. A control under one was
        /// listed at the point where it can be touched, and is looked up at that point again.
        var bars: [CGRect] = []
        /// The screen in points on any page whose controls were listed: a tap on one of them is
        /// then checked and sent as on a long list, without a live query for the control.
        var frame: CGRect?
    }

    @MainActor
    /// `host` is the Pallas Agent on the other end; `hosts` every one this runner serves.
    private func serve(_ client: ScreenSocket, host: String, hosts: [String]) async {
        var target: XCUIApplication?
        var targetID = ""
        var last: Seen?
        var known: LongList?
        /// Bundle IDs of the apps this connection opened by name.
        var names: [String: String] = [:]
        /// The name an `open` left on the system search page, until one tap there opens an app.
        var choosing: String?
        /// The name of an `open` whose search page never came: its observation of the Home screen
        /// is only to be looked at. No target is kept after it, so nothing there can be touched.
        var missing: String?
        while let cmd = try? await Task.detached(operation: { try client.receive(PhoneCommand.self) }).value {
            var response = RunnerReply()
            var stages: [String] = []
            /// The command's touch was already made: a tap that chose an app on the search page.
            var acted = false
            let began = ContinuousClock.now
            var lap = began
            /// What the list showed and the picture before this command, to tell a list that lags behind its picture.
            let before = (rows: known?.rows, pixels: last?.pixels)
            func mark(_ stage: String) {
                let now = ContinuousClock.now
                stages.append(stage + " " + String(Int((lap.duration(to: now) / .milliseconds(1)).rounded())))
                lap = now
            }
            work: do {
                let issues = issueLock.withLock { issueCount }
                guard client.connected else { throw RunnerFailure.disconnected }
                guard UIApplication.shared.isProtectedDataAvailable else { throw RunnerFailure.deviceLocked }
                if cmd.action == "return" {
                    // The turn ended or waits for the person: put Pallas Agent back in front. Nothing is observed.
                    target = nil
                    targetID = ""
                    choosing = nil
                    last = nil
                    known = nil
                    guard !host.isEmpty, DSHRequestScreenApp(XCUIApplication(bundleIdentifier: host), host) else { throw RunnerFailure.openFailed }
                    mark("return")
                    break work
                }
                if cmd.action == "open" {
                    // Only the runner itself is off limits. Pallas Agent may be opened on purpose to check its own screens.
                    let own = [Bundle.main.bundleIdentifier].compactMap { $0 }
                    guard let name = cmd.app else { throw RunnerFailure.openFailed }
                    var id = names[name] ?? Self.bundleID(for: name)
                    guard id.map(own.contains) != true else { throw RunnerFailure.openFailed }
                    let previous = (target, targetID)
                    target = nil
                    last = nil
                    known = nil
                    choosing = nil
                    missing = nil
                    guard client.connected else { throw RunnerFailure.stopped }
                    if id.map({ DSHRequestScreenApp(XCUIApplication(bundleIdentifier: $0), $0) }) != true {
                        // Three labels or more are a bundle ID that is not installed. Anything
                        // shorter can be the name under an icon: "Booking.com".
                        guard name.split(separator: ".").count <= 2 else { throw RunnerFailure.openFailed }
                        // A question of iOS's own holds the keyboard, so the search page cannot be
                        // typed into. The app in front stays the target: looking at it shows the question.
                        if Self.inFront(of: previous.1).alert != nil {
                            (target, targetID) = previous
                            stages.append("system alert")
                            throw previous.0 == nil ? RunnerFailure.openFailed : .screenChanged
                        }
                        do {
                            id = try await search(for: name, client, mark: mark)
                            mark("tap")
                            guard id.map(own.contains) != true else { throw RunnerFailure.openFailed }
                            names[name] = id
                        } catch is NoSearchPage {
                            id = nil
                            target = XCUIApplication(bundleIdentifier: "com.apple.springboard")
                            targetID = "com.apple.springboard"
                            missing = name
                        } catch let page as SearchPage {
                            // The search page stays the screen the model sees, until it taps one icon.
                            id = nil
                            target = XCUIApplication(bundleIdentifier: page.owner)
                            targetID = page.owner
                            choosing = name
                        }
                    }
                    if choosing == nil, missing == nil {
                        guard let id, case let app = XCUIApplication(bundleIdentifier: id), app.wait(for: .runningForeground, timeout: 4),
                              issueLock.withLock({ issueCount == issues }) else { throw RunnerFailure.openFailed }
                        DSHLeaveSystemAlerts(app)
                        target = app
                        targetID = id
                        mark("launch")
                    }
                } else if choosing != nil, cmd.writes, let page = target {
                    // On the search page only one tap, on an app icon among the results, is taken.
                    // The app it brings forward becomes the target. Nothing else there is touched.
                    guard cmd.action == "tap" else {
                        stages.append("search page: tap only")
                        throw RunnerFailure.notIcon
                    }
                    guard let seen = last, cmd.screen == seen.id else {
                        stages.append("stale screen")
                        throw RunnerFailure.screenChanged
                    }
                    try requireForeground(page, client)
                    let root = try page.snapshot(), all = Self.flat(root)
                    guard let field = all.first(where: { [.searchField, .textField, .textView].contains($0.elementType) }) else {
                        throw RunnerFailure.screenChanged
                    }
                    let spot = try Self.point(cmd.target?.x ?? cmd.x, cmd.target?.y ?? cmd.y, on: root.frame)
                    guard let icon = Self.resultIcons(all, field: field.frame).first(where: { $0.frame.contains(spot) }) else {
                        stages.append("not an icon")
                        throw RunnerFailure.notIcon
                    }
                    last = nil
                    choosing = nil
                    let shown = Set(DSHActiveApps() ?? [])
                    guard DSHSendTap(CGPoint(x: icon.frame.midX, y: icon.frame.midY)) else { throw RunnerFailure.inputFailed }
                    mark("choose")
                    let deadline = ContinuousClock.now + .seconds(8)
                    var opened: String?
                    while opened == nil, ContinuousClock.now < deadline {
                        guard client.connected else { throw RunnerFailure.disconnected }
                        try await Task.sleep(nanoseconds: 200_000_000)
                        opened = DSHActiveApps()?.first { !Self.searchHosts.contains($0) && !shown.contains($0) }
                    }
                    let own = [host, Bundle.main.bundleIdentifier].compactMap { $0 } + hosts
                    guard let id = opened, !own.contains(id), case let app = XCUIApplication(bundleIdentifier: id),
                          app.wait(for: .runningForeground, timeout: 4) else {
                        target = nil
                        targetID = ""
                        XCUIDevice.shared.press(.home)
                        throw RunnerFailure.openFailed
                    }
                    DSHLeaveSystemAlerts(app)
                    target = app
                    targetID = id
                    known = nil
                    acted = true
                    mark("launch")
                }
                guard let app = target else { throw RunnerFailure.openFailed }
                try requireForeground(app, client)
                // What iOS itself has put in front of the app. A notification drops over the top bar
                // for a few seconds. It is neither read nor touched: wait for it to leave, and push
                // it back up if it stays.
                var front = Self.inFront(of: targetID)
                if var banner = front.banner {
                    // A banner stays for seven or eight seconds. One that outlasts ten is there to stay.
                    let limit = ContinuousClock.now + .seconds(10)
                    while true {
                        guard ContinuousClock.now < limit else {
                            _ = DSHSendSwipe(CGPoint(x: banner.midX, y: banner.midY), CGPoint(x: banner.midX, y: max(banner.minY - banner.height, 1)))
                            try await Task.sleep(nanoseconds: 600_000_000)
                            break
                        }
                        try await Task.sleep(nanoseconds: 300_000_000)
                        try requireForeground(app, client)
                        guard let still = Self.inFront(of: targetID).banner else { break }
                        banner = still
                    }
                    front = Self.inFront(of: targetID)
                    mark("banner")
                }
                if cmd.writes && !acted {
                    // Each refusal leaves its reason in the timing line, as counts and grid positions.
                    guard let seen = last, cmd.screen == seen.id else {
                        stages.append("stale screen")
                        throw RunnerFailure.screenChanged
                    }
                    let current = try Self.capture()
                    last = nil // consume once, including attempts with an uncertain outcome
                    let change = PhoneFrame.change(from: seen.pixels, to: current.signature, at: cmd)
                    stages.append("pixels " + (change?.summary ?? "?"))
                    guard change?.permits == true else { throw RunnerFailure.screenChanged }
                    // A touch on the expanded Live Activity brings Pallas Agent forward, which ends the turn.
                    if cmd.reachesTop, let overlay = cmd.overlay,
                       let y = PhoneFrame.overlayLine(in: await Task.detached { current.topText() }.value, overlay: overlay) {
                        Self.collapseOverlay(at: y, on: seen.screen)
                        throw RunnerFailure.covered
                    }
                    try requireForeground(app, client)
                    mark("check")
                    switch cmd.action {
                    case "tap":
                        // A question that came up after `front` was read has changed the picture,
                        // and the check above has refused the touch.
                        if let alert = front.alert {
                            // Only the question's own buttons can be reached. A touch meant for a
                            // control of the app would land on the question instead.
                            var place = (cmd.x, cmd.y)
                            if let target = cmd.target {
                                guard target.role == "alert", let button = Self.buttons(of: alert).first(where: {
                                    $0.label == target.label && abs($0.x - target.x) <= 5 && abs($0.y - target.y) <= 5
                                }) else {
                                    stages.append("system alert")
                                    throw RunnerFailure.screenChanged
                                }
                                place = (button.x, button.y)
                                mark("find")
                            }
                            let point = try Self.point(place.0, place.1, on: alert.screen)
                            if !DSHSendTap(point) {
                                let owner = XCUIApplication(bundleIdentifier: alert.owner)
                                DSHLeaveSystemAlerts(owner)
                                let spot = owner.coordinate(withNormalizedOffset: .zero).withOffset(CGVector(dx: point.x, dy: point.y))
                                DSHPerformWithoutIdleWait(owner) { spot.tap() }
                            }
                        } else if cmd.target?.role == "alert" {
                            stages.append("alert gone")
                            throw RunnerFailure.screenChanged
                        } else if let screen = seen.screen {
                            let point = try Self.point(cmd.target?.x ?? cmd.x, cmd.target?.y ?? cmd.y, on: screen)
                            if let target = cmd.target {
                                guard Self.holds(target, at: point, on: screen, in: app, under: seen.bars) else {
                                    stages.append("target gone")
                                    throw RunnerFailure.screenChanged
                                }
                                try requireForeground(app, client)
                                mark("find")
                            }
                            guard DSHSendTap(point) else { throw RunnerFailure.inputFailed }
                        } else if let screen = seen.frame, let target = cmd.target,
                                  case let point = try Self.point(target.x, target.y, on: screen),
                                  Self.holds(target, at: point, on: screen, in: app, under: seen.bars) {
                            // Any other page whose controls were listed: the control is still under
                            // the point, so the touch is sent there. One look at the point replaces
                            // the live query below, which reads the tree again for every property
                            // (wode16, Settings: 0.4 s to find and 0.5 s to tap). When the look does
                            // not find the control, the query still decides.
                            try requireForeground(app, client)
                            mark("find at point")
                            guard DSHSendTap(point) else { throw RunnerFailure.inputFailed }
                        } else if let target = cmd.target {
                            let query = app.descendants(matching: .any).matching(NSPredicate(
                                format: "label == %@ AND elementType IN %@", target.label, Self.types(of: target).map(\.rawValue)))
                            let bounds = app.frame
                            let matches = query.allElementsBoundByAccessibilityElement.compactMap { element -> (XCUIElement, CGPoint)? in
                                guard element.isEnabled, element.isHittable,
                                      let spot = PhoneTarget.touchPoint(of: element.frame, in: bounds, under: seen.bars) else { return nil }
                                let point = Self.place(spot, in: bounds)
                                return abs(point.0 - target.x) <= 5 && abs(point.1 - target.y) <= 5 ? (element, spot) : nil
                            }
                            guard matches.count == 1 else {
                                stages.append("target found \(matches.count)")
                                throw RunnerFailure.screenChanged
                            }
                            try requireForeground(app, client)
                            mark("find")
                            let (element, spot) = matches[0]
                            if spot == CGPoint(x: element.frame.midX, y: element.frame.midY) {
                                DSHPerformWithoutIdleWait(app) { element.tap() }
                            } else {
                                // A bar lies over the control's centre: touch what shows above it.
                                stages.append("above bar")
                                let clear = app.coordinate(withNormalizedOffset: .zero)
                                    .withOffset(CGVector(dx: spot.x - bounds.minX, dy: spot.y - bounds.minY))
                                DSHPerformWithoutIdleWait(app) { clear.tap() }
                            }
                        } else if let screen = seen.frame, DSHSendTap(try Self.point(cmd.x, cmd.y, on: screen)) {
                            // A touch at a place the model chose, sent as it is.
                            stages.append("sent at point")
                        } else {
                            let point = try coordinate(cmd.x, cmd.y, in: app)
                            DSHPerformWithoutIdleWait(app) { point.tap() }
                        }
                    case "swipe":
                        // On every page, not only over a long list: the swipe is shaped here, as a finger's.
                        // An app that waits for the answer to a question of iOS's own may answer
                        // nothing else, so it is not asked for its frame then.
                        if let screen = front.alert?.screen ?? seen.screen ?? DSHAXApplication(app).flatMap({ DSHAXSnapshot($0, 1)?.frame }),
                           !screen.isEmpty {
                            guard try DSHSendSwipe(Self.point(cmd.x, cmd.y, on: screen), Self.point(cmd.x2, cmd.y2, on: screen)) else {
                                throw RunnerFailure.inputFailed
                            }
                        } else {
                            // XCTest's own drag cannot be shaped that way: fast enough to stay under a long
                            // press, it coasts for twice the distance, with a rest at the end or without.
                            let start = try coordinate(cmd.x, cmd.y, in: app)
                            let end = try coordinate(cmd.x2, cmd.y2, in: app)
                            DSHPerformWithoutIdleWait(app) { start.press(forDuration: 0.12, thenDragTo: end) }
                        }
                    case "type":
                        guard var rest = cmd.text?[...], !rest.isEmpty, rest.utf8.count <= 8192 else { throw RunnerFailure.invalidAction }
                        // Nothing is typed into a question of iOS's own: it may be asking for a password.
                        guard front.alert == nil else {
                            stages.append("system alert")
                            throw RunnerFailure.screenChanged
                        }
                        while !rest.isEmpty {
                            try requireForeground(app, client)
                            // Each event costs about a third of a second besides its keys (wode16: 384 ms
                            // for three characters), so three at a time made a page of text take minutes.
                            // Forty are typed in two thirds of a second, which is then how often the
                            // app in front is looked at.
                            let chunk = rest.prefix(40)
                            if seen.screen != nil {
                                guard DSHSendText(String(chunk)) else { throw RunnerFailure.inputFailed }
                            } else {
                                DSHPerformWithoutIdleWait(app) { app.typeText(String(chunk)) }
                            }
                            guard issueLock.withLock({ issueCount == issues }) else { throw RunnerFailure.inputFailed }
                            rest = rest.dropFirst(chunk.count)
                        }
                    default: throw RunnerFailure.invalidAction
                    }
                    guard issueLock.withLock({ issueCount == issues }) else { throw RunnerFailure.inputFailed }
                    mark("act")
                } else if !cmd.writes, cmd.action != "open", cmd.action != "observe" { throw RunnerFailure.invalidAction }
                try requireForeground(app, client)
                // XCTest no longer waits for the app after an event, so wait on the picture instead.
                var frame = cmd.action == "observe" ? try Self.capture() : try await Self.settledFrame()
                mark("settle")
                var observation: PhoneObservation!
                var list: LongList?
                var bars: [CGRect] = []
                var bounds: CGRect?
                for pass in 0..<2 {
                    observation = try await Self.read(frame, cmd)
                    mark("read")
                    // A question of iOS's own covers the app. It is the page now, and its buttons are
                    // all that can be tapped. The app is asked nothing: one that reads the pasteboard
                    // on its main thread stands still until the paste question is answered, and
                    // every request to it then runs into the accessibility timeout, a minute long.
                    if let alert = Self.inFront(of: targetID).alert {
                        if cmd.navigation == true {
                            let words = Self.flat(alert.node).first { $0.elementType == .staticText && !$0.label.isEmpty }?.label
                            (observation.page, observation.controls) = (alert.node.label.isEmpty ? words : alert.node.label, Self.buttons(of: alert))
                        }
                        list = known
                        mark("alert")
                        break
                    }
                    list = Self.longList(in: app, known: known)
                    // After a swipe a list can go on naming the rows it showed before, for a moment.
                    // The picture has moved and the rows have not: ask again, three times at most.
                    if cmd.action == "swipe", pass == 0, let old = before.rows, let pixels = before.pixels,
                       PhoneFrame.change(from: pixels, to: frame.signature, at: PhoneCommand(action: "observe", picture: false))?.slight != true {
                        var waits = 0
                        while waits < 3, let rows = list?.rows, Self.same(rows, old) {
                            try await Task.sleep(nanoseconds: 300_000_000)
                            try requireForeground(app, client)
                            list = Self.longList(in: app, known: known)
                            waits += 1
                        }
                        if waits > 0 { stages.append("rows lagged \(waits)") }
                    }
                    mark(list.map { "list \($0.rows.count)/\($0.total)" } ?? "list")
                    // iOS opens the Live Activity expanded over the target's status and navigation bars and
                    // keeps it there until it is swiped. Shrink it, then read the screen the model acts on.
                    for _ in 0..<2 {
                        guard let y = PhoneFrame.overlayLine(in: observation.text, overlay: cmd.overlay ?? []) else { break }
                        Self.collapseOverlay(at: y, on: list?.screen)
                        try requireForeground(app, client)
                        frame = try await Self.settledFrame()
                        observation = try await Self.read(frame, cmd)
                        list = Self.longList(in: app, known: known)
                        mark("island")
                    }
                    if cmd.navigation == true {
                        if let menu = list.flatMap(Self.menu(around:)) {
                            (observation.page, observation.controls, bars, bounds) = (menu.page, menu.controls, menu.bars, menu.bounds)
                            mark("rows")
                        } else {
                            // Asked for directly, the tree comes without XCUIElement's own waiting and checks.
                            let direct = DSHAXApplication(app).flatMap { DSHAXSnapshot($0, 0) }
                            let menu = Self.navigationMenu(try direct ?? app.snapshot())
                            (observation.page, observation.controls, bars) = (menu.page, menu.controls, menu.bars)
                            bounds = direct == nil ? nil : menu.bounds
                            mark(direct == nil ? "controls" : "tree")
                        }
                        if !bars.isEmpty { stages.append("bars \(bars.count)") }
                    }
                    // An app that is starting holds a still launch picture and shows its first page while
                    // that picture is being read, so the first touch would be refused against it. After
                    // `open`, look once more, and read again when the screen has moved and stopped.
                    guard cmd.action == "open" || acted, pass == 0 else { break }
                    let moved = PhoneFrame.change(from: frame.signature, to: try Self.capture().signature, at: cmd)?.slight != true
                    mark(moved ? "moved" : "still")
                    guard moved else { break }
                    try requireForeground(app, client)
                    frame = try await Self.settledFrame()
                    mark("settle")
                }
                try requireForeground(app, client)
                known = list
                last = Seen(id: observation.screen, pixels: frame.signature, screen: list?.screen, bars: bars, frame: bounds)
                observation.opening = choosing
                observation.searchMissing = missing
                if missing != nil {
                    // Looked at only: the next command must be another `open`.
                    target = nil
                    targetID = ""
                    last = nil
                    known = nil
                    missing = nil
                }
                response.observation = observation
            } catch {
                response.error = ((error as? RunnerFailure) ?? .captureFailed).rawValue
            }
            let reply = response
            let total = Int((began.duration(to: .now) / .milliseconds(1)).rounded())
            Self.log.notice("\(cmd.action, privacy: .public) \(total) ms: \(stages.joined(separator: ", "), privacy: .public) \(reply.error ?? "ok", privacy: .public)")
            do { try await Task.detached { try client.send(reply) }.value } catch { return }
            let recoverable: [RunnerFailure] = [.screenChanged, .openFailed, .covered, .notIcon]
            if let error = reply.error, !recoverable.map(\.rawValue).contains(error) { return }
        }
    }

    private static func read(_ frame: PhoneFrame, _ cmd: PhoneCommand) async throws -> PhoneObservation {
        try await Task.detached { try frame.observation(picture: cmd.picture) }.value
    }

    /// The first frame after which the picture stopped changing, or the one at `limit`.
    /// A busy app never tells XCTest it is idle, so the runner waits on what it can see.
    @MainActor private static func settledFrame(limit: Duration = .seconds(2)) async throws -> PhoneFrame {
        let deadline = ContinuousClock.now + limit
        try await Task.sleep(nanoseconds: 200_000_000)
        var frame = try capture()
        while ContinuousClock.now < deadline {
            try await Task.sleep(nanoseconds: 150_000_000)
            let next = try capture()
            let still = next.matches(frame.signature, at: PhoneCommand(action: "observe", picture: false))
            frame = next
            if still { break }
        }
        return frame
    }

    /// A drag from the right side of the expanded Live Activity towards its middle shrinks it to the
    /// compact island, where it stays. `y` is the line the runner read it at, in thousandths.
    /// The gesture goes through SpringBoard, which owns the island: through the target app XCTest
    /// first waits for that app to idle, and on a busy screen the wait alone outlasts the caller.
    /// Over a long list, where `screen` is known, the drag is sent as is: for an event anywhere,
    /// XCTest reads the front app's tree as well.
    @MainActor private static func collapseOverlay(at y: Int, on screen: CGRect?) {
        let dy = CGFloat(min(max(y, 40), 100)) / 1000
        if let screen {
            _ = DSHSendDrag(CGPoint(x: screen.minX + 0.85 * screen.width, y: screen.minY + dy * screen.height),
                            CGPoint(x: screen.midX, y: screen.minY + dy * screen.height), 0.12)
            return
        }
        let system = XCUIApplication(bundleIdentifier: "com.apple.springboard")
        DSHLeaveSystemAlerts(system)
        let start = system.coordinate(withNormalizedOffset: CGVector(dx: 0.85, dy: dy))
        let end = system.coordinate(withNormalizedOffset: CGVector(dx: 0.5, dy: dy))
        DSHPerformWithoutIdleWait(system) { start.press(forDuration: 0.12, thenDragTo: end) }
    }

    /// What iOS itself has put in front of the app.
    private struct InFront {
        /// A question or a notice of iOS's own: the process that draws it, the alert, and that
        /// process's frame, which is the screen.
        var alert: (owner: String, node: XCUIElementSnapshot, screen: CGRect)?
        /// Where a notification banner lies.
        var banner: CGRect?
    }

    /// The test daemon names the processes that have something on screen. With nothing in the way
    /// that is the app alone. A permission question replaces it with SpringBoard, and a banner
    /// brings SpringBoard in beside it. Neither is in the app's own tree, so the app's page and
    /// controls read as if nothing were there. Alerts of other apps are theirs to show.
    @MainActor private static func inFront(of id: String) -> InFront {
        var found = InFront()
        guard let active = DSHActiveApps(), active != [id] else { return found }
        for other in active where other != id && other.hasPrefix("com.apple.") {
            guard let root = try? XCUIApplication(bundleIdentifier: other).snapshot() else { continue }
            for node in flat(root) where !node.frame.isEmpty {
                if node.elementType == .alert, found.alert == nil { found.alert = (other, node, root.frame) }
                if node.identifier == "NotificationShortLookView" { found.banner = node.frame }
            }
        }
        return found
    }

    private static func flat(_ node: XCUIElementSnapshot) -> [XCUIElementSnapshot] { [node] + node.children.flatMap(flat) }

    /// Whether two readings of a list name the same rows in the same order.
    private static func same(_ rows: [Any], _ others: [Any]) -> Bool {
        rows.count == others.count && zip(rows, others).allSatisfy { ($0 as? NSObject)?.isEqual($1) == true }
    }

    /// The buttons of a system alert, as controls of their own kind.
    private static func buttons(of alert: (owner: String, node: XCUIElementSnapshot, screen: CGRect)) -> [PhoneTarget] {
        flat(alert.node).filter { $0.elementType == .button && $0.isEnabled && !$0.label.isEmpty && !$0.frame.isEmpty }.map {
            let place = center($0.frame, in: alert.screen)
            return PhoneTarget(label: $0.label, role: "alert", x: place.0, y: place.1)
        }
    }

    @MainActor private func requireForeground(_ app: XCUIApplication, _ client: ScreenSocket) throws {
        guard client.connected else { throw RunnerFailure.disconnected }
        guard UIApplication.shared.isProtectedDataAvailable else { throw RunnerFailure.deviceLocked }
        guard app.state == .runningForeground else { throw RunnerFailure.foregroundLost }
        guard client.connected else { throw RunnerFailure.disconnected }
    }

    @MainActor private static func capture() throws -> PhoneFrame {
        guard let frame = PhoneFrame(XCUIScreen.main.screenshot().pngRepresentation) else { throw RunnerFailure.captureFailed }
        DSHSetScreenOrientation(frame.orientation)
        return frame
    }

    private static func center(_ rect: CGRect, in screen: CGRect) -> (Int, Int) {
        place(CGPoint(x: rect.midX, y: rect.midY), in: screen)
    }

    /// A point of the screen in the 0..1000 coordinates of an observation.
    private static func place(_ point: CGPoint, in screen: CGRect) -> (Int, Int) {
        (Int(((point.x - screen.minX) * 1000 / max(screen.width, 1)).rounded()),
         Int(((point.y - screen.minY) * 1000 / max(screen.height, 1)).rounded()))
    }

    /// A list on screen with far more rows than it shows. XCTest gets one element per row of a table,
    /// on screen or not, and the table makes each row's cell to describe it. In a chat list of
    /// thousands that takes many seconds, and every XCUIElement call pays it again.
    private struct LongList {
        let app: Any
        /// The application's frame, which is the screen in points.
        let screen: CGRect
        let list: Any
        let rows: [Any]
        /// How many rows the list has in all.
        let total: Int
        /// Levels between the application and the list.
        let depth: Int
    }

    /// `known` is the long list of the previous look. Under a menu, a sheet or a search page the
    /// middle of the screen is something else, while that list is often still in the tree.
    @MainActor private static func longList(in app: XCUIApplication, known: LongList?) -> LongList? {
        guard let root = DSHAXApplication(app), let screen = DSHAXSnapshot(root, 1)?.frame, !screen.isEmpty else { return nil }
        let parent = "XC_kAXXCAttributeParent", cells = "XC_kAXXCAttributeVisibleCells", count = "XC_kAXXCAttributeChildrenCount"
        for (start, remembered) in [(DSHAXElementAtPoint(CGPoint(x: screen.midX, y: screen.midY), app), false), (known?.list, true)] {
            guard var element = start else { continue }
            var found: (list: Any, rows: [Any], total: Int, level: Int)?
            var level = 0
            // The outermost long list wins: reading it whole would cost the most.
            while level < 60, let values = DSHAXValues(element, [parent, cells, count]) {
                if let rows = values[cells] as? [Any], !rows.isEmpty, let total = values[count] as? Int, total >= 60 {
                    found = (element, rows, total, level)
                }
                guard let above = values[parent], DSHAXOwns(app, above) else {
                    // A page pushed over the list takes it out of the app's tree. The list still
                    // answers then, but its parents no longer lead to the application.
                    if let found, !remembered || DSHAXSnapshot(element, 1)?.elementType == .application {
                        return LongList(app: root, screen: screen, list: found.list, rows: found.rows, total: found.total,
                                        depth: level - found.level)
                    }
                    break
                }
                element = above
                level += 1
            }
        }
        return nil
    }

    /// The page and controls of a screen with a long list: the tree down to the list, the subtrees
    /// beside it, and of the list itself only the rows it reports as visible.
    @MainActor private static func menu(around found: LongList) -> (page: String, controls: [PhoneTarget], bars: [CGRect], bounds: CGRect)? {
        guard let top = DSHAXSnapshot(found.app, found.depth + 1) else { return nil }
        var level = [top]
        for _ in 0..<found.depth { level = level.flatMap { $0.children } }
        guard let list = level.first(where: { (DSHAXElement($0) as? NSObject)?.isEqual(found.list) == true }) else {
            // A sheet or an alert stands in front, and the tree is that alone.
            return DSHAXSnapshot(found.app, 0).map { navigationMenu($0) }
        }
        var whole: [ObjectIdentifier: XCUIElementSnapshot] = [:]
        for node in level where node !== list {
            // `top` stops at this level. A node here may have had children.
            guard let element = DSHAXElement(node), let full = DSHAXSnapshot(element, 0) else { return nil }
            whole[ObjectIdentifier(node)] = full
        }
        let rows = shown(of: found, on: top.frame)
        return navigationMenu(top) { node in
            node === list ? rows : node.children.map { whole[ObjectIdentifier($0)] ?? $0 }
        }
    }

    /// What the list shows: its visible rows, and the headers and footers before, between and after them.
    @MainActor private static func shown(of found: LongList, on screen: CGRect) -> [XCUIElementSnapshot] {
        func read(_ elements: [Any]) -> [XCUIElementSnapshot] {
            elements.compactMap { DSHAXSnapshot($0, 0) }.filter { $0.frame.intersects(screen) }
        }
        // The list names all its children here without making a cell for any of them.
        let children = "XC_kAXXCAttributeChildren"
        guard found.total <= 5000, let all = DSHAXValues(found.list, [children])?[children] as? NSArray,
              case let places = found.rows.map({ all.index(of: $0) }), !places.contains(NSNotFound),
              let first = places.min(), let last = places.max(), last - first < places.count + 8 else {
            return read(found.rows).sorted { ($0.frame.minY, $0.frame.minX) < ($1.frame.minY, $1.frame.minX) }
        }
        // A header comes before the first row and a footer after the last. Anywhere else the
        // neighbours of the visible rows are rows off screen, and reading one makes its cell.
        let from = first <= 8 ? 0 : first, through = last >= all.count - 9 ? all.count - 1 : last
        return read((from...through).map { all[$0] })
    }

    /// A keypad drawn by the app has keys, not buttons: Calculator's digits. They are listed and
    /// tapped as buttons. The system keyboard's keys are never listed.
    private static func types(of target: PhoneTarget) -> [XCUIElement.ElementType] {
        target.role == "cell" ? [.cell] : target.role == "link" ? [.link] : target.role == "rowText" ? [.staticText]
            : target.role == "image" ? [.image] : [.button, .key]
    }

    /// Whether a touch at `point` reaches the control the model chose: the element under the point,
    /// or one of the few that contain it, has that control's kind, name and place and is enabled.
    @MainActor private static func holds(_ target: PhoneTarget, at point: CGPoint, on screen: CGRect, in app: XCUIApplication,
                                         under bars: [CGRect]) -> Bool {
        var element = DSHAXElementAtPoint(point, app)
        for _ in 0..<8 {
            guard let current = element, let node = DSHAXSnapshot(current, 1) else { return false }
            // The control is listed where it is touched, which is above its centre under a floating bar.
            if types(of: target).contains(node.elementType), node.label == target.label, node.isEnabled,
               let spot = PhoneTarget.touchPoint(of: node.frame, in: screen, under: bars), case let listed = place(spot, in: screen),
               abs(listed.0 - target.x) <= 5, abs(listed.1 - target.y) <= 5 { return true }
            element = DSHAXParent(node)
        }
        return false
    }

    private static func point(_ x: Int?, _ y: Int?, on screen: CGRect) throws -> CGPoint {
        guard let x, let y, (0...1000).contains(x), (0...1000).contains(y) else { throw RunnerFailure.invalidAction }
        return CGPoint(x: screen.minX + CGFloat(x) / 1000 * screen.width, y: screen.minY + CGFloat(y) / 1000 * screen.height)
    }

    /// `children` gives the nodes below one, for a tree put together from several snapshots.
    private static func navigationMenu(
        _ root: XCUIElementSnapshot, children: (XCUIElementSnapshot) -> [XCUIElementSnapshot] = { $0.children }
    ) -> (page: String, controls: [PhoneTarget], bars: [CGRect], bounds: CGRect) {
        var page = root.label, controls: [PhoneTarget] = []
        let bounds = root.frame
        // The bars that float over the bottom of the page: iOS draws the page's rows under them,
        // and a touch there goes to the bar.
        var bars: [CGRect] = []
        func findBars(_ item: XCUIElementSnapshot, above: [CGRect]) {
            let frame = item.frame
            if [.searchField, .tabBar, .toolbar].contains(item.elementType), bounds.intersects(frame),
               frame.minY > bounds.minY + bounds.height * 0.6, frame.width > bounds.width * 0.4 {
                bars.append(PhoneTarget.barFrame(of: frame, above: above))
                return
            }
            for child in children(item) { findBars(child, above: above + [frame]) }
        }
        findBars(root, above: [])
        func editsValue(_ node: XCUIElementSnapshot) -> Bool {
            [.switch, .slider, .stepper, .secureTextField, .textField].contains(node.elementType)
                || children(node).contains(where: editsValue)
        }
        /// `inside` is true below a control that is listed itself: its pictures are its own icon.
        /// `row` is the frame of the list row the item belongs to.
        func visit(_ item: XCUIElementSnapshot, navigationBar: Bool, rowText: Bool, inside: Bool, row: CGRect? = nil) {
            let type = item.elementType, frame = item.frame
            // A list reuses its rows, and some apps go on reporting the contents of a row that has
            // left the screen where they last were. What a row holds lies inside it.
            if let row, !row.insetBy(dx: -2, dy: -2).contains(CGPoint(x: frame.midX, y: frame.midY)) { return }
            let nav = type == .navigationBar || navigationBar
            if [.secureTextField, .textField, .textView, .keyboard, .switch, .slider, .stepper].contains(type) { return }
            // A row that holds a field or a switch is not a control, and its text names the value.
            // What else it holds is: Calendar keeps an event's title field and the button for its
            // date and time in one row.
            let editing = type == .cell && editsValue(item)
            if type == .navigationBar, bounds.intersects(frame) {
                let title = item.label.isEmpty ? item.identifier : item.label
                if !title.isEmpty { page = title }
            }
            let parentIndex = controls.count
            // A picture that stands alone and is large enough to be one is a control: a photo in
            // Photos' grid, named by its date. Icons and symbols are smaller.
            let picture = type == .image && !inside && !nav && frame.width >= 44 && frame.height >= 44
            if !editing, ([.cell, .button, .link, .key].contains(type) || (type == .staticText && rowText) || picture),
               item.isEnabled, !item.label.isEmpty,
               item.label.count <= 200, frame.width > 5, frame.height > 5,
               let spot = PhoneTarget.touchPoint(of: frame, in: bounds, under: bars) {
                let p = place(spot, in: bounds)
                let explicitBack = ["back", "backbutton", "返回"].contains(item.identifier.lowercased())
                    || ["back", "返回"].contains(item.label.lowercased())
                let role = nav && type == .button ? (explicitBack ? "back" : "navigationButton") :
                    type == .cell ? "cell" : type == .link ? "link" : type == .staticText ? "rowText" : type == .image ? "image" : "button"
                let target = PhoneTarget(label: item.label, role: role, x: p.0, y: p.1)
                if !controls.contains(target), controls.count < 245 { controls.append(target) }
            }
            let childStart = controls.count
            let useRowText = type == .cell ? item.label.isEmpty && !editing : rowText && type != .button && type != .link
            let within = inside || controls.count > parentIndex
            for child in children(item) { visit(child, navigationBar: nav, rowText: useRowText, inside: within, row: type == .cell && frame.height > 5 ? frame : row) }
            // Settings exposes a row and its embedded button as two elements with different frames.
            if type == .cell, childStart > parentIndex, controls[childStart...].contains(where: {
                $0.label == item.label && ($0.role == "button" || $0.role == "link")
            }) {
                controls.remove(at: parentIndex)
            }
        }
        visit(root, navigationBar: false, rowText: false, inside: false)
        return (page, controls, bars, bounds)
    }

    @MainActor private func coordinate(_ x: Int?, _ y: Int?, in app: XCUIApplication) throws -> XCUICoordinate {
        guard let x, let y, (0...1000).contains(x), (0...1000).contains(y) else { throw RunnerFailure.invalidAction }
        return app.coordinate(withNormalizedOffset: CGVector(dx: CGFloat(x)/1000, dy: CGFloat(y)/1000))
    }

    /// Opens an app known only by the name under its icon, the way a person does: Home, pull the
    /// search down, type the name, tap the icon that carries it. On a real device LaunchServices
    /// lists no installed apps to this process, so a name cannot be turned into a bundle ID first.
    /// Returns the bundle ID of the app that came to the front. Nothing on the search page is read
    /// out: only app icons are compared with the name. `mark` names each stage reached.
    @MainActor private func search(for name: String, _ client: ScreenSocket, mark: (String) -> Void) async throws -> String {
        let home = "com.apple.springboard"
        func wait<T>(_ limit: Duration, for ready: () -> T?) async throws -> T? {
            let deadline = ContinuousClock.now + limit
            while true {
                guard client.connected else { throw RunnerFailure.disconnected }
                guard UIApplication.shared.isProtectedDataAvailable else { throw RunnerFailure.deviceLocked }
                if let value = ready() { return value }
                guard ContinuousClock.now < deadline else { return nil }
                try await Task.sleep(nanoseconds: 200_000_000)
            }
        }
        /// The system's search page and its field, when it is up and takes typing. Only these
        /// processes are looked at: a field with the keyboard in any other app is not for this text.
        /// iOS 27 moved the page into `com.apple.campo`, with the field floating at the top.
        func page() -> (all: [XCUIElementSnapshot], field: XCUIElementSnapshot, owner: String)? {
            for id in DSHActiveApps() ?? [] where Self.searchHosts.contains(id) {
                guard let root = try? XCUIApplication(bundleIdentifier: id).snapshot() else { continue }
                let all = Self.flat(root), focus = NSSelectorFromString("hasKeyboardFocus")
                if let field = all.first(where: { node in
                    // iOS 27's field is a text view.
                    [.searchField, .textField, .textView].contains(node.elementType)
                        && ((node as? NSObject).flatMap { $0.responds(to: focus) ? $0.value(forKey: "hasKeyboardFocus") as? Bool : nil } ?? true)
                }) { return (all, field, id) }
            }
            return nil
        }
        func fail(_ stage: String) -> RunnerFailure {
            mark("no " + stage)
            XCUIDevice.shared.press(.home)
            return .openFailed
        }
        guard DSHDirectAvailable(), DSHActiveApps() != nil else { throw RunnerFailure.openFailed }
        // The page is always pulled down afresh from the Home screen. A name searched before is
        // still in its field then, selected, and typing replaces it. On the page that Home returns
        // to after an app was opened from it, typing adds to the old name instead, and emptying
        // the field first leaves the page showing what it shows for an empty field.
        var entered: Bool?
        for attempt in 0..<3 where entered == nil {
            XCUIDevice.shared.press(.home)
            var there = try await wait(.seconds(2), for: { () -> Bool? in page() != nil ? false : DSHActiveApps() == [home] ? true : nil })
            if there == false {
                // Home has led back to the search page an app was opened from. Once more, for the Home screen.
                XCUIDevice.shared.press(.home)
                there = try await wait(.milliseconds(2500), for: { page() == nil && DSHActiveApps() == [home] ? true : nil })
            }
            mark("home")
            // Pull only once the Home screen has stopped moving after the press.
            if there == true { _ = try await Self.settledFrame(limit: .milliseconds(1500)) }
            if there == true, let board = try? XCUIApplication(bundleIdentifier: home).snapshot(), !board.frame.isEmpty {
                // Pull down from an app icon: on a widget stack the same pull turns its pages instead.
                let screen = board.frame, middle = CGPoint(x: screen.midX, y: screen.minY + 0.45 * screen.height)
                let icons = Self.flat(board).filter {
                    $0.elementType == .icon && $0.frame.width < 0.3 * screen.width && screen.contains($0.frame)
                        && (0.2...0.7).contains(($0.frame.midY - screen.minY) / screen.height)
                }.map { CGPoint(x: $0.frame.midX, y: $0.frame.midY) }
                    .sorted { hypot($0.x - middle.x, $0.y - middle.y) < hypot($1.x - middle.x, $1.y - middle.y) }
                // A pull that did not bring the page starts from another icon the next time.
                let start = icons.isEmpty ? middle : icons[attempt % icons.count]
                // End the pull above the page dots and the Dock: from a low icon a full-length pull
                // ran into them and brought no search page (iOS 27).
                let reach = max(0.12 * screen.height, min(0.25 * screen.height, screen.minY + 0.72 * screen.height - start.y))
                Self.log.notice("open search pull from \(Int(start.x)),\(Int(start.y)) by \(Int(reach)) of \(Int(screen.width))x\(Int(screen.height))")
                guard DSHSendSwipe(start, CGPoint(x: start.x, y: start.y + reach)) else { break }
                _ = try await wait(.milliseconds(2500), for: { page() != nil ? true : nil })
                mark("pull")
                if page() == nil {
                    let apps = DSHActiveApps() ?? []
                    let fields = apps.map { id -> String in
                        let all = (try? XCUIApplication(bundleIdentifier: id).snapshot()).map(Self.flat) ?? []
                        let inputs = all.filter { [.searchField, .textField, .textView].contains($0.elementType) }
                            .map { "\($0.elementType.rawValue)@\(Int($0.frame.midY)) \($0.identifier) focus=\(String(describing: ($0 as? NSObject)?.value(forKey: "hasKeyboardFocus")))" }
                        return "\(id) n=\(all.count) [\(inputs.joined(separator: "; "))]"
                    }
                    Self.log.notice("open search no page: \(fields.joined(separator: " | "), privacy: .public)")
                }
                // The keyboard takes text once the page has stopped moving in.
                _ = try await Self.settledFrame(limit: .milliseconds(1500))
                if page() != nil {
                    guard DSHSendText(name) else { throw fail("typing") }
                    let wanted = PhoneCommand.bare(name)
                    func written() -> String? { (page()?.field.value as? String).map(PhoneCommand.bare) }
                    if try await wait(.seconds(1), for: { written() == wanted ? true : nil }) != nil {
                        entered = true
                    } else if written().map({ $0.hasSuffix(wanted) }) != true {
                        // Not the name after an old one: a keyboard that shows what it is given its own way.
                        entered = false
                    }
                    if entered != true { Self.log.notice("open search field: \(written() ?? "nil", privacy: .public)") }
                    mark(entered == true ? "type" : entered == false ? "type unconfirmed" : "type again")
                    continue
                }
            }
            // A question of iOS's own holds the keyboard until it is answered.
            if Self.inFront(of: "").alert != nil { throw fail("search page under a system alert") }
        }
        guard entered != nil else {
            // The pull brought nothing. Leave the screen as it is, for the model to look at.
            if page() == nil, DSHActiveApps() == [home] {
                mark("no search page")
                throw NoSearchPage()
            }
            throw fail("search page")
        }
        var before: (label: String, frame: CGRect)?
        var seen = ""
        // The same icon in the same place twice: results move while they arrive.
        guard let icon = try await wait(.seconds(3), for: { () -> CGRect? in
            guard let found = page() else { return nil }
            let icons = Self.resultIcons(found.all, field: found.field.frame)
            seen = "field \(Int(found.field.frame.midY)) of \(Int(found.all.first?.frame.height ?? 0)), icons "
                + icons.map { "\($0.label)@\(Int($0.frame.midY))" }.joined(separator: " ")
            let now = PhoneCommand.icon(named: name, among: icons.map(\.label)).map { (icons[$0].label, icons[$0].frame) }
            defer { before = now }
            return now.flatMap { $0.0 == before?.label && $0.1 == before?.frame ? $0.1 : nil }
        }) else {
            Self.log.notice("open search: \(seen, privacy: .public)")
            // The results are on screen: leave them there for the model to look at and choose.
            if let owner = page()?.owner {
                mark("no icon")
                throw SearchPage(owner: owner)
            }
            throw fail("icon")
        }
        mark("icon")
        let shown = Set(DSHActiveApps() ?? [])
        func opened(within limit: Duration) async throws -> String? {
            try await wait(limit, for: { DSHActiveApps()?.first { $0 != home && !shown.contains($0) } })
        }
        guard DSHSendTap(CGPoint(x: icon.midX, y: icon.midY)) else { throw fail("tap") }
        var id = try await opened(within: .seconds(3))
        // A tap while the results were still settling can open nothing. Once more, where the icon is now.
        if id == nil, let found = page() {
            let icons = Self.resultIcons(found.all, field: found.field.frame)
            if let index = PhoneCommand.icon(named: name, among: icons.map(\.label)) {
                mark("tap again")
                let frame = icons[index].frame
                if DSHSendTap(CGPoint(x: frame.midX, y: frame.midY)) { id = try await opened(within: .seconds(6)) }
            }
        }
        guard let id else {
            if let owner = page()?.owner {
                mark("no app")
                throw SearchPage(owner: owner)
            }
            throw fail("app")
        }
        return id
    }

    /// The system search page is up with results for the name, and none could be chosen by name.
    private struct SearchPage: Error { let owner: String }
    /// The Home screen is in front and pulling it down brought no search page.
    private struct NoSearchPage: Error {}

    /// Processes that show the system search page or the Home screen. An app that comes to the
    /// front after a tap on the search page is none of these.
    private static let searchHosts: Set<String> = ["com.apple.Spotlight", "com.apple.campo", "com.apple.springboard"]

    /// App icons among the search results. Results sit on the far side of the field from the
    /// keyboard: above a field at the bottom (iOS 26), below a field floating at the top (iOS 27).
    static func resultIcons(_ all: [XCUIElementSnapshot], field: CGRect) -> [XCUIElementSnapshot] {
        let screen = all.first?.frame ?? .zero
        let fieldOnTop = screen.isEmpty ? false : field.midY < screen.midY
        return all.filter {
            $0.elementType == .icon && !$0.frame.isEmpty && !$0.frame.intersects(field)
                && (fieldOnTop ? $0.frame.minY >= field.maxY : $0.frame.maxY <= field.minY)
        }
    }

    /// For display names, accept only a unique exact match that LaunchServices exposes.
    private static func bundleID(for name: String) -> String? {
        if name.range(of: #"^[A-Za-z0-9_-]+(?:\.[A-Za-z0-9_-]+)+$"#, options: .regularExpression) != nil { return name }
        let sel = NSSelectorFromString("defaultWorkspace")
        guard let cls = NSClassFromString("LSApplicationWorkspace") as? NSObject.Type, cls.responds(to: sel),
              let workspace = cls.perform(sel)?.takeUnretainedValue() as? NSObject else { return nil }
        func get(_ obj: NSObject, _ property: String) -> AnyObject? {
            let selector = NSSelectorFromString(property)
            return obj.responds(to: selector) ? obj.perform(selector)?.takeUnretainedValue() : nil
        }
        let matches = (get(workspace, "allInstalledApplications") as? [NSObject] ?? []).compactMap { app -> String? in
            guard let display = get(app, "localizedName") as? String,
                  display.caseInsensitiveCompare(name) == .orderedSame else { return nil }
            return get(app, "bundleIdentifier") as? String
        }
        return matches.count == 1 ? matches[0] : nil
    }
}
