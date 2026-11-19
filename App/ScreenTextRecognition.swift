import CoreGraphics
import Vision

enum ScreenTextRecognition {
    static func recognize(_ image: CGImage, cpuOnly: Bool = false) -> [VNRecognizedTextObservation] {
        let request = VNRecognizeTextRequest()
        request.recognitionLevel = .accurate
        #if os(macOS)
        // Revision 3 can stall on macOS. Revision 2 still recognizes both screen languages.
        if VNRecognizeTextRequest.supportedRevisions.contains(VNRecognizeTextRequestRevision2) {
            request.revision = VNRecognizeTextRequestRevision2
        }
        #endif
        request.recognitionLanguages = ["zh-Hans", "en-US"]
        request.usesLanguageCorrection = false
        if cpuOnly {
            for (stage, devices) in (try? request.supportedComputeStageDevices) ?? [:] {
                if let cpu = devices.first(where: { if case .cpu = $0 { return true }; return false }) {
                    request.setComputeDevice(cpu, for: stage)
                }
            }
        }
        try? VNImageRequestHandler(cgImage: image).perform([request])
        return request.results ?? []
    }
}
