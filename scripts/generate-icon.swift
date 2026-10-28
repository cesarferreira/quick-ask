#!/usr/bin/env swift
import AppKit
import Foundation

// Draw in a 1024-point coordinate space so every icon size comes from one vector.
func spark(center: NSPoint, width: CGFloat, height: CGFloat) -> NSBezierPath {
    let path = NSBezierPath()
    let x = center.x
    let y = center.y
    path.move(to: NSPoint(x: x, y: y + height))
    path.curve(to: NSPoint(x: x + width, y: y),
               controlPoint1: NSPoint(x: x + width * 0.18, y: y + height * 0.18),
               controlPoint2: NSPoint(x: x + width * 0.18, y: y + height * 0.18))
    path.curve(to: NSPoint(x: x, y: y - height),
               controlPoint1: NSPoint(x: x + width * 0.18, y: y - height * 0.18),
               controlPoint2: NSPoint(x: x + width * 0.18, y: y - height * 0.18))
    path.curve(to: NSPoint(x: x - width, y: y),
               controlPoint1: NSPoint(x: x - width * 0.18, y: y - height * 0.18),
               controlPoint2: NSPoint(x: x - width * 0.18, y: y - height * 0.18))
    path.curve(to: NSPoint(x: x, y: y + height),
               controlPoint1: NSPoint(x: x - width * 0.18, y: y + height * 0.18),
               controlPoint2: NSPoint(x: x - width * 0.18, y: y + height * 0.18))
    path.close()
    return path
}

func render(size: Int) throws -> Data {
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
        isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: bitmap) else {
        throw NSError(domain: "QuickAskIcon", code: 1,
                      userInfo: [NSLocalizedDescriptionKey: "Could not create the icon canvas."])
    }
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    let transform = NSAffineTransform()
    transform.scale(by: CGFloat(size) / 1024)
    transform.concat()

    let tile = NSBezierPath(roundedRect: NSRect(x: 80, y: 80, width: 864, height: 864),
                            xRadius: 194, yRadius: 194)
    NSGradient(colors: [
        NSColor(srgbRed: 0.20, green: 0.15, blue: 0.29, alpha: 1),
        NSColor(srgbRed: 0.075, green: 0.057, blue: 0.11, alpha: 1),
    ])!.draw(in: tile, angle: -90)
    NSColor(srgbRed: 0.58, green: 0.46, blue: 0.77, alpha: 0.28).setStroke()
    tile.lineWidth = 3
    tile.stroke()

    NSGraphicsContext.saveGraphicsState()
    let glow = NSShadow()
    glow.shadowColor = NSColor(srgbRed: 0.64, green: 0.43, blue: 1, alpha: 0.28)
    glow.shadowBlurRadius = 44
    glow.shadowOffset = .zero
    glow.set()
    let mainSpark = spark(center: NSPoint(x: 487, y: 486), width: 254, height: 308)
    NSGradient(colors: [
        NSColor(srgbRed: 0.93, green: 0.88, blue: 1, alpha: 1),
        NSColor(srgbRed: 0.66, green: 0.48, blue: 0.96, alpha: 1),
    ])!.draw(in: mainSpark, angle: -90)
    NSGraphicsContext.restoreGraphicsState()

    NSColor(srgbRed: 0.80, green: 0.69, blue: 1, alpha: 1).setFill()
    spark(center: NSPoint(x: 739, y: 754), width: 60, height: 69).fill()

    guard let png = bitmap.representation(using: .png, properties: [:]) else {
        throw NSError(domain: "QuickAskIcon", code: 2,
                      userInfo: [NSLocalizedDescriptionKey: "Could not encode the icon."])
    }
    return png
}

let output = URL(fileURLWithPath: CommandLine.arguments.dropFirst().first ?? "scripts/icons",
                 isDirectory: true)
let files = FileManager.default
try files.createDirectory(at: output, withIntermediateDirectories: true)
let temporary = files.temporaryDirectory.appendingPathComponent("quick-ask-icon-\(UUID().uuidString)")
let iconset = temporary.appendingPathComponent("QuickAsk.iconset", isDirectory: true)
try files.createDirectory(at: iconset, withIntermediateDirectories: true)
defer { try? files.removeItem(at: temporary) }

for side in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let suffix = scale == 2 ? "@2x" : ""
        let filename = "icon_\(side)x\(side)\(suffix).png"
        try render(size: side * scale).write(to: iconset.appendingPathComponent(filename))
    }
}
try render(size: 1024).write(to: output.appendingPathComponent("QuickAsk.png"))
let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["--convert", "icns", "--output",
                      output.appendingPathComponent("QuickAsk.icns").path, iconset.path]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else {
    throw NSError(domain: "QuickAskIcon", code: Int(iconutil.terminationStatus),
                  userInfo: [NSLocalizedDescriptionKey: "iconutil could not create QuickAsk.icns."])
}
print("Created \(output.appendingPathComponent("QuickAsk.icns").path)")
