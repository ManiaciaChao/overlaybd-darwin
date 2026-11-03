import AppKit
import SwiftUI

struct GraphScrollCommand {
    enum Action {
        case restore(GraphViewport), fitWidth, fitAll, center(CGRect), zoom(CGFloat)
    }
    let id = UUID()
    let action: Action
}

// One native two-axis viewport owns scrolling and zoom. The SwiftUI document
// contains real controls; only the connecting paths are drawn into a Canvas.
struct GraphViewportView<Content: View>: NSViewRepresentable {
    let size: CGSize
    let command: GraphScrollCommand?
    let content: Content
    let onChange: (GraphViewport) -> Void
    init(size: CGSize, command: GraphScrollCommand?, onChange: @escaping (GraphViewport) -> Void,
         @ViewBuilder content: () -> Content) {
        self.size = size; self.command = command; self.onChange = onChange; self.content = content()
    }
    func makeCoordinator() -> Coordinator { Coordinator(onChange: onChange) }
    func makeNSView(context: Context) -> NSScrollView {
        let scroll = NSScrollView()
        scroll.hasVerticalScroller = true
        scroll.hasHorizontalScroller = true
        scroll.autohidesScrollers = true
        scroll.drawsBackground = false
        scroll.borderType = .noBorder
        scroll.allowsMagnification = true
        scroll.minMagnification = 0.15
        scroll.maxMagnification = 2.5
        let host = NSHostingView(rootView: content)
        host.isFlipped = true
        host.sizingOptions = []
        host.frame = NSRect(origin: .zero, size: size)
        scroll.documentView = host
        context.coordinator.host = host
        context.coordinator.scroll = scroll
        context.coordinator.observe()
        return scroll
    }
    func updateNSView(_ scroll: NSScrollView, context: Context) {
        let coordinator = context.coordinator
        coordinator.onChange = onChange
        coordinator.host?.rootView = content
        if coordinator.host?.frame.size != size {
            coordinator.host?.setFrameSize(size)
        }
        if let command, coordinator.lastCommand != command.id {
            coordinator.lastCommand = command.id
            // Let the hosting document and clip view lay out at their new size
            // before restoring an origin or fitting a subtree.
            DispatchQueue.main.async { [weak coordinator] in
                guard let coordinator, coordinator.lastCommand == command.id else { return }
                coordinator.apply(command.action)
            }
        }
        coordinator.publish()
    }
    static func dismantleNSView(_ scroll: NSScrollView, coordinator: Coordinator) {
        coordinator.observers.forEach(NotificationCenter.default.removeObserver)
        coordinator.observers = []
    }
    final class Coordinator {
        var host: NSHostingView<Content>?
        weak var scroll: NSScrollView?
        var onChange: (GraphViewport) -> Void
        var lastCommand: UUID?
        var lastViewport: GraphViewport?
        var observers: [NSObjectProtocol] = []
        init(onChange: @escaping (GraphViewport) -> Void) { self.onChange = onChange }
        func observe() {
            guard let scroll else { return }
            scroll.contentView.postsBoundsChangedNotifications = true
            for (name, object) in [(NSView.boundsDidChangeNotification, scroll.contentView as AnyObject),
                                   (NSScrollView.didEndLiveMagnifyNotification, scroll as AnyObject)] {
                observers.append(NotificationCenter.default.addObserver(forName: name, object: object, queue: .main) {
                    [weak self] _ in self?.publish()
                })
            }
        }
        func publish() {
            guard let scroll else { return }
            let rect = scroll.documentVisibleRect
            let state = GraphViewport(origin: rect.origin, scale: scroll.magnification, visibleSize: rect.size)
            guard rect.width > 0, rect.height > 0, state != lastViewport else { return }
            lastViewport = state
            DispatchQueue.main.async { [weak self] in
                guard let self, self.lastViewport == state else { return }
                self.onChange(state)
            }
        }
        func apply(_ action: GraphScrollCommand.Action) {
            guard let scroll, let host else { return }
            switch action {
            case .restore(let state):
                scroll.magnification = state.scale.clamped(to: scroll.minMagnification...scroll.maxMagnification)
                scroll.contentView.scroll(to: state.origin)
            case .fitWidth:
                let scale = min(1, scroll.contentSize.width / max(1, host.frame.width))
                scroll.magnification = scale.clamped(to: scroll.minMagnification...scroll.maxMagnification)
                scroll.contentView.scroll(to: .zero)
            case .fitAll:
                scroll.magnify(toFit: host.bounds)
            case .center(let rect):
                let visible = scroll.documentVisibleRect
                scroll.contentView.scroll(to: CGPoint(x: max(0, rect.midX - visible.width / 2),
                                                     y: max(0, rect.midY - visible.height / 2)))
            case .zoom(let factor):
                let rect = scroll.documentVisibleRect
                scroll.setMagnification((scroll.magnification * factor).clamped(to: scroll.minMagnification...scroll.maxMagnification),
                                        centeredAt: CGPoint(x: rect.midX, y: rect.midY))
            }
            scroll.reflectScrolledClipView(scroll.contentView)
            publish()
        }
    }
}
private extension CGFloat {
    func clamped(to range: ClosedRange<CGFloat>) -> CGFloat { Swift.min(range.upperBound, Swift.max(range.lowerBound, self)) }
}
