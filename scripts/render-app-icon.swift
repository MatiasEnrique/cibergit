#!/usr/bin/env swift
// Original cibergit artwork, MIT. Regenerate on macOS with AppKit and iconutil.
import AppKit

guard CommandLine.arguments.count == 2 else {
    fatalError("Usage: swift scripts/render-app-icon.swift NEW_OUTPUT_DIRECTORY")
}
let output = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let files = FileManager.default
guard !files.fileExists(atPath: output.path) else {
    fatalError("Output already exists; choose a new directory")
}
try files.createDirectory(at: output, withIntermediateDirectories: true)
let iconset = output.appendingPathComponent("cibergit.iconset", isDirectory: true)
try files.createDirectory(at: iconset, withIntermediateDirectories: false)

func color(_ red: CGFloat, _ green: CGFloat, _ blue: CGFloat, _ alpha: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: red, green: green, blue: blue, alpha: alpha)
}

func render(size: Int, destination: URL) throws {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let graphics = NSGraphicsContext(bitmapImageRep: bitmap)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = graphics
    let context = graphics.cgContext
    context.scaleBy(x: CGFloat(size) / 1024, y: CGFloat(size) / 1024)
    context.setAllowsAntialiasing(true)

    let tile = NSBezierPath(roundedRect: NSRect(x: 90, y: 90, width: 844, height: 844),
        xRadius: 190, yRadius: 190)
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = color(0.10, 0.16, 0.19, 0.25)
    shadow.shadowBlurRadius = 28
    shadow.shadowOffset = NSSize(width: 0, height: -12)
    shadow.set()
    color(0.90, 0.93, 0.94).setFill()
    tile.fill()
    NSGraphicsContext.restoreGraphicsState()
    NSGradient(starting: color(0.98, 0.99, 0.99), ending: color(0.81, 0.87, 0.89))!
        .draw(in: tile, angle: -90)
    color(1, 1, 1, 0.72).setStroke()
    tile.lineWidth = 3
    tile.stroke()

    // Two histories converge into a single reviewed path. The nodes stay clear
    // at Dock and Finder sizes; no type or borrowed product mark is involved.
    let ink = color(0.15, 0.24, 0.28)
    let accent = color(0.13, 0.43, 0.43)
    let main = NSBezierPath()
    main.move(to: NSPoint(x: 360, y: 284))
    main.line(to: NSPoint(x: 360, y: 744))
    main.lineWidth = 58
    main.lineCapStyle = .round
    ink.setStroke()
    main.stroke()
    let branch = NSBezierPath()
    branch.move(to: NSPoint(x: 664, y: 730))
    branch.line(to: NSPoint(x: 664, y: 638))
    branch.curve(to: NSPoint(x: 512, y: 487), controlPoint1: NSPoint(x: 664, y: 543),
        controlPoint2: NSPoint(x: 512, y: 568))
    branch.curve(to: NSPoint(x: 360, y: 368), controlPoint1: NSPoint(x: 512, y: 406),
        controlPoint2: NSPoint(x: 360, y: 425))
    branch.lineWidth = 58
    branch.lineCapStyle = .round
    accent.setStroke()
    branch.stroke()
    for (point, tint) in [(NSPoint(x: 360, y: 738), ink),
                          (NSPoint(x: 664, y: 738), accent),
                          (NSPoint(x: 360, y: 284), ink)] {
        tint.setFill()
        NSBezierPath(ovalIn: NSRect(x: point.x - 78, y: point.y - 78, width: 156, height: 156)).fill()
        color(0.95, 0.97, 0.97).setFill()
        NSBezierPath(ovalIn: NSRect(x: point.x - 32, y: point.y - 32, width: 64, height: 64)).fill()
    }
    NSGraphicsContext.restoreGraphicsState()
    try bitmap.representation(using: .png, properties: [:])!.write(to: destination, options: .withoutOverwriting)
}

for points in [16, 32, 128, 256, 512] {
    try render(size: points, destination: iconset.appendingPathComponent("icon_\(points)x\(points).png"))
    try render(size: points * 2, destination: iconset.appendingPathComponent("icon_\(points)x\(points)@2x.png"))
}
try render(size: 1024, destination: output.appendingPathComponent("cibergit.png"))
let task = Process()
task.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
task.arguments = ["--convert", "icns", "--output", output.appendingPathComponent("cibergit.icns").path, iconset.path]
try task.run()
task.waitUntilExit()
guard task.terminationStatus == 0 else { fatalError("iconutil failed") }
print(output.path)
