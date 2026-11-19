import CryptoKit
import Darwin
import Foundation
import notify

enum RunnerFailure: String, Error {
    case unavailable, notInstalled, groupUnavailable, protocolError, disconnected, timedOut
    case stopped, openFailed, screenChanged, wrongOrientation, invalidAction, inputFailed, captureFailed
    case foregroundLost, deviceLocked
    /// The runner is installed, and iOS would not open it.
    case launchRefused
    /// The touch would have landed on Pallas Agent's own Live Activity. No input was sent.
    case covered
    /// On the search page left after an open by name, only one tap on an app icon is taken.
    case notIcon
}

struct RunnerReply: Codable {
    var observation: PhoneObservation?
    var error: String?
}

/// The OS restricts this Unix socket to the two signed members of the App Group.
/// A connection belongs to one chat turn. Closing it revokes all following input.
///
/// A host from TestFlight is signed by another team than the runner the person signs themself, and
/// two teams cannot share an App Group. Such a runner listens on the loopback interface instead,
/// which any app on the phone can reach, so both ends first prove they hold the key of `Loopback`.
final class ScreenSocket: @unchecked Sendable {
    static let suffix = ".screen.xctrunner"
    /// The host's bundle ID, which the installer writes into a runner's Info.plist when another
    /// team signed that host. Such a runner serves over the loopback interface.
    static let foreignHost = Bundle.main.object(forInfoDictionaryKey: "DSHScreenHost") as? String
    static var hostID: String {
        if let foreignHost { return foreignHost }
        let id = Bundle.main.bundleIdentifier ?? ""
        return id.hasSuffix(suffix) ? String(id.dropLast(suffix.count)) : id
    }
    /// Every host a runner signed apart from them serves, each with the key it holds. Installers
    /// from 0.5.4 write `DSHScreenHosts` ([{host, key}]), so a TestFlight and a test build on one
    /// device can both use it; older ones wrote the single `DSHScreenHost` / `DSHScreenKey`.
    static var foreignLinks: [(host: String, link: Loopback)] {
        var links: [(host: String, link: Loopback)] = []
        for item in Bundle.main.object(forInfoDictionaryKey: "DSHScreenHosts") as? [[String: String]] ?? [] {
            guard let host = item["host"], !host.isEmpty, let link = Loopback(hex: item["key"]),
                  !links.contains(where: { $0.host == host }) else { continue }
            links.append((host, link))
        }
        if links.isEmpty, let foreignHost,
           let link = Loopback(hex: Bundle.main.object(forInfoDictionaryKey: "DSHScreenKey") as? String) {
            links.append((foreignHost, link))
        }
        return links
    }
    /// Darwin notification the runner posts once it listens. It reaches the host through a Mach
    /// port, so iOS does not hold it back the way it defers a backgrounded host's timers.
    static var readyName: String { hostID + ".screen.ready" }
    private let fd: Int32
    /// The engine gives a gesture 55 seconds. Reading a long list of a busy app can take most of that.
    init(_ fd: Int32, readTimeout: Int = 50) throws {
        guard fd >= 0 else { throw RunnerFailure.unavailable }
        self.fd = fd
        var one: Int32 = 1
        _ = setsockopt(fd, SOL_SOCKET, SO_NOSIGPIPE, &one, socklen_t(MemoryLayout.size(ofValue: one)))
        _ = fcntl(fd, F_SETFD, FD_CLOEXEC)
        var tv = timeval(tv_sec: readTimeout, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, socklen_t(MemoryLayout.size(ofValue: tv)))
        tv.tv_sec = 40
        _ = setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, socklen_t(MemoryLayout.size(ofValue: tv)))
    }
    deinit { Darwin.close(fd) }
    func stop() { _ = shutdown(fd, SHUT_RDWR) }

    private static func address() throws -> sockaddr_un {
        guard let dir = FileManager.default.containerURL(forSecurityApplicationGroupIdentifier: "group." + hostID + ".screen") else {
            throw RunnerFailure.groupUnavailable
        }
        let path = dir.appendingPathComponent("s").path
        var addr = sockaddr_un()
        guard path.utf8CString.count <= MemoryLayout.size(ofValue: addr.sun_path) else { throw RunnerFailure.groupUnavailable }
        addr.sun_family = sa_family_t(AF_UNIX)
        addr.sun_len = UInt8(MemoryLayout<sockaddr_un>.size)
        withUnsafeMutableBytes(of: &addr.sun_path) { $0.copyBytes(from: path.utf8CString.map { UInt8(bitPattern: $0) }) }
        return addr
    }

    /// The secret the installer gives both sides of a pair signed by two teams: the runner in its
    /// Info.plist, the host as a file in its documents. Neither side sends it. Each proves it holds
    /// it, so an app that answers on the port, or calls it, learns nothing and gets nowhere.
    struct Loopback {
        private let key: SymmetricKey

        init?(hex: String?) {
            guard let hex = hex?.trimmingCharacters(in: .whitespacesAndNewlines), hex.utf8.count == 64 else { return nil }
            var bytes: [UInt8] = []
            var rest = Substring(hex)
            while !rest.isEmpty {
                guard let byte = UInt8(rest.prefix(2), radix: 16) else { return nil }
                bytes.append(byte)
                rest = rest.dropFirst(2)
            }
            key = SymmetricKey(data: bytes)
        }

        /// The runner picks a free port for each turn and leaves it under this name. Any app can
        /// read or overwrite the number, which is why the port alone admits nobody.
        private static func register(_ host: String = ScreenSocket.hostID) -> Int32? {
            var token = NOTIFY_TOKEN_INVALID
            return notify_register_check(host + ".screen.port", &token) == NOTIFY_STATUS_OK ? token : nil
        }
        static var published: UInt16 {
            guard let token = register() else { return 0 }
            defer { notify_cancel(token) }
            var state: UInt64 = 0
            notify_get_state(token, &state)
            return UInt16(exactly: state) ?? 0
        }
        /// The system forgets the number once nobody is registered for the name, so the runner
        /// stays registered for as long as it listens.
        final class Beacon {
            private let token: Int32?
            init(port: UInt16, host: String = ScreenSocket.hostID) {
                token = Loopback.register(host)
                if let token { notify_set_state(token, UInt64(port)) }
            }
            deinit {
                guard let token else { return }
                notify_set_state(token, 0)
                notify_cancel(token)
            }
        }

        fileprivate struct Hello: Codable {
            var nonce = Data((0..<32).map { _ in UInt8.random(in: .min ... .max) })
            var proof: Data?
        }
        fileprivate func proof(_ role: String, _ host: Hello, _ runner: Hello) -> Data {
            Data(HMAC<SHA256>.authenticationCode(for: Data(role.utf8) + host.nonce + runner.nonce, using: key))
        }
        fileprivate func proves(_ role: String, _ host: Hello, _ runner: Hello, _ proof: Data?) -> Bool {
            guard host.nonce.count == 32, runner.nonce.count == 32, let proof else { return false }
            return HMAC<SHA256>.isValidAuthenticationCode(proof, authenticating: Data(role.utf8) + host.nonce + runner.nonce, using: key)
        }
    }

    /// How long `receive` waits from now on.
    func readTimeout(_ seconds: Int) {
        var wait = timeval(tv_sec: seconds, tv_usec: 0)
        _ = setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &wait, socklen_t(MemoryLayout.size(ofValue: wait)))
    }

    /// A socket on the loopback interface: connected to `port`, or listening on a free one.
    private static func loopback(port: UInt16) throws -> ScreenSocket {
        let socket = try ScreenSocket(Darwin.socket(AF_INET, SOCK_STREAM, 0))
        var addr = sockaddr_in()
        addr.sin_len = UInt8(MemoryLayout<sockaddr_in>.size)
        addr.sin_family = sa_family_t(AF_INET)
        addr.sin_port = port.bigEndian
        addr.sin_addr.s_addr = INADDR_LOOPBACK.bigEndian
        let status = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                port == 0 ? Darwin.bind(socket.fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
                          : Darwin.connect(socket.fd, $0, socklen_t(MemoryLayout<sockaddr_in>.size))
            }
        }
        guard status == 0, port != 0 || Darwin.listen(socket.fd, 2) == 0 else { throw RunnerFailure.unavailable }
        return socket
    }

    private var port: UInt16 {
        var addr = sockaddr_in()
        var size = socklen_t(MemoryLayout<sockaddr_in>.size)
        let status = withUnsafeMutablePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) { getsockname(fd, $0, &size) }
        }
        return status == 0 ? UInt16(bigEndian: addr.sin_port) : 0
    }

    /// The host's side of a turn with a runner of another team. No runner listening yet, or
    /// something else on the port, reads as `.unavailable`.
    static func connect(_ link: Loopback) throws -> ScreenSocket {
        let port = Loopback.published
        guard port != 0 else { throw RunnerFailure.unavailable }
        let socket = try loopback(port: port)
        socket.readTimeout(3)
        let mine = Loopback.Hello()
        guard (try? socket.send(mine)) != nil, let theirs = try? socket.receive(Loopback.Hello.self),
              link.proves("runner", mine, theirs, theirs.proof),
              (try? socket.send(Loopback.Hello(proof: link.proof("host", mine, theirs)))) != nil else {
            throw RunnerFailure.unavailable
        }
        socket.readTimeout(50)
        return socket
    }

    /// The runner's side: whether the caller holds the key too.
    private func admits(_ link: Loopback) -> Bool {
        readTimeout(5)
        let mine = Loopback.Hello()
        guard let theirs = try? receive(Loopback.Hello.self),
              (try? send(Loopback.Hello(nonce: mine.nonce, proof: link.proof("runner", theirs, mine)))) != nil,
              let answer = try? receive(Loopback.Hello.self), link.proves("host", theirs, mine, answer.proof) else { return false }
        readTimeout(180)
        return true
    }

    static func endpoint(listen: Bool) throws -> ScreenSocket {
        let socket = try ScreenSocket(Darwin.socket(AF_UNIX, SOCK_STREAM, 0))
        var addr = try address()
        if listen {
            _ = withUnsafePointer(to: &addr.sun_path) { p in p.withMemoryRebound(to: CChar.self, capacity: 1) { unlink($0) } }
        }
        let status = withUnsafePointer(to: &addr) { p in
            p.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                listen ? Darwin.bind(socket.fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                       : Darwin.connect(socket.fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard status == 0, !listen || Darwin.listen(socket.fd, 2) == 0 else { throw RunnerFailure.unavailable }
        return socket
    }

    // One XCTest invocation serves one turn. Close the listener as soon as its
    // caller connects, so a later turn cannot queue on a runner that is exiting.
    // Wait longer than the host's 24 s connect deadline: once XCTest presses Home,
    // iOS moves the host to the Background role and may defer its retries by seconds.
    // With `link` the listener is a loopback port, and a caller that cannot prove it holds the key
    // is dropped without ending the wait for the host.
    static func acceptClient(_ link: Loopback? = nil) throws -> ScreenSocket? {
        let listener = try link == nil ? endpoint(listen: true) : loopback(port: 0)
        var beacon: Loopback.Beacon?
        if link != nil {
            guard listener.port != 0 else { throw RunnerFailure.unavailable }
            beacon = Loopback.Beacon(port: listener.port)
        }
        defer { beacon = nil }
        notify_post(readyName)
        var p = pollfd(fd: listener.fd, events: Int16(POLLIN), revents: 0)
        let deadline = ContinuousClock.now + .seconds(30)
        while true {
            let left = ContinuousClock.now.duration(to: deadline) / .milliseconds(1)
            guard left > 0 else { return nil }
            let result = poll(&p, 1, Int32(left))
            if result < 0, errno == EINTR { continue }
            if result == 0 { return nil }
            guard result > 0 else { throw RunnerFailure.timedOut }
            // Between actions the model may be reading a screen over cellular.
            let client = try ScreenSocket(Darwin.accept(listener.fd, nil, nil), readTimeout: 180)
            if let link, !client.admits(link) { continue }
            return client
        }
    }

    /// A runner serving several hosts: one loopback port per host, each published under that
    /// host's name and each admitting only its key, so every host connects the way it always has.
    /// Answers the first caller that proves its key, and which host it is.
    static func acceptClient(links: [(host: String, link: Loopback)]) throws -> (client: ScreenSocket, host: String)? {
        guard !links.isEmpty else { throw RunnerFailure.unavailable }
        let listeners = try links.map { _ in try loopback(port: 0) }
        guard listeners.allSatisfy({ $0.port != 0 }) else { throw RunnerFailure.unavailable }
        // The ports stay published while this waits; the beacons clear them when it returns.
        let beacons = zip(listeners, links).map { Loopback.Beacon(port: $0.port, host: $1.host) }
        defer { withExtendedLifetime(beacons) {} }
        for link in links { notify_post(link.host + ".screen.ready") }
        var fds = listeners.map { pollfd(fd: $0.fd, events: Int16(POLLIN), revents: 0) }
        let deadline = ContinuousClock.now + .seconds(30)
        while true {
            let left = ContinuousClock.now.duration(to: deadline) / .milliseconds(1)
            guard left > 0 else { return nil }
            let result = poll(&fds, nfds_t(fds.count), Int32(left))
            if result < 0, errno == EINTR { continue }
            if result == 0 { return nil }
            guard result > 0 else { throw RunnerFailure.timedOut }
            for index in fds.indices where fds[index].revents & Int16(POLLIN) != 0 {
                fds[index].revents = 0
                let client = try ScreenSocket(Darwin.accept(listeners[index].fd, nil, nil), readTimeout: 180)
                if client.admits(links[index].link) { return (client, links[index].host) }
            }
        }
    }

    var connected: Bool {
        var byte: UInt8 = 0
        let n = recv(fd, &byte, 1, MSG_PEEK | MSG_DONTWAIT)
        return n > 0 || (n < 0 && (errno == EAGAIN || errno == EINTR))
    }

    func send<T: Encodable>(_ value: T) throws {
        let data = try JSONEncoder().encode(value)
        guard data.count <= 4_000_000 else { throw RunnerFailure.protocolError }
        var length = UInt32(data.count).bigEndian
        try withUnsafeBytes(of: &length) { try write($0) }
        try data.withUnsafeBytes { try write($0) }
    }

    private func write(_ bytes: UnsafeRawBufferPointer) throws {
        var sent = 0
        while sent < bytes.count {
            let n = Darwin.write(fd, bytes.baseAddress!.advanced(by: sent), bytes.count - sent)
            if n < 0, errno == EINTR { continue }
            guard n > 0 else { throw RunnerFailure.disconnected }
            sent += n
        }
    }

    private func read(count: Int) throws -> Data {
        var data = Data(count: count)
        try data.withUnsafeMutableBytes { (p: UnsafeMutableRawBufferPointer) in
            var offset = 0
            while offset < count {
                let n = Darwin.read(fd, p.baseAddress!.advanced(by: offset), count - offset)
                if n < 0, errno == EINTR { continue }
                if n < 0, errno == EAGAIN { throw RunnerFailure.timedOut }
                guard n > 0 else { throw RunnerFailure.disconnected }
                offset += n
            }
        }
        return data
    }

    func receive<T: Decodable>(_ type: T.Type) throws -> T {
        let header = try read(count: 4)
        let size = Int(header.withUnsafeBytes { $0.loadUnaligned(as: UInt32.self).bigEndian })
        guard (1...4_000_000).contains(size) else { throw RunnerFailure.protocolError }
        return try JSONDecoder().decode(type, from: read(count: size))
    }
}
