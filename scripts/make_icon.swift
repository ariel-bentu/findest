// Renders the Findest app icon: a friendly two-tone face (Finder-inspired,
// original artwork) with a large lightning bolt for speed.
//
//   swift scripts/make_icon.swift assets/AppIcon.png   # 1024x1024 PNG
//   (scripts/make_icon.sh turns it into assets/AppIcon.icns)
import AppKit
import CoreGraphics

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "AppIcon.png"
let space = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8, bytesPerRow: 0,
                    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

func linear(_ colors: [CGColor], from: CGPoint, to: CGPoint) {
    let g = CGGradient(colorsSpace: space, colors: colors as CFArray, locations: nil)!
    ctx.drawLinearGradient(g, start: from, end: to, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
}

// Full-bleed square artwork: macOS (26+) applies its own rounded mask to an
// opaque square icon; padded/transparent artwork instead gets shrunk onto a
// gray plate. Draw on the classic 824pt grid, scaled up to fill the canvas.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let bodyPath = CGPath(rect: body, transform: nil)
let k = size / body.width
ctx.scaleBy(x: k, y: k)
ctx.translateBy(x: -body.minX, y: -body.minY)

// Two-tone face: light left half, deep blue right half split by a soft S-curve.
ctx.saveGState()
ctx.addPath(bodyPath)
ctx.clip()
linear([rgb(0xbfe6ff), rgb(0x6cc0ff)], from: CGPoint(x: 0, y: 924), to: CGPoint(x: 0, y: 100))
let split = CGMutablePath()
split.move(to: CGPoint(x: 560, y: 940))
split.addCurve(to: CGPoint(x: 470, y: 470), control1: CGPoint(x: 470, y: 780), control2: CGPoint(x: 600, y: 600))
split.addCurve(to: CGPoint(x: 545, y: 80), control1: CGPoint(x: 360, y: 360), control2: CGPoint(x: 470, y: 200))
split.addLine(to: CGPoint(x: 940, y: 80))
split.addLine(to: CGPoint(x: 940, y: 940))
split.closeSubpath()
ctx.addPath(split)
ctx.clip()
linear([rgb(0x3d8bff), rgb(0x1652c9)], from: CGPoint(x: 0, y: 924), to: CGPoint(x: 0, y: 100))
ctx.restoreGState()

// Subtle top gloss.
ctx.saveGState()
ctx.addPath(bodyPath)
ctx.clip()
linear([rgb(0xffffff, 0.22), rgb(0xffffff, 0)], from: CGPoint(x: 0, y: 924), to: CGPoint(x: 0, y: 640))
ctx.restoreGState()

// Eyes and smile.
let ink = rgb(0x0e2a5c)
ctx.setFillColor(ink)
for x in [330.0, 560.0] {
    ctx.addPath(CGPath(roundedRect: CGRect(x: x, y: 600, width: 46, height: 150), cornerWidth: 23, cornerHeight: 23, transform: nil))
}
ctx.fillPath()
ctx.setStrokeColor(ink)
ctx.setLineWidth(34)
ctx.setLineCap(.round)
let smile = CGMutablePath()
smile.move(to: CGPoint(x: 280, y: 420))
smile.addCurve(to: CGPoint(x: 744, y: 420), control1: CGPoint(x: 400, y: 280), control2: CGPoint(x: 624, y: 280))
ctx.addPath(smile)
ctx.strokePath()

// The lightning bolt, large and diagonal, reaching down to the lower edge.
let bolt = CGMutablePath()
bolt.addLines(between: [
    CGPoint(x: 700, y: 760),
    CGPoint(x: 520, y: 420),
    CGPoint(x: 660, y: 420),
    CGPoint(x: 580, y: 118),
    CGPoint(x: 875, y: 500),
    CGPoint(x: 735, y: 500),
    CGPoint(x: 850, y: 760),
])
bolt.closeSubpath()

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 24, color: rgb(0x000000, 0.45))
ctx.addPath(bolt)
ctx.setFillColor(rgb(0xff9f0a))
ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(bolt)
ctx.clip()
linear([rgb(0xfff27a), rgb(0xffc21a), rgb(0xff8a00)], from: CGPoint(x: 0, y: 760), to: CGPoint(x: 0, y: 118))
ctx.restoreGState()

ctx.addPath(bolt)
ctx.setStrokeColor(rgb(0x8a3b00, 0.9))
ctx.setLineWidth(14)
ctx.setLineJoin(.round)
ctx.strokePath()

// Highlight along the bolt's upper edge.
let shine = CGMutablePath()
shine.move(to: CGPoint(x: 690, y: 730))
shine.addLine(to: CGPoint(x: 548, y: 446))
ctx.addPath(shine)
ctx.setStrokeColor(rgb(0xffffff, 0.75))
ctx.setLineWidth(12)
ctx.setLineCap(.round)
ctx.strokePath()

let image = ctx.makeImage()!
let rep = NSBitmapImageRep(cgImage: image)
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("wrote \(out)")
