// render-icon.swift — composes the brand icon master into the macOS app-icon
// shape: a 1024x1024 transparent canvas with the artwork clipped to the
// centred 824x824 rounded square (corner radius 185) that every macOS icon
// uses, so the Dock and Finder show it the way they show any other app.
//
// Usage: swift scripts/render-icon.swift <master.png> <out-1024.png>
// Called by scripts/make-icon.sh.

import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let args = CommandLine.arguments
guard args.count == 3 else {
    FileHandle.standardError.write("usage: render-icon.swift <master.png> <out.png>\n".data(using: .utf8)!)
    exit(2)
}
let masterURL = URL(fileURLWithPath: args[1])
let outURL = URL(fileURLWithPath: args[2])

guard let src = CGImageSourceCreateWithURL(masterURL as CFURL, nil),
      let master = CGImageSourceCreateImageAtIndex(src, 0, nil) else {
    FileHandle.standardError.write("cannot read \(masterURL.path)\n".data(using: .utf8)!)
    exit(1)
}

let canvas = 1024
let body: CGFloat = 824
let radius: CGFloat = 185
let inset = (CGFloat(canvas) - body) / 2

let space = CGColorSpace(name: CGColorSpace.sRGB)!
guard let ctx = CGContext(
    data: nil, width: canvas, height: canvas, bitsPerComponent: 8, bytesPerRow: 0,
    space: space, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else { exit(1) }

ctx.interpolationQuality = .high
let rect = CGRect(x: inset, y: inset, width: body, height: body)
ctx.addPath(CGPath(roundedRect: rect, cornerWidth: radius, cornerHeight: radius, transform: nil))
ctx.clip()
ctx.draw(master, in: rect)

guard let out = ctx.makeImage(),
      let dest = CGImageDestinationCreateWithURL(outURL as CFURL, UTType.png.identifier as CFString, 1, nil) else {
    exit(1)
}
CGImageDestinationAddImage(dest, out, nil)
guard CGImageDestinationFinalize(dest) else { exit(1) }
