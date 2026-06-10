// Generates the app icon masters in assets/icon/ from code, so the icon can
// be regenerated at any time. Run from anywhere:
//
//   swift apps/silo_app/tool/generate_icon.swift
//
// Outputs (all 1024x1024 PNG):
//   assets/icon/icon.png          rounded-square icon (transparent corners)
//   assets/icon/icon_maskable.png full-bleed background, mark inside the
//                                 central 60% safe zone (Android adaptive
//                                 foreground, web maskable icons)
//   assets/icon/icon_macos.png    macOS style: ~10% transparent margin,
//                                 corner radius ~22.5% of the inner size
//
// The mark is a flat grain-silo silhouette (cylinder with a domed cap and two
// seam lines) in off-white on a deep-teal vertical gradient. The teal family
// follows the app's ColorScheme seed, 0xFF356859 (lib/main.dart).

import CoreGraphics
import Foundation
import ImageIO

let canvas: CGFloat = 1024

func srgb(_ hex: UInt32) -> CGColor {
  CGColor(
    colorSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
    components: [
      CGFloat((hex >> 16) & 0xFF) / 255,
      CGFloat((hex >> 8) & 0xFF) / 255,
      CGFloat(hex & 0xFF) / 255,
      1,
    ])!
}

// Gradient endpoints sit in the hue family of the seed color 0xFF356859:
// lighter teal at the top, deeper teal at the bottom.
let gradientTop = srgb(0x44806B)
let gradientBottom = srgb(0x1E423A)
let offWhite = srgb(0xF4F1E8)

func makeContext() -> CGContext {
  let context = CGContext(
    data: nil,
    width: Int(canvas),
    height: Int(canvas),
    bitsPerComponent: 8,
    bytesPerRow: 0,
    space: CGColorSpace(name: CGColorSpace.sRGB)!,
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
  context.interpolationQuality = .high
  return context
}

// Fills `shape` with the vertical teal gradient. `top` and `bottom` are in
// top-down y coordinates (CoreGraphics contexts are bottom-up, so they are
// flipped here).
func fillGradient(_ context: CGContext, shape: CGPath, top: CGFloat, bottom: CGFloat) {
  context.saveGState()
  context.addPath(shape)
  context.clip()
  let gradient = CGGradient(
    colorsSpace: CGColorSpace(name: CGColorSpace.sRGB)!,
    colors: [gradientTop, gradientBottom] as CFArray,
    locations: [0, 1])!
  context.drawLinearGradient(
    gradient,
    start: CGPoint(x: canvas / 2, y: canvas - top),
    end: CGPoint(x: canvas / 2, y: canvas - bottom),
    options: [])
  context.restoreGState()
}

// Builds the silo silhouette around the canvas center, scaled by `scale`.
// Two seam rectangles are appended so an even-odd fill leaves gaps that show
// the background through the body. All coordinates below are top-down at
// scale 1 and get flipped to the context's bottom-up space.
func siloPath(scale: CGFloat) -> CGPath {
  let center = canvas / 2

  func point(_ x: CGFloat, _ y: CGFloat) -> CGPoint {
    CGPoint(x: center + (x - center) * scale, y: canvas - (center + (y - center) * scale))
  }

  let bodyLeft: CGFloat = 320
  let bodyRight: CGFloat = 704
  let bodyTop: CGFloat = 392
  let bodyBottom: CGFloat = 784
  let domeHeight: CGFloat = 152
  let footRadius: CGFloat = 20 * scale

  let path = CGMutablePath()

  // Body with a domed cap. The dome is the top half of an ellipse spanning
  // the body width.
  path.move(to: point(bodyLeft, bodyTop))
  path.addCurve(
    to: point(center, bodyTop - domeHeight),
    control1: point(bodyLeft, bodyTop - domeHeight * 0.92),
    control2: point(center - (bodyRight - bodyLeft) * 0.28, bodyTop - domeHeight))
  path.addCurve(
    to: point(bodyRight, bodyTop),
    control1: point(center + (bodyRight - bodyLeft) * 0.28, bodyTop - domeHeight),
    control2: point(bodyRight, bodyTop - domeHeight * 0.92))
  path.addLine(to: point(bodyRight, bodyBottom - footRadius / scale))
  path.addArc(
    tangent1End: point(bodyRight, bodyBottom),
    tangent2End: point(bodyRight - footRadius / scale, bodyBottom),
    radius: footRadius)
  path.addArc(
    tangent1End: point(bodyLeft, bodyBottom),
    tangent2End: point(bodyLeft, bodyBottom - footRadius / scale),
    radius: footRadius)
  path.closeSubpath()

  // Seam lines: 16px-tall gaps across the body (even-odd fill turns these
  // into holes). The first sits at the dome/body junction so the domed cap
  // reads as a cap.
  for seamY: CGFloat in [396, 590] {
    let topLeft = point(bodyLeft, seamY)
    let bottomRight = point(bodyRight, seamY + 16)
    path.addRect(
      CGRect(
        x: topLeft.x, y: bottomRight.y,
        width: bottomRight.x - topLeft.x, height: topLeft.y - bottomRight.y))
  }

  return path
}

func drawMark(_ context: CGContext, scale: CGFloat) {
  context.setFillColor(offWhite)
  context.addPath(siloPath(scale: scale))
  context.fillPath(using: .evenOdd)
}

func writePNG(_ context: CGContext, to url: URL) {
  let image = context.makeImage()!
  let destination = CGImageDestinationCreateWithURL(
    url as CFURL, "public.png" as CFString, 1, nil)!
  CGImageDestinationAddImage(destination, image, nil)
  guard CGImageDestinationFinalize(destination) else {
    fatalError("failed to write \(url.path)")
  }
  print("wrote \(url.path)")
}

let toolDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent()
let iconDir =
  toolDir
  .deletingLastPathComponent()
  .appendingPathComponent("assets")
  .appendingPathComponent("icon")
try FileManager.default.createDirectory(at: iconDir, withIntermediateDirectories: true)

// icon.png: rounded square (corner radius ~17.6%, inside the iOS mask radius
// so flattened corners never show through), mark at full size.
do {
  let context = makeContext()
  let rounded = CGPath(
    roundedRect: CGRect(x: 0, y: 0, width: canvas, height: canvas),
    cornerWidth: 180, cornerHeight: 180, transform: nil)
  fillGradient(context, shape: rounded, top: 0, bottom: canvas)
  drawMark(context, scale: 1)
  writePNG(context, to: iconDir.appendingPathComponent("icon.png"))
}

// icon_maskable.png: full-bleed background; the mark is scaled so its whole
// bounding box stays inside the central 60% safe zone (a 614px circle).
do {
  let context = makeContext()
  let square = CGPath(
    rect: CGRect(x: 0, y: 0, width: canvas, height: canvas), transform: nil)
  fillGradient(context, shape: square, top: 0, bottom: canvas)
  drawMark(context, scale: 0.9)
  writePNG(context, to: iconDir.appendingPathComponent("icon_maskable.png"))
}

// icon_macos.png: Apple's macOS convention; a rounded rect with a transparent
// margin (inner size 824, ~10% margin per side) and corner radius ~22.5% of
// the inner size.
do {
  let context = makeContext()
  let margin: CGFloat = 100
  let inner = canvas - margin * 2
  let rounded = CGPath(
    roundedRect: CGRect(x: margin, y: margin, width: inner, height: inner),
    cornerWidth: inner * 0.225, cornerHeight: inner * 0.225, transform: nil)
  fillGradient(context, shape: rounded, top: margin, bottom: canvas - margin)
  drawMark(context, scale: inner / canvas)
  writePNG(context, to: iconDir.appendingPathComponent("icon_macos.png"))
}
