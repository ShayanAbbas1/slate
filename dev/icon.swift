// Generates Slate's app icon as a .iconset directory.
//
// The mark is an S built from three table rows, offset along the letter's
// diagonal -- the letter and the result grid are the same shape.
//
// It is code rather than a checked-in binary so it stays editable: the row
// weight, the stagger and the tile colour are numbers here, not pixels nobody
// can reopen.
//
// Usage: swift dev/icon.swift <out.iconset>

import AppKit
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

guard CommandLine.arguments.count == 2 else {
    FileHandle.standardError.write("usage: swift dev/icon.swift <out.iconset>\n".data(using: .utf8)!)
    exit(2)
}
let outDir = CommandLine.arguments[1]

// Apple's icon corner is a superellipse, not a circular round-rect. n=5 is the
// usual approximation and costs six lines.
func squircle(_ r: CGRect, n: CGFloat = 5) -> CGPath {
    let p = CGMutablePath()
    let (a, b, cx, cy) = (r.width / 2, r.height / 2, r.midX, r.midY)
    for i in 0...720 {
        let t = CGFloat(i) / 720 * 2 * .pi
        let (ct, st) = (cos(t), sin(t))
        let x = cx + a * pow(abs(ct), 2 / n) * (ct < 0 ? -1 : 1)
        let y = cy + b * pow(abs(st), 2 / n) * (st < 0 ? -1 : 1)
        i == 0 ? p.move(to: CGPoint(x: x, y: y)) : p.addLine(to: CGPoint(x: x, y: y))
    }
    p.closeSubpath()
    return p
}

func render(_ size: Int) -> CGImage {
    let s = CGFloat(size)
    let ctx = CGContext(
        data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
        space: CGColorSpace(name: CGColorSpace.sRGB)!,
        bitmapInfo: CGImageAlphaInfo.premultipliedFirst.rawValue)!

    // The classic macOS icon grid: the tile fills 824 of a 1024 canvas, the
    // rest is transparent margin the system draws spacing and shadow into.
    // ponytail: macOS 26 prefers an Icon Composer .icon bundle that the system
    // masks and shades itself; move to that if the flat tile starts looking
    // out of place beside system icons.
    let inset = s * 100 / 1024
    let tile = CGRect(x: inset, y: inset, width: s - inset * 2, height: s - inset * 2)
    ctx.addPath(squircle(tile))
    ctx.setFillColor(CGColor(red: 0.04, green: 0.04, blue: 0.05, alpha: 1))
    ctx.fillPath()

    let t = tile.width
    let h = t * 0.145, gap = t * 0.085, w = t * 0.50, off = t * 0.11
    let total = h * 3 + gap * 2
    ctx.setFillColor(CGColor(gray: 1, alpha: 1))
    for (i, dx) in [off, 0, -off].enumerated() {
        let rect = CGRect(x: tile.midX - w / 2 + dx,
                          y: tile.midY + total / 2 - h - CGFloat(i) * (h + gap),
                          width: w, height: h)
        ctx.addPath(CGPath(roundedRect: rect, cornerWidth: h / 2, cornerHeight: h / 2,
                           transform: nil))
        ctx.fillPath()
    }
    return ctx.makeImage()!
}

try? FileManager.default.createDirectory(atPath: outDir, withIntermediateDirectories: true)

for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        let dest = CGImageDestinationCreateWithURL(
            URL(fileURLWithPath: "\(outDir)/\(name)") as CFURL,
            UTType.png.identifier as CFString, 1, nil)!
        CGImageDestinationAddImage(dest, render(base * scale), nil)
        guard CGImageDestinationFinalize(dest) else {
            FileHandle.standardError.write("failed to write \(name)\n".data(using: .utf8)!)
            exit(1)
        }
    }
}
