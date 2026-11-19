import Foundation
import CoreGraphics
import ImageIO
import Vision
import UniformTypeIdentifiers

struct PhoneCommand: Codable {
    let action: String
    var app: String?
    var x: Int?
    var y: Int?
    var x2: Int?
    var y2: Int?
    var text: String?
    var screen: String?
    var picture: Bool
    var navigation: Bool?
    var target: PhoneTarget?
    /// The title and subtitle of Pallas Agent's own Live Activity. The runner shrinks it when it reads them at the top.
    var overlay: [String]?

    var writes: Bool { ["tap", "swipe", "type"].contains(action) }
    /// Seconds a `type` may take on top of an ordinary command. Keys go out one after another, so
    /// a page of text takes minutes where a tap takes one or two seconds. The slowest runner in
    /// use types 7.8 characters a second (wode16, 1050 characters in 134 s); this allows for half
    /// that, and `iphone-screen.cjs` allows the same.
    var typingSeconds: Int { action == "type" ? ((text?.count ?? 0) + 3) / 4 : 0 }
    /// A touch that starts or ends where the expanded Live Activity covers the target's top bar.
    var reachesTop: Bool {
        guard ["tap", "swipe"].contains(action) else { return false }
        let ends = [target?.y ?? y, action == "swipe" ? y2 : nil]
        return ends.contains { ($0 ?? 1000) <= PhoneFrame.topBand }
    }

    /// A name as it is compared: letters and digits only, in lower case.
    static func bare(_ text: String) -> String { String(text.lowercased().filter { $0.isLetter || $0.isNumber }) }

    /// Which of the app icons a search shows is the app asked for by `name`: the one named exactly
    /// so, else the only one whose name contains it ("携程" for 携程旅行). Case, spaces and
    /// punctuation do not count. nil when none fits or apps with different names do. A name longer
    /// than the icon's is not accepted: it is another app's more often than this one's.
    static func icon(named name: String, among labels: [String]) -> Int? {
        let wanted = bare(name), names = labels.map(bare)
        guard !wanted.isEmpty else { return nil }
        if let exact = names.firstIndex(of: wanted) { return exact }
        let close = names.indices.filter { wanted.count >= 2 && names[$0].contains(wanted) }
        return Set(close.map { names[$0] }).count == 1 ? close[0] : nil
    }
}

struct PhoneObservation: Codable {
    let screen: String
    let text: String
    let jpeg: Data?
    var page: String?
    var controls: [PhoneTarget]?
    /// Set when `open` by name could not tell which result to tap: the screen is the system
    /// search page for this name, and one tap on an app icon there opens the app.
    var opening: String?
    /// Set when `open` by name pulled the Home screen down and no search page came: the screen
    /// is the Home screen as it is, to be looked at only. The next command must be an `open`.
    var searchMissing: String?
}

/// Coordinates belong to this observation only. The runner checks the same
/// labeled element again immediately before using it.
struct PhoneTarget: Codable, Equatable {
    let label: String
    let role: String
    let x: Int
    let y: Int
}

extension PhoneTarget {
    /// Where a control with this frame is touched, in the same points as the frame: its centre,
    /// unless a bar floating over the bottom of the page (a search field, a tab bar, a toolbar)
    /// lies over the centre. Such a bar takes the touch, and a little above its own edge too:
    /// on Settings' first page the row 通用 ends under the search bar, a touch on its centre,
    /// 7 points above the bar, did nothing, and one 20 points above opened it (wode16, iOS 27).
    /// The touch then goes to the middle of what shows above the bar. nil when the centre is off
    /// screen or too little shows: the page has to be scrolled first.
    static func touchPoint(of frame: CGRect, in bounds: CGRect, under bars: [CGRect]) -> CGPoint? {
        var point = CGPoint(x: frame.midX, y: frame.midY)
        guard bounds.contains(point) else { return nil }
        // A control that lies inside a bar is the bar's own: the microphone in a search field.
        for bar in bars where !bar.insetBy(dx: -4, dy: -4).contains(frame) {
            let clear = bar.minY - 10
            guard point.y > clear, point.y < bar.maxY + 10, point.x > bar.minX - 10, point.x < bar.maxX + 10 else { continue }
            let top = max(frame.minY, bounds.minY)
            guard clear - top >= 12 else { return nil }
            point.y = (top + clear) / 2
        }
        return point
    }

    /// The bar that an element of a bar stands for: the element's frame, or the frame of the glass
    /// around it. `above` are the frames of the elements that contain it, outermost first.
    static func barFrame(of frame: CGRect, above: [CGRect]) -> CGRect {
        var bar = frame
        for outer in above.reversed() {
            guard outer.height <= max(frame.height * 2, 56), outer.contains(bar) else { break }
            bar = outer
        }
        return bar
    }
}

/// Normalized coordinates share one top-left origin in screenshots, OCR and HID.
struct PhoneFrame {
    let image: CGImage
    let signature: [UInt8]
    /// How the screen is turned, as a UIInterfaceOrientation raw value (1 portrait, 2 upside down,
    /// 3 landscape right, 4 landscape left). XCTest keeps the screenshot's pixels in portrait and
    /// says which way it is turned in its EXIF orientation; `image` is already turned upright.
    let orientation: Int
    private static let side = 64

    init?(_ data: Data) {
        let options: [CFString: Any] = [kCGImageSourceCreateThumbnailFromImageAlways: true,
                                        kCGImageSourceCreateThumbnailWithTransform: true,
                                        kCGImageSourceThumbnailMaxPixelSize: 1280]
        guard let src = CGImageSourceCreateWithData(data as CFData, nil),
              let image = CGImageSourceCreateThumbnailAtIndex(src, 0, options as CFDictionary) else { return nil }
        let properties = CGImageSourceCopyPropertiesAtIndex(src, 0, nil) as? [CFString: Any]
        orientation = Self.orientation(exif: properties?[kCGImagePropertyOrientation] as? Int ?? 1)
        self.image = image
        let n = Self.side
        var pixels = [UInt8](repeating: 0, count: n*n)
        guard pixels.withUnsafeMutableBytes({ bytes in
            guard let ctx = CGContext(data: bytes.baseAddress, width: n, height: n, bitsPerComponent: 8,
                                      bytesPerRow: n, space: CGColorSpaceCreateDeviceGray(), bitmapInfo: 0) else { return false }
            ctx.draw(image, in: CGRect(x: 0, y: 0, width: n, height: n))
            return true
        }) else { return nil }
        signature = pixels
    }

    /// The interface orientation a screenshot's EXIF orientation stands for (measured on iPad, iOS 27).
    static func orientation(exif: Int) -> Int {
        switch exif {
        case 8: 3
        case 6: 4
        case 3: 2
        default: 1
        }
    }

    /// What differs between two signatures, in cells of the 64 by 64 grid: how many, how many of
    /// them beside the touch, and the rows and columns they span.
    struct Change: Equatable {
        var cells = 0, near = 0, top = 0, bottom = 0, left = 0, right = 0
        /// Cells that differ, by the finer measure, on the strip of screen the finger lands on: the
        /// full width, two rows above and below a tap or the start of a swipe. nil for anything else.
        var strip: Int?
        /// A screen that barely moved, and not at all where the finger lands.
        var slight: Bool { cells <= 60 && near <= 3 }
        /// A tap or swipe also goes ahead when the screen moved elsewhere and its own strip did not:
        /// a page still loading further down, a banner, a carousel. Typing has no place of its own.
        var permits: Bool { slight || strip == 0 }
        /// For the runner's log: numbers only.
        var summary: String {
            (cells == 0 ? "0" : "\(cells) near \(near) rows \(top)-\(bottom) cols \(left)-\(right)") + (strip.map { " strip \($0)" } ?? "")
        }
    }

    static func change(from previous: [UInt8], to current: [UInt8], at command: PhoneCommand) -> Change? {
        guard previous.count == current.count, current.count == side * side else { return nil }
        let n = side
        // The clock's rows are not compared, so a touch on them has no strip to vouch for it.
        let finger = ["tap", "swipe"].contains(command.action) ? (command.target?.y ?? command.y).map { $0 * (n-1) / 1000 } : nil
        var change = Change(strip: finger.flatMap { $0 + 2 >= 3 ? 0 : nil })
        for i in 0..<current.count {
            let delta = abs(Int(current[i]) - Int(previous[i])), row = i / n, column = i % n
            // 12 grey levels tell one label from another of the same length; 28 often do not.
            guard delta > 12, row >= 3 else { continue } // rows 0-2: clock and radio indicators
            // The last two columns hold the scroll indicator, which fades for a second after a swipe.
            if let finger, abs(row - finger) <= 2, column < n - 2 { change.strip? += 1 }
            guard delta > 28 else { continue }
            if change.cells == 0 { (change.top, change.left, change.right) = (row, column, column) }
            change.cells += 1
            (change.bottom, change.left, change.right) = (row, min(change.left, column), max(change.right, column))
            if let x = command.x, let y = command.y,
               abs(column - x * (n-1) / 1000) <= 2, abs(row - y * (n-1) / 1000) <= 2 { change.near += 1 }
        }
        return change
    }

    func matches(_ previous: [UInt8], at command: PhoneCommand) -> Bool {
        Self.change(from: previous, to: signature, at: command)?.slight ?? false
    }

    /// The share of the screen, in thousandths from the top, that the expanded Live Activity can cover.
    static let topBand = 140

    /// Recognized lines as "(x,y) text" with centers in 0...1000 from the top left.
    /// `band` reads only that many thousandths from the top.
    private func recognize(band: Int = 1000) -> [String] {
        let rows = image.height * band / 1000
        let region = band < 1000 ? image.cropping(to: CGRect(x: 0, y: 0, width: image.width, height: rows)) ?? image : image
        let share = Double(region.height) / Double(image.height)
        return ScreenTextRecognition.recognize(region, cpuOnly: true).prefix(100).compactMap { item -> String? in
            guard let text = item.topCandidates(1).first?.string else { return nil }
            let b = item.boundingBox
            return "(\(Int(b.midX*1000)),\(Int((1-b.midY)*share*1000))) \(text.prefix(200))"
        }
    }

    /// The text of the top band alone, for a quick look at what lies over the target's top bar.
    func topText() -> String { recognize(band: Self.topBand).joined(separator: "\n") }

    /// The y (0...1000) of Pallas Agent's expanded Live Activity, when `text` shows its title or subtitle
    /// in the top band. iOS draws it over the target's status and navigation bars.
    static func overlayLine(in text: String, overlay: [String]) -> Int? {
        func plain(_ value: String) -> String { value.lowercased().filter { !$0.isWhitespace } }
        let wanted = overlay.map(plain).filter { !$0.isEmpty }
        guard !wanted.isEmpty else { return nil }
        return text.split(separator: "\n").compactMap { line -> Int? in
            guard line.first == "(", let end = line.firstIndex(of: ")"), let comma = line[..<end].firstIndex(of: ","),
                  let y = Int(line[line.index(after: comma)..<end]), y <= topBand else { return nil }
            let label = plain(String(line[line.index(after: end)...]))
            return wanted.contains { label.contains($0) } ? y : nil
        }.min()
    }

    func observation(picture: Bool) throws -> PhoneObservation {
        let lines = recognize()
        var jpeg: Data?
        if picture {
            let data = NSMutableData()
            guard let dest = CGImageDestinationCreateWithData(data, UTType.jpeg.identifier as CFString, 1, nil) else { throw CocoaError(.coderInvalidValue) }
            CGImageDestinationAddImage(dest, image, [kCGImageDestinationLossyCompressionQuality: 0.70] as CFDictionary)
            guard CGImageDestinationFinalize(dest) else { throw CocoaError(.coderInvalidValue) }
            jpeg = data as Data
        }
        return PhoneObservation(screen: UUID().uuidString, text: lines.joined(separator: "\n"), jpeg: jpeg)
    }
}
