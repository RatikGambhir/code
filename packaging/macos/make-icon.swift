// Draws Hangar's app icon: the 2×2 tile mark from `chrome::logo_mark` on a
// macOS-style rounded square, at every size an .iconset needs.
//
//   swift packaging/macos/make-icon.swift /tmp/Hangar.iconset
//   iconutil -c icns /tmp/Hangar.iconset -o packaging/macos/Hangar.icns
//
// Colors are black and white for now; change the constants below to recolor.

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

// Grayscale for now: a near-black body with white tiles.
let bodyTop = CGColor(gray: 0.13, alpha: 1)
let bodyBottom = CGColor(gray: 0.04, alpha: 1)
let bodyEdge = CGColor(gray: 1, alpha: 0.10)
let tileStrong = CGColor(gray: 1, alpha: 1)
let tileWeak = CGColor(gray: 1, alpha: 0.45)

/// Draws the icon into a `size`×`size` context, laid out on Apple's
/// 1024-point icon grid (an 824-point body with room for its shadow).
func drawIcon(_ ctx: CGContext, size: CGFloat) {
    let scale = size / 1024
    ctx.scaleBy(x: scale, y: scale)
    // Draw top-down, like the screen.
    ctx.translateBy(x: 0, y: 1024)
    ctx.scaleBy(x: 1, y: -1)

    let body = CGRect(x: 100, y: 100, width: 824, height: 824)
    let bodyPath = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

    // Soft shadow under the body.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: 10), blur: 28, color: CGColor(gray: 0, alpha: 0.35))
    ctx.addPath(bodyPath)
    ctx.setFillColor(bodyBottom)
    ctx.fillPath()
    ctx.restoreGState()

    // Body: a gentle top-to-bottom gradient.
    ctx.saveGState()
    ctx.addPath(bodyPath)
    ctx.clip()
    let gradient = CGGradient(
        colorsSpace: CGColorSpace(name: CGColorSpace.genericGrayGamma2_2),
        colors: [bodyTop, bodyBottom] as CFArray,
        locations: [0, 1]
    )!
    ctx.drawLinearGradient(gradient, start: CGPoint(x: 512, y: body.minY), end: CGPoint(x: 512, y: body.maxY), options: [])
    ctx.restoreGState()

    // A hairline edge so the icon reads on a dark Dock.
    ctx.addPath(CGPath(roundedRect: body.insetBy(dx: 1.5, dy: 1.5), cornerWidth: 184, cornerHeight: 184, transform: nil))
    ctx.setStrokeColor(bodyEdge)
    ctx.setLineWidth(3)
    ctx.strokePath()

    // The mark, with logo_mark's proportions: tiles 42% of the mark, the
    // rest the gap, corners 12%. Strong tiles on the diagonal.
    let mark: CGFloat = 440
    let tile = mark * 0.42
    let gap = mark - tile * 2
    let radius = mark * 0.12
    let origin = CGPoint(x: 512 - mark / 2, y: 512 - mark / 2)
    for row in 0..<2 {
        for column in 0..<2 {
            let rect = CGRect(
                x: origin.x + CGFloat(column) * (tile + gap),
                y: origin.y + CGFloat(row) * (tile + gap),
                width: tile,
                height: tile
            )
            ctx.addPath(CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil))
            ctx.setFillColor(row == column ? tileStrong : tileWeak)
            ctx.fillPath()
        }
    }
}

func writePNG(size: Int, to url: URL) throws {
    let space = CGColorSpace(name: CGColorSpace.sRGB)!
    guard let ctx = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
        space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { throw NSError(domain: "make-icon", code: 1) }
    drawIcon(ctx, size: CGFloat(size))
    guard let image = ctx.makeImage(),
          let dest = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil)
    else { throw NSError(domain: "make-icon", code: 2) }
    CGImageDestinationAddImage(dest, image, nil)
    guard CGImageDestinationFinalize(dest) else { throw NSError(domain: "make-icon", code: 3) }
}

let args = CommandLine.arguments
guard args.count == 2 else {
    FileHandle.standardError.write("usage: swift make-icon.swift <out.iconset>\n".data(using: .utf8)!)
    exit(2)
}
let out = URL(fileURLWithPath: args[1])
try FileManager.default.createDirectory(at: out, withIntermediateDirectories: true)
for points in [16, 32, 128, 256, 512] {
    try writePNG(size: points, to: out.appendingPathComponent("icon_\(points)x\(points).png"))
    try writePNG(size: points * 2, to: out.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
}
print("wrote \(out.path)")
