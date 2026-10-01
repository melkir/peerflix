// Draws Peerflix's icon, a white play button on a red rounded square, into
// the iconset directory given, for iconutil to make the .icns of.
import AppKit

let dir = URL(fileURLWithPath: CommandLine.arguments[1])
try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

func draw(_ px: Int) -> Data {
    let s = CGFloat(px) / 1024
    let ctx = CGContext(
        data: nil, width: px, height: px, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.scaleBy(x: s, y: s)

    // macOS's icon grid: a 824 square with 185 corners, centred in 1024.
    let square = CGPath(
        roundedRect: CGRect(x: 100, y: 100, width: 824, height: 824),
        cornerWidth: 185, cornerHeight: 185, transform: nil)
    ctx.addPath(square)
    ctx.clip()
    let gradient = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.sRGB),
        colors: [
            CGColor(srgbRed: 0.93, green: 0.27, blue: 0.29, alpha: 1),
            CGColor(srgbRed: 0.62, green: 0.08, blue: 0.20, alpha: 1),
        ] as CFArray,
        locations: [0, 1])!
    ctx.drawLinearGradient(
        gradient, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])

    // The triangle, nudged right to look centred, its corners rounded.
    let (left, right, half): (CGFloat, CGFloat, CGFloat) = (402, 682, 170)
    let play = CGMutablePath()
    play.move(to: CGPoint(x: left, y: 512))
    play.addArc(tangent1End: CGPoint(x: left, y: 512 + half),
                tangent2End: CGPoint(x: right, y: 512), radius: 36)
    play.addArc(tangent1End: CGPoint(x: right, y: 512),
                tangent2End: CGPoint(x: left, y: 512 - half), radius: 36)
    play.addArc(tangent1End: CGPoint(x: left, y: 512 - half),
                tangent2End: CGPoint(x: left, y: 512 + half), radius: 36)
    play.closeSubpath()
    ctx.addPath(play)
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    ctx.fillPath()

    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    return rep.representation(using: .png, properties: [:])!
}

for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(size)x\(size).png" : "icon_\(size)x\(size)@2x.png"
        try draw(size * scale).write(to: dir.appendingPathComponent(name))
    }
}
