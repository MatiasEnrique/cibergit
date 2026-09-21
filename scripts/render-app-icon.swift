#!/usr/bin/env swift
// cibergit's mark, MIT. Regenerate on macOS with AppKit and iconutil.
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

let grid: CGFloat = 1024
let markCenter = NSPoint(x: 512, y: 512)
let markHeight: CGFloat = 600

enum PathToken {
    case command(Character)
    case number(CGFloat)
}

func tokenize(_ definition: String) -> [PathToken] {
    var tokens: [PathToken] = []
    var digits = ""
    func flush() {
        guard !digits.isEmpty else { return }
        guard let value = Double(digits) else { fatalError("Unreadable number \(digits)") }
        tokens.append(.number(CGFloat(value)))
        digits = ""
    }
    for character in definition {
        switch character {
        case "0"..."9", ".":
            digits.append(character)
        case "-", "+":
            // A sign continues the number only as an exponent's sign.
            if digits.hasSuffix("e") || digits.hasSuffix("E") {
                digits.append(character)
            } else {
                flush()
                digits.append(character)
            }
        case "e", "E":
            if digits.isEmpty { fatalError("Unsupported path command \(character)") }
            digits.append(character)
        case " ", ",", "\n", "\r", "\t":
            flush()
        default:
            flush()
            tokens.append(.command(character))
        }
    }
    flush()
    return tokens
}

// The mark uses absolute moves, lines and cubics only. Anything else is refused
// rather than approximated, so editing the SVG cannot silently lose detail.
func bezierPath(from definition: String) -> NSBezierPath {
    let path = NSBezierPath()
    var tokens = tokenize(definition)[...]
    func number() -> CGFloat {
        guard case .number(let value)? = tokens.popFirst() else { fatalError("Expected a coordinate") }
        return value
    }
    func point() -> NSPoint { NSPoint(x: number(), y: number()) }
    var command: Character = " "
    while let token = tokens.first {
        if case .command(let letter) = token {
            command = letter
            tokens.removeFirst()
        }
        switch command {
        case "M":
            path.move(to: point())
            command = "L"    // repeated coordinates after a move are lines
        case "L":
            path.line(to: point())
        case "C":
            let first = point(), second = point(), end = point()
            path.curve(to: end, controlPoint1: first, controlPoint2: second)
        case "Z", "z":
            path.close()
        default:
            fatalError("Unsupported path command \(command)")
        }
    }
    return path
}

func markPath() throws -> NSBezierPath {
    let source = URL(fileURLWithPath: #filePath)
        .deletingLastPathComponent().deletingLastPathComponent()
        .appendingPathComponent("assets/icons/cibergit.svg")
    let document = try String(contentsOf: source, encoding: .utf8)
    let expression = try NSRegularExpression(pattern: "\\bd=\"([^\"]+)\"")
    let matches = expression.matches(in: document, range: NSRange(document.startIndex..., in: document))
    guard !matches.isEmpty else { fatalError("No path data in \(source.lastPathComponent)") }
    let mark = NSBezierPath()
    for match in matches {
        mark.append(bezierPath(from: String(document[Range(match.range(at: 1), in: document)!])))
    }
    // SVG measures y downwards from the top, so fitting the mark also flips it.
    let bounds = mark.cgPath.boundingBoxOfPath
    let scale = markHeight / bounds.height
    let transform = NSAffineTransform()
    transform.translateX(by: markCenter.x, yBy: markCenter.y)
    transform.scaleX(by: scale, yBy: -scale)
    transform.translateX(by: -bounds.midX, yBy: -bounds.midY)
    mark.transform(using: transform as AffineTransform)
    return mark
}

let mark = try markPath()

func render(size: Int, destination: URL) throws {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    let graphics = NSGraphicsContext(bitmapImageRep: bitmap)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = graphics
    let context = graphics.cgContext
    context.scaleBy(x: CGFloat(size) / grid, y: CGFloat(size) / grid)
    context.setAllowsAntialiasing(true)

    let tile = NSBezierPath(roundedRect: NSRect(x: 90, y: 90, width: 844, height: 844),
        xRadius: 190, yRadius: 190)
    // The tile is white, so the only thing separating it from a light Finder
    // list or Dock is its shadow and the hairline below. A gradient would read
    // as a tint at this size and there is nothing here to tint.
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = color(0.10, 0.16, 0.19, 0.25)
    shadow.shadowBlurRadius = 28
    shadow.shadowOffset = NSSize(width: 0, height: -12)
    shadow.set()
    color(1, 1, 1).setFill()
    tile.fill()
    NSGraphicsContext.restoreGraphicsState()
    // A white tile on white has no edge of its own. This hairline is what keeps
    // the rounded square a shape rather than a hole the mark floats in.
    color(0.82, 0.84, 0.86).setStroke()
    tile.lineWidth = 3
    tile.stroke()

    color(0.05, 0.06, 0.07).setFill()
    mark.fill()

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
