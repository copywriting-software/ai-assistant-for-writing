import AppKit
import ApplicationServices
import Foundation

// MARK: - Protocol Models

struct CursorRect: Codable {
    let x: Double
    let y: Double
    let height: Double
}

struct FocusSnapshot: Codable {
    let app_name: String?
    let window_title: String?
    let is_secure_field: Bool
    let text_before_cursor: String
    let text_after_cursor: String?
    let selected_text: String?
    let cursor_rect: CursorRect?
}

struct NativeCommand: Decodable {
    let cmd: String
    let text: String?
    let rect: CursorRect?
    let expected_prefix: String?
    let message: String?
}

// MARK: - Ghost Text Floating Panel

final class GhostPanel: NSPanel {
    private let label = NSTextField(labelWithString: "")

    init() {
        super.init(
            contentRect: NSRect(x: 0, y: 0, width: 320, height: 28),
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        self.level = .floating
        self.isOpaque = false
        self.backgroundColor = NSColor(calibratedWhite: 0.12, alpha: 0.88)
        self.hasShadow = true
        self.ignoresMouseEvents = true
        self.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary, .transient]

        let content = NSView(frame: NSRect(x: 0, y: 0, width: 320, height: 28))
        content.wantsLayer = true
        content.layer?.cornerRadius = 6
        content.layer?.borderWidth = 0.5
        content.layer?.borderColor = NSColor.white.withAlphaComponent(0.15).cgColor

        label.font = NSFont.systemFont(ofSize: 13)
        label.textColor = NSColor(calibratedWhite: 0.82, alpha: 0.95)
        label.lineBreakMode = .byTruncatingTail
        label.maximumNumberOfLines = 2
        label.translatesAutoresizingMaskIntoConstraints = false

        content.addSubview(label)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: content.leadingAnchor, constant: 8),
            label.trailingAnchor.constraint(equalTo: content.trailingAnchor, constant: -8),
            label.topAnchor.constraint(equalTo: content.topAnchor, constant: 4),
            label.bottomAnchor.constraint(equalTo: content.bottomAnchor, constant: -4),
            label.widthAnchor.constraint(lessThanOrEqualToConstant: 420),
        ])

        self.contentView = content
    }

    func showThinking(at quartzRect: CursorRect?) {
        label.stringValue = "✦ AI…"
        label.textColor = NSColor(calibratedWhite: 0.72, alpha: 0.85)
        positionAndShow(at: quartzRect)
    }

    func showGhost(text: String, at quartzRect: CursorRect?) {
        let displayText = "\(text)   [Tab ↹  Esc]"
        label.stringValue = displayText
        label.textColor = NSColor(calibratedWhite: 0.90, alpha: 0.98)
        positionAndShow(at: quartzRect)
    }

    private func positionAndShow(at quartzRect: CursorRect?) {
        self.contentView?.layoutSubtreeIfNeeded()
        let fitting = self.contentView?.fittingSize ?? NSSize(width: 240, height: 28)

        var origin = NSPoint(x: 200, y: 200)
        if let r = quartzRect, let primaryScreen = NSScreen.screens.first {
            let screenHeight = primaryScreen.frame.height
            let appKitY = screenHeight - r.y - fitting.height
            origin = NSPoint(x: r.x + 2, y: appKitY)
        } else {
            let mouse = NSEvent.mouseLocation
            origin = NSPoint(x: mouse.x + 12, y: mouse.y - fitting.height - 8)
        }

        self.setFrame(NSRect(origin: origin, size: fitting), display: true)
        self.orderFrontRegardless()
    }
}

// MARK: - Accessibility Helpers

final class AccessibilityBridge {
    static func ensureTrusted() -> Bool {
        let options = [kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String: true] as CFDictionary
        return AXIsProcessTrustedWithOptions(options)
    }

    static func focusedElement() -> AXUIElement? {
        let systemWide = AXUIElementCreateSystemWide()
        var focused: AnyObject?
        let sysErr = AXUIElementCopyAttributeValue(systemWide, kAXFocusedUIElementAttribute as CFString, &focused)
        if sysErr == .success, let el = focused {
            return (el as! AXUIElement)
        }

        // Fallback for Electron / Chromium / custom apps: query the frontmost app directly
        // and enable AXManualAccessibility so webviews/editors expose their focused element.
        guard let frontApp = NSWorkspace.shared.frontmostApplication else { return nil }
        let appEl = AXUIElementCreateApplication(frontApp.processIdentifier)
        AXUIElementSetAttributeValue(appEl, "AXManualAccessibility" as CFString, kCFBooleanTrue)
        AXUIElementSetAttributeValue(appEl, "AXEnhancedUserInterface" as CFString, kCFBooleanTrue)

        var appFocused: AnyObject?
        let appErr = AXUIElementCopyAttributeValue(appEl, kAXFocusedUIElementAttribute as CFString, &appFocused)
        if appErr == .success, let el = appFocused {
            return (el as! AXUIElement)
        }
        fputs("[MacHelper] focusedElement error: sys=\(sysErr.rawValue) app=\(appErr.rawValue) in \(frontApp.localizedName ?? "?")\n", stderr)
        return nil
    }

    static func focusedWindowTitle() -> String? {
        guard let frontApp = NSWorkspace.shared.frontmostApplication else { return nil }
        let appEl = AXUIElementCreateApplication(frontApp.processIdentifier)
        var winObj: AnyObject?
        guard AXUIElementCopyAttributeValue(appEl, kAXFocusedWindowAttribute as CFString, &winObj) == .success,
              let winEl = winObj else { return nil }
        var titleObj: AnyObject?
        if AXUIElementCopyAttributeValue(winEl as! AXUIElement, kAXTitleAttribute as CFString, &titleObj) == .success {
            return titleObj as? String
        }
        return nil
    }

    static func readSnapshot() -> FocusSnapshot? {
        let appName = NSWorkspace.shared.frontmostApplication?.localizedName
        guard let el = focusedElement() else {
            return nil
        }
        let windowTitle = focusedWindowTitle()

        var subroleObj: AnyObject?
        AXUIElementCopyAttributeValue(el, kAXSubroleAttribute as CFString, &subroleObj)
        let subrole = subroleObj as? String ?? ""
        let isSecure = subrole == (kAXSecureTextFieldSubrole as String)

        var valueObj: AnyObject?
        let valErr = AXUIElementCopyAttributeValue(el, kAXValueAttribute as CFString, &valueObj)
        let fullText = (valErr == .success ? (valueObj as? String) : nil) ?? ""

        var selectedObj: AnyObject?
        AXUIElementCopyAttributeValue(el, kAXSelectedTextAttribute as CFString, &selectedObj)
        let selectedText = selectedObj as? String

        var rangeObj: AnyObject?
        var textBefore = fullText
        var textAfter: String? = nil
        var cursorRect: CursorRect? = nil

        if AXUIElementCopyAttributeValue(el, kAXSelectedTextRangeAttribute as CFString, &rangeObj) == .success,
           let rangeVal = rangeObj {
            var cfRange = CFRange(location: 0, length: 0)
            if AXValueGetValue(rangeVal as! AXValue, .cfRange, &cfRange) {
                let utf16Count = fullText.utf16.count
                let loc = max(0, min(cfRange.location, utf16Count))
                let idx = String.Index(utf16Offset: loc, in: fullText)
                textBefore = String(fullText[..<idx].suffix(1000))

                let afterEndOffset = min(utf16Count, loc + max(0, cfRange.length))
                let afterIdx = String.Index(utf16Offset: afterEndOffset, in: fullText)
                let afterSlice = String(fullText[afterIdx...].prefix(200))
                if !afterSlice.isEmpty {
                    textAfter = afterSlice
                }

                var boundsRange = CFRange(location: max(0, loc - 1), length: loc > 0 ? 1 : 0)
                if let axRange = AXValueCreate(.cfRange, &boundsRange) {
                    var boundsObj: AnyObject?
                    if AXUIElementCopyParameterizedAttributeValue(
                        el,
                        kAXBoundsForRangeParameterizedAttribute as CFString,
                        axRange,
                        &boundsObj
                    ) == .success, let bVal = boundsObj {
                        var cgRect = CGRect.zero
                        if AXValueGetValue(bVal as! AXValue, .cgRect, &cgRect),
                           cgRect.height > 0 && cgRect.maxX > 0 {
                            cursorRect = CursorRect(
                                x: Double(cgRect.maxX),
                                y: Double(cgRect.minY),
                                height: Double(cgRect.height)
                            )
                        }
                    }
                }
            }
        }

        return FocusSnapshot(
            app_name: appName,
            window_title: windowTitle,
            is_secure_field: isSecure,
            text_before_cursor: textBefore,
            text_after_cursor: textAfter,
            selected_text: selectedText,
            cursor_rect: cursorRect
        )
    }

    static func insertText(_ text: String, expectedPrefix: String?) {
        if let expected = expectedPrefix, let current = readSnapshot() {
            if !expected.hasSuffix(current.text_before_cursor) && current.text_before_cursor != expected {
                fputs("[MacHelper] insertText aborted: prefix mismatch (curLen=\(current.text_before_cursor.count), expLen=\(expected.count))\n", stderr)
                return
            }
        }
        if let el = focusedElement() {
            var settable: DarwinBoolean = false
            let checkErr = AXUIElementIsAttributeSettable(el, kAXSelectedTextAttribute as CFString, &settable)
            let appName = NSWorkspace.shared.frontmostApplication?.localizedName ?? ""
            let isNativeApp = appName == "TextEdit" || appName == "Notes" || appName == "Mail"
            if isNativeApp && checkErr == .success && settable.boolValue {
                let err = AXUIElementSetAttributeValue(el, kAXSelectedTextAttribute as CFString, text as CFTypeRef)
                if err == .success {
                    fputs("[MacHelper] insertText via AXSelectedText ok\n", stderr)
                    return
                }
            }
        }
        fputs("[MacHelper] insertText via pasteViaClipboard (len=\(text.count))\n", stderr)
        pasteViaClipboard(text)
    }

    static func replaceSelection(_ text: String) {
        guard let el = focusedElement() else { return }
        var selectedObj: AnyObject?
        AXUIElementCopyAttributeValue(el, kAXSelectedTextAttribute as CFString, &selectedObj)
        let selected = (selectedObj as? String) ?? ""

        if !selected.isEmpty {
            let err = AXUIElementSetAttributeValue(el, kAXSelectedTextAttribute as CFString, text as CFTypeRef)
            if err == .success { return }
        } else {
            let err = AXUIElementSetAttributeValue(el, kAXValueAttribute as CFString, text as CFTypeRef)
            if err == .success { return }
        }
        pasteViaClipboard(text)
    }

    private static func pasteViaClipboard(_ text: String) {
        let pb = NSPasteboard.general
        let prev = pb.string(forType: .string)
        pb.clearContents()
        pb.setString(text, forType: .string)

        let src = CGEventSource(stateID: .hidSystemState)
        let vKey: CGKeyCode = 9 // 'v'
        if let down = CGEvent(keyboardEventSource: src, virtualKey: vKey, keyDown: true),
           let up = CGEvent(keyboardEventSource: src, virtualKey: vKey, keyDown: false) {
            down.flags = .maskCommand
            up.flags = .maskCommand
            down.post(tap: .cghidEventTap)
            up.post(tap: .cghidEventTap)
        }

        DispatchQueue.main.asyncAfter(deadline: .now() + 0.15) {
            if let prev = prev {
                pb.clearContents()
                pb.setString(prev, forType: .string)
            }
        }
    }
}

// MARK: - App Coordinator

final class Coordinator {
    let panel = GhostPanel()
    var statusItem: NSStatusItem?
    var statusMenuItem: NSMenuItem?
    var ghostVisible = false
    var isThinking = false
    var lastRect: CursorRect? = nil
    private var pendingRead: DispatchWorkItem?

    func setupMenuBar(trusted: Bool) {
        let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.variableLength)
        item.button?.title = trusted ? "✦ AI" : "✦ AI (!)"
        item.button?.toolTip = "AI Writing Assistant"

        let menu = NSMenu()
        let statusTitle = trusted
            ? "Status: Active (Accessibility OK)"
            : "Status: Grant Accessibility in System Settings"
        let statusRow = NSMenuItem(title: statusTitle, action: nil, keyEquivalent: "")
        statusRow.isEnabled = false
        menu.addItem(statusRow)
        self.statusMenuItem = statusRow

        if !trusted {
            let openSettings = NSMenuItem(
                title: "Open Accessibility Settings…",
                action: #selector(openAccessibilitySettings),
                keyEquivalent: ""
            )
            openSettings.target = self
            menu.addItem(openSettings)
        }

        menu.addItem(NSMenuItem.separator())
        let hints = [
            "Tab — Accept suggestion",
            "⌥ + → — Accept next word",
            "Esc — Dismiss",
            "⌘⇧G — Fix grammar / layout",
            "⌘⇧R — Rewrite concise",
        ]
        for h in hints {
            let row = NSMenuItem(title: h, action: nil, keyEquivalent: "")
            row.isEnabled = false
            menu.addItem(row)
        }

        menu.addItem(NSMenuItem.separator())
        let quitItem = NSMenuItem(title: "Quit AI Writing Assistant", action: #selector(quitApp), keyEquivalent: "q")
        quitItem.target = self
        menu.addItem(quitItem)

        item.menu = menu
        self.statusItem = item
    }

    @objc func openAccessibilitySettings() {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility") {
            NSWorkspace.shared.open(url)
        }
    }

    @objc func quitApp() {
        NSApp.terminate(nil)
    }

    func emit(_ dict: [String: Any]) {
        guard let data = try? JSONSerialization.data(withJSONObject: dict),
              let line = String(data: data, encoding: .utf8) else { return }
        print(line)
        fflush(stdout)
    }

    func emitEvent<T: Encodable>(name: String, payload: T) {
        let encoder = JSONEncoder()
        guard let data = try? encoder.encode(payload),
              var obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        obj["event"] = name
        emit(obj)
    }

    func scheduleSnapshotRead() {
        pendingRead?.cancel()
        let item = DispatchWorkItem { [weak self] in
            guard let self = self, let snap = AccessibilityBridge.readSnapshot() else { return }
            self.lastRect = snap.cursor_rect
            self.emitEvent(name: "text_changed", payload: snap)
        }
        pendingRead = item
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.015, execute: item)
    }

    func startStdinListener() {
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            while let line = readLine() {
                guard let data = line.data(using: .utf8),
                      let cmd = try? JSONDecoder().decode(NativeCommand.self, from: data) else { continue }
                DispatchQueue.main.async {
                    self?.handleCommand(cmd)
                }
            }
            DispatchQueue.main.async {
                NSApp.terminate(nil)
            }
        }
    }

    private func handleCommand(_ cmd: NativeCommand) {
        switch cmd.cmd {
        case "show_thinking":
            let rect = cmd.rect ?? lastRect
            isThinking = true
            ghostVisible = false
            panel.showThinking(at: rect)
        case "show_ghost":
            if let text = cmd.text, !text.isEmpty {
                let rect = cmd.rect ?? lastRect
                isThinking = false
                ghostVisible = true
                panel.showGhost(text: text, at: rect)
            }
        case "hide_ghost":
            isThinking = false
            ghostVisible = false
            panel.orderOut(nil)
        case "insert_text":
            isThinking = false
            ghostVisible = false
            panel.orderOut(nil)
            if let text = cmd.text {
                AccessibilityBridge.insertText(text, expectedPrefix: cmd.expected_prefix)
            }
        case "replace_selection":
            isThinking = false
            ghostVisible = false
            panel.orderOut(nil)
            if let text = cmd.text {
                AccessibilityBridge.replaceSelection(text)
            }
        case "notify_error":
            fputs("AI Assistant error: \(cmd.message ?? "unknown")\n", stderr)
        default:
            break
        }
    }
}

let coordinator = Coordinator()

func eventTapCallback(
    proxy: CGEventTapProxy,
    type: CGEventType,
    event: CGEvent,
    refcon: UnsafeMutableRawPointer?
) -> Unmanaged<CGEvent>? {
    guard type == .keyDown else {
        return Unmanaged.passUnretained(event)
    }

    let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
    let flags = event.flags

    // Cmd+Shift+G (keyCode 5) -> Fix grammar / layout
    if flags.contains(.maskCommand) && flags.contains(.maskShift) && keyCode == 5 {
        DispatchQueue.main.async {
            if let snap = AccessibilityBridge.readSnapshot() {
                coordinator.emitEvent(name: "fix_grammar", payload: snap)
            }
        }
        return nil
    }

    // Cmd+Shift+R (keyCode 15) -> Rewrite concise
    if flags.contains(.maskCommand) && flags.contains(.maskShift) && keyCode == 15 {
        DispatchQueue.main.async {
            if let snap = AccessibilityBridge.readSnapshot() {
                var obj: [String: Any] = [
                    "event": "rewrite",
                    "style": "concise",
                    "app_name": snap.app_name as Any,
                    "is_secure_field": snap.is_secure_field,
                    "text_before_cursor": snap.text_before_cursor,
                    "selected_text": snap.selected_text as Any,
                ]
                if snap.app_name == nil { obj.removeValue(forKey: "app_name") }
                if snap.selected_text == nil { obj.removeValue(forKey: "selected_text") }
                coordinator.emit(obj)
            }
        }
        return nil
    }

    if coordinator.ghostVisible || coordinator.isThinking {
        // Esc (53) -> Dismiss ghost or thinking pill
        if keyCode == 53 {
            DispatchQueue.main.async {
                coordinator.isThinking = false
                coordinator.ghostVisible = false
                coordinator.panel.orderOut(nil)
                coordinator.emit(["event": "dismiss"])
            }
            return nil
        }
    }

    if coordinator.ghostVisible {
        // Tab (48) without modifiers -> Accept full ghost text
        if keyCode == 48 && !flags.contains(.maskCommand) && !flags.contains(.maskControl) && !flags.contains(.maskAlternate) {
            fputs("[MacHelper] Tab pressed -> accept_all\n", stderr)
            DispatchQueue.main.async {
                coordinator.emit(["event": "accept_all"])
            }
            return nil
        }

        // Option + Right Arrow (124) -> Accept next word
        if keyCode == 124 && flags.contains(.maskAlternate) {
            DispatchQueue.main.async {
                coordinator.emit(["event": "accept_word"])
            }
            return nil
        }
    }

    // Ignore shortcut combos and navigation/non-text keys for autocomplete
    let nonTextKeyCodes: Set<Int64> = [
        48,  // Tab
        53,  // Esc
        115, // Home
        116, // Page Up
        119, // End
        121, // Page Down
        123, 124, 125, 126, // Arrow keys
    ]
    if !flags.contains(.maskCommand) && !flags.contains(.maskControl) && !nonTextKeyCodes.contains(keyCode) {
        DispatchQueue.main.async {
            coordinator.scheduleSnapshotRead()
        }
    }

    return Unmanaged.passUnretained(event)
}

// MARK: - Main

let app = NSApplication.shared
app.setActivationPolicy(.accessory)

let isTrusted = AccessibilityBridge.ensureTrusted()
let listenAccess = CGPreflightListenEventAccess()
fputs("[MacHelper] startup: axTrusted=\(isTrusted) listenAccess=\(listenAccess)\n", stderr)
if !listenAccess {
    CGRequestListenEventAccess()
}

coordinator.setupMenuBar(trusted: isTrusted)

let eventMask = (1 << CGEventType.keyDown.rawValue)
guard let tap = CGEvent.tapCreate(
    tap: .cgSessionEventTap,
    place: .headInsertEventTap,
    options: .defaultTap,
    eventsOfInterest: CGEventMask(eventMask),
    callback: eventTapCallback,
    userInfo: nil
) else {
    fputs("Failed to create CGEventTap. Grant Accessibility & Input Monitoring permissions to your terminal/app.\n", stderr)
    exit(1)
}

let runLoopSource = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
CFRunLoopAddSource(CFRunLoopGetCurrent(), runLoopSource, .commonModes)
CGEvent.tapEnable(tap: tap, enable: true)

coordinator.startStdinListener()
app.run()
